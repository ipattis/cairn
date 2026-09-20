//! Daemon state: the one process that owns the vault, the rollout server, and the runs.
//!
//! The cockpit never holds any of this. It relays through the localhost API, so closing
//! the app cannot stop a run and the webview never sees a credential.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::Mutex;

use wikiskill_core::config::Config;
use wikiskill_core::eval::TaskSet;
use wikiskill_core::git::Repo;
use wikiskill_core::iteration::{AgentFileSink, Control, LoopDeps, Runner};
use wikiskill_core::jev::JevClient;
use wikiskill_core::model;
use wikiskill_core::sandbox::{self, Isolation, ProfileRequest};
use wikiskill_core::state::{RunState, RunStatus, Store};
use wikiskill_core::vault::Vault;
use wikiskill_core::verifier::Verifier;

use crate::secrets;
use crate::supervisor::Supervisor;

pub struct App {
    pub config: Config,
    pub config_path: PathBuf,
    pub store: Store,
    pub vault: Vault,
    pub repo: Repo,
    pub supervisor: Arc<Supervisor>,
    pub token: String,
    /// Pause/cancel handles for runs this process started.
    controls: Mutex<HashMap<String, Arc<Control>>>,
}

impl App {
    /// Loads config, reconciles orphaned runs, and prepares the rollout server — but does
    /// not start it: `status` and the API should work with no executor installed.
    pub async fn load(config_path: &Path) -> Result<Arc<Self>> {
        let config = Config::load(config_path).await?;
        let store = Store::new(config.daemon.state_dir.clone());
        let token = match &config.daemon.token {
            Some(token) if !token.is_empty() => token.clone(),
            _ => store.ensure_token().await?,
        };

        let reconciled = store.reconcile_orphans().await?;
        if !reconciled.is_empty() {
            tracing::warn!(
                "marked {} run(s) failed: they were still `queued` or `running` when the daemon \
                 last stopped ({})",
                reconciled.len(),
                reconciled.join(", ")
            );
        }

        let vault = Vault::open(&config.vault)?;
        let repo = Repo::open(&config.vault);
        if !repo.is_repo().await {
            anyhow::bail!(
                "the vault at {} is not a git repository; run `wikiskilld init` first",
                config.vault.display()
            );
        }

        let isolation: Arc<dyn Isolation> = Arc::from(sandbox::for_host(
            &config.sandbox,
            config.daemon.state_dir.join("profiles"),
        )?);
        crate::supervisor::check_isolation(&config, isolation.as_ref())?;
        let supervisor = Arc::new(Supervisor::new(config.clone(), isolation)?);

        Ok(Arc::new(Self {
            config,
            config_path: config_path.to_path_buf(),
            store,
            vault,
            repo,
            supervisor,
            token,
            controls: Mutex::new(HashMap::new()),
        }))
    }

    /// Builds the loop's dependencies for one run. Model clients are built per run so a
    /// config edit takes effect on the next run rather than needing a daemon restart.
    pub async fn loop_deps(&self, run_id: &str) -> Result<Arc<LoopDeps>> {
        let missing = secrets::missing_for(
            self.config.jev.enabled,
            // Mantle uses a Keychain key; Bedrock runtime uses the AWS credential chain.
            matches!(
                self.config.models.maintainer.endpoint,
                wikiskill_core::config::Endpoint::BedrockMantle
            ) || matches!(
                self.config.models.proposer.endpoint,
                wikiskill_core::config::Endpoint::BedrockMantle
            ),
        );
        if !missing.is_empty() {
            anyhow::bail!(
                "missing credentials: {}. Add them to the Keychain (see docs/setup.md) \
                 and restart the daemon.",
                missing.join(", ")
            );
        }

        let verifier_isolation = sandbox::for_host(
            &self.config.sandbox,
            self.config.daemon.state_dir.join("profiles"),
        )?;
        let verifier = Verifier::new(
            self.config.verifier.clone(),
            verifier_isolation,
            vec![self.config.vault.clone()],
        );

        let jev_key = std::env::var("JEV_API_KEY").unwrap_or_default();

        Ok(Arc::new(LoopDeps {
            config: self.config.clone(),
            vault: self.vault.clone(),
            repo: self.repo.clone(),
            store: self.store.clone(),
            executor: self.supervisor.client(),
            verifier: Arc::new(verifier),
            maintainer_client: Arc::from(
                model::client_for(&self.config.models.maintainer).await?,
            ),
            proposer_client: Arc::from(model::client_for(&self.config.models.proposer).await?),
            jev: Arc::new(JevClient::new(self.config.jev.clone(), jev_key)?),
            agent_files: Arc::clone(&self.supervisor) as Arc<dyn AgentFileSink>,
            work_root: self.config.daemon.state_dir.join("work").join(run_id),
        }))
    }

    /// `load` already validates; the status endpoint calls this on every poll, so nothing here
    /// may log — see [`TaskSet::size_notes`].
    pub async fn task_set(&self) -> Result<TaskSet> {
        TaskSet::load(&self.vault.area_root(wikiskill_core::vault::Area::Eval)).await
    }

    /// Starts a run in the background and returns its id. The run outlives any client.
    pub async fn start_run(self: &Arc<Self>, model_override: Option<String>) -> Result<String> {
        if let Some(existing) = self.active_run().await? {
            anyhow::bail!(
                "run `{existing}` is still active; pause or cancel it before starting another \
                 (they would share one rollout server and one vault)"
            );
        }

        let tasks = self.task_set().await?;
        for note in tasks.size_notes() {
            tracing::warn!("{note}");
        }
        let run_id = format!(
            "run-{}",
            chrono::Local::now().format("%Y%m%d-%H%M%S")
        );
        let model = model_override.unwrap_or_else(|| self.config.models.inference.id.clone());
        let run = RunState::new(
            run_id.clone(),
            model,
            self.config.r#loop.iterations,
            self.config.r#loop.gate_rule,
        );
        self.store.save_run(&run).await?;

        let deps = self.loop_deps(&run_id).await?;
        let control = Arc::new(Control::default());
        self.controls
            .lock()
            .await
            .insert(run_id.clone(), Arc::clone(&control));

        let app = Arc::clone(self);
        let id = run_id.clone();
        tokio::spawn(async move {
            // Starting the server belongs in here, not in the handler. It is the one slow step
            // before the run proper, and doing it in the handler put an await that can fail —
            // or hang — after the run record and the control entry were already created: the
            // control was never removed, so `active_run` reported this run forever and every
            // later start was refused, while the run itself sat at `queued` with no error. It
            // also means a client that disconnects no longer cancels the start.
            //
            // Starting it per run rather than at daemon startup means a machine with no
            // executor installed can still serve the cockpit and show its runs.
            let mut runner = Runner::new(deps, tasks, run, control);
            let result = match app.supervisor.start().await {
                Ok(()) => runner.run_all().await,
                Err(e) => Err(e.context("starting the rollout server")),
            };
            if let Err(e) = result {
                tracing::error!(run = %id, "run failed: {e:#}");
                // The runner records its own failures, but a failure to start the server happens
                // before it runs at all — without this the record would stay `queued` forever.
                if runner.run.status != RunStatus::Failed {
                    runner.run.status = RunStatus::Failed;
                    runner.run.phase = "failed".into();
                    runner.run.error = Some(format!("{e:#}"));
                    runner.run.finished = Some(chrono::Utc::now());
                    if let Err(e) = app.store.save_run(&runner.run).await {
                        tracing::error!(run = %id, "recording the failure: {e:#}");
                    }
                }
            }
            app.controls.lock().await.remove(&id);
            if let Err(e) = app.supervisor.stop().await {
                tracing::warn!("stopping the rollout server: {e:#}");
            }
        });

        Ok(run_id)
    }

    /// Measures the empty-skill baseline on validation and test — phase 0's exit criterion.
    pub async fn baseline(self: &Arc<Self>) -> Result<RunState> {
        let tasks = self.task_set().await?;
        for note in tasks.size_notes() {
            tracing::warn!("{note}");
        }
        let deps = self.loop_deps("baseline").await?;
        self.supervisor.start().await?;

        let run_id = format!("baseline-{}", chrono::Local::now().format("%Y%m%d-%H%M%S"));
        let run = RunState::new(
            run_id,
            self.config.models.inference.id.clone(),
            self.config.r#loop.iterations,
            self.config.r#loop.gate_rule,
        );
        let mut runner = Runner::new(deps, tasks, run, Arc::new(Control::default()));
        runner.baseline().await?;
        runner.run.status = RunStatus::Completed;
        runner.run.finished = Some(chrono::Utc::now());
        self.store.save_run(&runner.run).await?;
        self.supervisor.stop().await?;
        Ok(runner.run.clone())
    }

    pub async fn control(&self, run_id: &str) -> Option<Arc<Control>> {
        self.controls.lock().await.get(run_id).cloned()
    }

    /// Id of a run this process is driving, if any.
    pub async fn active_run(&self) -> Result<Option<String>> {
        Ok(self.controls.lock().await.keys().next().cloned())
    }

    /// Renders the Seatbelt profile a rollout would run under, so it can be reviewed
    /// before anything runs — phase 0 asks for exactly this.
    pub fn render_profile(&self) -> Result<String> {
        let seatbelt = sandbox::Seatbelt::new(
            self.config.sandbox.clone(),
            self.config.daemon.state_dir.join("profiles"),
        );
        Ok(seatbelt.render(&ProfileRequest {
            // The state dir itself, matching `Supervisor::new`. A `work` subdirectory here would
            // print a policy narrower than the one rollouts actually run under, which defeats the
            // point of a reviewable profile — and hides exactly the class of bug that makes the
            // executor die with an empty stderr.
            work_dir: self.config.daemon.state_dir.clone(),
            agent_home: Some(self.config.executor.rollout_home.clone()),
            deny_read: vec![self.config.vault.clone()],
            allow_network: self.config.sandbox.allow_network_for_rollouts,
        }))
    }
}

/// A ready-to-use daemon over a throwaway vault. Shared with the API tests, which need a
/// real `App` behind the router.
#[cfg(test)]
pub(crate) async fn test_app(dir: &Path) -> Result<Arc<App>> {
    let vault = dir.join("Vault");
    let state = dir.join("state");
    let mut config = Config::sample(vault.clone(), state.clone());
    // No Seatbelt in CI containers; the daemon refuses unless this is declared.
    config.sandbox.disabled = true;
    let config_path = dir.join("config.json");
    config.save(&config_path).await?;
    Vault::init(&vault).await?;
    Repo::open(&vault).init().await?;
    App::load(&config_path).await
}

#[cfg(test)]
mod tests {
    use super::test_app as app;
    use super::*;

    #[tokio::test]
    async fn loading_requires_a_git_vault() {
        let dir = tempfile::tempdir().unwrap();
        let vault = dir.path().join("Vault");
        let mut config = Config::sample(vault.clone(), dir.path().join("state"));
        config.sandbox.disabled = true;
        let config_path = dir.path().join("config.json");
        config.save(&config_path).await.unwrap();
        Vault::init(&vault).await.unwrap();

        let err = match App::load(&config_path).await {
            Ok(_) => panic!("a non-git vault should be refused"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("not a git repository"), "{err}");
    }

    #[tokio::test]
    async fn loading_generates_a_token_and_reconciles_orphans() {
        let dir = tempfile::tempdir().unwrap();
        let app = app(dir.path()).await.unwrap();
        assert_eq!(app.token.len(), 32);

        let mut orphan = RunState::new(
            "run-orphan",
            "m",
            8,
            wikiskill_core::config::GateRule::Strict,
        );
        orphan.status = RunStatus::Running;
        app.store.save_run(&orphan).await.unwrap();

        let reloaded = App::load(&app.config_path).await.unwrap();
        assert_eq!(
            reloaded.store.load_run("run-orphan").await.unwrap().status,
            RunStatus::Failed
        );
        // The token is stable across restarts, so the cockpit keeps working.
        assert_eq!(reloaded.token, app.token);
    }

    #[tokio::test]
    async fn starting_a_run_needs_a_task_set() {
        let dir = tempfile::tempdir().unwrap();
        let app = app(dir.path()).await.unwrap();
        let err = app.start_run(None).await.unwrap_err().to_string();
        assert!(
            err.contains("eval") || err.contains("train"),
            "expected a task-set error, got: {err}"
        );
    }

    #[tokio::test]
    async fn the_profile_is_reviewable_before_anything_runs() {
        let dir = tempfile::tempdir().unwrap();
        let app = app(dir.path()).await.unwrap();
        let profile = app.render_profile().unwrap();
        assert!(profile.contains("(deny default)"));
        // The vault deny must come after the allows: last matching rule wins in SBPL.
        let deny_vault = profile.rfind("Vault").expect("vault deny block");
        let last_allow = profile.rfind("(allow file-write*").expect("write allows");
        assert!(deny_vault > last_allow, "{profile}");
    }

    /// A run that cannot start the executor must not hold the slot. This used to leak: the
    /// control entry was inserted before `supervisor.start()`, and nothing removed it when that
    /// failed, so `active_run` reported a run that did not exist and every later start was
    /// refused with "still active" until the daemon was restarted.
    #[tokio::test]
    async fn a_run_that_cannot_start_the_executor_releases_the_slot_and_records_why() {
        let dir = tempfile::tempdir().unwrap();
        let vault = dir.path().join("Vault");
        let mut config = Config::sample(vault.clone(), dir.path().join("state"));
        config.sandbox.disabled = true;
        // A binary that cannot exist, so `supervisor.start()` fails at once. Without this the
        // test would launch the real `opencode serve` if one is installed, and the run would
        // carry on into live model calls.
        config.executor.binary = dir.path().join("no-such-opencode").display().to_string();
        let config_path = dir.path().join("config.json");
        config.save(&config_path).await.unwrap();
        Vault::init(&vault).await.unwrap();
        Repo::open(&vault).init().await.unwrap();
        let app = App::load(&config_path).await.unwrap();
        seed_task_set(&app).await;
        // `loop_deps` refuses to build a rollout client without this; the secrets tests set it
        // to their own value under their own lock, and nothing removes it.
        std::env::set_var("FIREWORKS_API_KEY", "fw-test");

        let id = app.start_run(None).await.unwrap();
        // Returns before the executor is even reached: starting it is the background task's job.
        for _ in 0..100 {
            if app.active_run().await.unwrap().is_none() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(app.active_run().await.unwrap(), None, "the slot is still held");

        let run = app.store.load_run(&id).await.unwrap();
        assert_eq!(run.status, RunStatus::Failed, "left as {:?}", run.status);
        let error = run.error.unwrap_or_default();
        assert!(error.contains("rollout server"), "{error}");
        assert!(run.finished.is_some());

        // And the slot really is usable again.
        app.start_run(None).await.unwrap();
    }

    /// The smallest task set `TaskSet::load` accepts. `Config::sample` points the executor at a
    /// binary that does not exist, which is what makes the run above fail at the right step.
    async fn seed_task_set(app: &Arc<App>) {
        use wikiskill_core::eval::{Expect, Split, Task};
        let eval = app
            .vault
            .area_root(wikiskill_core::vault::Area::Eval);
        for (split, id) in [
            (Split::Train, "t0"),
            (Split::Val, "v0"),
            (Split::Test, "s0"),
        ] {
            wikiskill_core::eval::write_jsonl(
                &eval.join(split.file_name()),
                &[Task {
                    id: id.into(),
                    prompt: "fix the failing test".into(),
                    workspace: app.config.daemon.state_dir.join("ws"),
                    check_program: "/bin/echo".into(),
                    check_args: vec!["1/1".into()],
                    expect: Expect::Fraction,
                    tags: vec![],
                }],
            )
            .await
            .unwrap();
        }
    }
}
