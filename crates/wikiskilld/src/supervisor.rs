//! Supervising `opencode serve` — the rollout executor.
//!
//! Four things make this more than "spawn a process":
//!
//! * **Pinned version.** The loop talks to the V2 server API, which is still labelled
//!   experimental, so a version outside the pinned line is a hard error rather than a warning.
//! * **Isolated `HOME`.** The rollout server must not load the daily-driver config, the
//!   skills symlink, or any MCP server. It gets its own `HOME`/`XDG_CONFIG_HOME` under the
//!   support directory, with a config that denies the tools rollouts may not use.
//! * **Seatbelt.** OpenCode's own `external_directory` rules are advice a bash tool can
//!   walk around, so the real boundary is a generated SBPL profile: the work tree is
//!   writable, the vault is unreadable.
//! * **A generated server password.** V2 requires HTTP Basic auth and invents a password when
//!   `OPENCODE_SERVER_PASSWORD` is unset — which the daemon would then have to scrape out of
//!   the child's log. Generating one per daemon means the two sides agree without parsing, and
//!   a stray process from an earlier run cannot answer for this one. It goes in the child's
//!   environment, never in argv, where `ps` would publish it to every user on the host.
//!
//! Regenerating the agent file needs a restart, because OpenCode reads agent files at
//! startup — that is why [`Supervisor`] is also the loop's [`AgentFileSink`].

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::Mutex;

use wikiskill_core::config::Config;
use wikiskill_core::executor::{opencode::OpencodeClient, Executor};
use wikiskill_core::iteration::AgentFileSink;
use wikiskill_core::sandbox::{Isolation, ProfileRequest};

/// Where V2 looks for Markdown agents, relative to a config directory.
///
/// Plural. V1 read `agent/`; the documented V2 location is `~/.config/opencode/agents/<name>.md`,
/// and a file in the wrong one is not an error — it is an agent that silently does not exist,
/// which surfaces later as a session creation failure naming an agent the loop just wrote.
const AGENT_DIR: &str = "agents";

pub struct Supervisor {
    config: Config,
    isolation: Arc<dyn Isolation>,
    client: Arc<OpencodeClient>,
    child: Mutex<Option<tokio::process::Child>>,
    /// Only writable subtree: the support directory, which holds the rollout `HOME` and
    /// every per-rollout workspace copy.
    work_root: PathBuf,
    /// The Basic-auth password this daemon will hand its server. Generated once per
    /// [`Supervisor`], so a restart keeps the client and the child in agreement.
    password: String,
}

impl Supervisor {
    pub fn new(config: Config, isolation: Arc<dyn Isolation>) -> Result<Self> {
        let password = uuid::Uuid::new_v4().simple().to_string();
        let client = Arc::new(OpencodeClient::new(
            config.executor.base_url.clone(),
            config.executor.pinned_version.clone(),
            config.executor.model_provider.clone(),
            password.clone(),
        )?);
        let work_root = config.daemon.state_dir.clone();
        Ok(Self {
            config,
            isolation,
            client,
            child: Mutex::new(None),
            work_root,
            password,
        })
    }

    pub fn client(&self) -> Arc<OpencodeClient> {
        Arc::clone(&self.client)
    }

    /// Base URL the executor should use for the deploy model.
    ///
    /// Only Fireworks is offered here on purpose: the rollout key in the executor's environment
    /// is the Fireworks one, and the curator endpoints (Bedrock) are deliberately unreachable
    /// from a rollout.
    fn deploy_base_url(&self) -> Result<&'static str> {
        match self.config.models.inference.endpoint {
            wikiskill_core::config::Endpoint::Fireworks => {
                Ok(wikiskill_core::model::openai_compat::FIREWORKS_BASE_URL)
            }
            other => anyhow::bail!(
                "models.inference.endpoint is {other:?}, but rollouts reach their model through \
                 the executor, which is given only the Fireworks key. Point the deploy model at \
                 fireworks, or extend the rollout provider declaration."
            ),
        }
    }

    /// The rollout server's private config directory. Written on every start, so an edit
    /// to the daily-driver config can never leak into rollouts.
    pub async fn write_rollout_config(&self) -> Result<()> {
        let home = &self.config.executor.rollout_home;
        let config_dir = home.join(".config/opencode");
        tokio::fs::create_dir_all(config_dir.join(AGENT_DIR)).await?;

        // The deploy models get their own provider entry rather than relying on the executor's
        // bundled catalog. A measurement is pinned to a dated model id, and a dated id the
        // provider serves today is routinely absent from that catalog — and declaring models
        // under a catalog provider only *overrides* ones it already knows, so an unknown id
        // stays unknown. A custom OpenAI-compatible provider is the one form that registers it,
        // which keeps the pin (and so the comparability of scores across runs) intact.
        let mut models = serde_json::Map::new();
        let deploy = std::iter::once(&self.config.models.inference)
            .chain(self.config.models.exploratory.as_ref());
        for spec in deploy {
            models.insert(
                spec.id.clone(),
                serde_json::json!({
                    "name": spec.id,
                    // V2 assumes tools, text and image input for an uncatalogued model, but
                    // those are its words "fallback assumptions, not detected capabilities".
                    // Stating them makes a rollout that cannot call a tool a config error here
                    // rather than a mute transcript scored 0.
                    "capabilities": { "tools": true, "input": ["text"], "output": ["text"] },
                }),
            );
        }
        let mut providers = serde_json::Map::new();
        providers.insert(
            self.config.executor.model_provider.clone(),
            serde_json::json!({
                "name": "WikiSkill deploy models",
                "package": "@opencode/ai/providers/openai-compatible",
                // The credential is named, not written. It is resolved by the executor from its
                // own environment, which holds only the rollout key — putting the key itself in
                // this file would hand it to every rollout, which can read its own HOME.
                "env": ["FIREWORKS_API_KEY"],
                "settings": { "baseURL": self.deploy_base_url()? },
                "models": models,
            }),
        );

        // The permission ruleset is the same one the daemon attaches to every session. Sending
        // it twice is deliberate: the session copy is the one that counts, and this one keeps
        // the server's own housekeeping agents — title, summary, compaction — from reaching a
        // tool a rollout may not use.
        let config = serde_json::json!({
            "$schema": "https://opencode.ai/config.json",
            // An update mid-run would swap the executor under a measurement. "disable" also
            // keeps the sandbox from having to allow a write to the install directory.
            "update": "disable",
            // Snapshots commit to the workspace's git repo for undo. The rollout's workspace is
            // a copy the loop diffs itself, and an extra commit in it is noise in the diff.
            "snapshots": false,
            "mcp": { "servers": {} },
            "model": format!(
                "{}/{}",
                self.config.executor.model_provider, self.config.models.inference.id
            ),
            "providers": providers,
            "permissions": wikiskill_core::executor::opencode::rollout_permissions(),
        });
        tokio::fs::write(
            config_dir.join("opencode.json"),
            serde_json::to_vec_pretty(&config)?,
        )
        .await?;
        Ok(())
    }

    /// Starts the server and waits for it to answer, then checks the pinned version.
    pub async fn start(&self) -> Result<()> {
        self.write_rollout_config().await?;
        let mut guard = self.child.lock().await;
        if guard.is_some() {
            return Ok(());
        }

        let (host, port) = split_host_port(&self.config.executor.base_url)?;
        let args: Vec<String> = vec![
            "serve".into(),
            "--hostname".into(),
            host,
            "--port".into(),
            port.to_string(),
        ];

        let request = ProfileRequest {
            work_dir: self.work_root.clone(),
            agent_home: Some(self.config.executor.rollout_home.clone()),
            // The paper's ablation: an inference agent that can read the wiki produces
            // worse skills. The daemon is the only process that touches the vault.
            deny_read: vec![self.config.vault.clone()],
            allow_network: self.config.sandbox.allow_network_for_rollouts,
        };
        let mut command = self
            .isolation
            .command(&request, &self.config.executor.binary, &args)
            .await?;

        let home = &self.config.executor.rollout_home;
        tokio::fs::create_dir_all(home).await?;
        tokio::fs::create_dir_all(&self.work_root).await?;
        command
            // Otherwise the server inherits the daemon's working directory, which is wherever
            // the user happened to launch it — a path the sandbox has no reason to allow, and a
            // cwd the rollout has no business seeing.
            .current_dir(&self.work_root)
            .env_clear()
            .env("HOME", home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_DATA_HOME", home.join(".local/share"))
            .env("XDG_CACHE_HOME", home.join(".cache"))
            .env("PATH", std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into()))
            .env("TERM", "dumb")
            .env("NO_COLOR", "1")
            // V2 requires Basic auth and generates a password when this is unset. Setting it is
            // what lets the client authenticate without reading the child's output.
            .env("OPENCODE_SERVER_PASSWORD", &self.password)
            // `env_clear` above means the server would otherwise log nothing at all, and its own
            // log is the only place the reason behind a 500 from its API appears. Forwarded
            // rather than fixed so turning it on is a daemon-level decision, not a rebuild.
            .envs(
                std::env::var("OPENCODE_LOG_LEVEL")
                    .ok()
                    .map(|v| ("OPENCODE_LOG_LEVEL".to_string(), v)),
            )
            // Only the keys rollouts need; curator and Jev credentials stay behind.
            .envs(crate::secrets::rollout_env())
            .kill_on_drop(true);

        let child = command
            .spawn()
            .map_err(|e| anyhow::anyhow!("spawning `{} serve`: {e}", self.config.executor.binary))?;
        *guard = Some(child);
        drop(guard);

        self.wait_until_ready(Duration::from_secs(30)).await?;
        let info = self.client.check_pinned().await?;
        tracing::info!(
            version = %info.version,
            isolation = self.isolation.name(),
            "rollout server ready"
        );
        Ok(())
    }

    async fn wait_until_ready(&self, timeout: Duration) -> Result<()> {
        // Bounded twice over: the loop's own deadline, and this outer one. The inner deadline is
        // only consulted *between* attempts, so a single probe that hangs makes it unenforceable —
        // and a `start()` that never returns is a run that stays `queued` with no error anywhere.
        match tokio::time::timeout(timeout + Duration::from_secs(5), self.poll_until_ready(timeout))
            .await
        {
            Ok(result) => result,
            Err(_) => Err(anyhow::anyhow!(
                "`opencode serve` readiness check hung for more than {timeout:?}; it accepted a \
                 connection without answering"
            )),
        }
    }

    async fn poll_until_ready(&self, timeout: Duration) -> Result<()> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut last_error = None;
        while tokio::time::Instant::now() < deadline {
            match self.client.info().await {
                Ok(_) => return Ok(()),
                Err(e) => last_error = Some(e),
            }
            // A server that died on startup will never answer; surface that instead.
            if let Some(child) = self.child.lock().await.as_mut() {
                if let Some(status) = child.try_wait()? {
                    anyhow::bail!("`opencode serve` exited immediately with {status}");
                }
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        Err(anyhow::anyhow!(
            "`opencode serve` did not become ready within {timeout:?}: {}",
            last_error
                .map(|e| e.to_string())
                .unwrap_or_else(|| "no response".into())
        ))
    }

    pub async fn stop(&self) -> Result<()> {
        let mut guard = self.child.lock().await;
        if let Some(mut child) = guard.take() {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
        Ok(())
    }

    pub async fn restart(&self) -> Result<()> {
        self.stop().await?;
        self.start().await
    }

    pub async fn running(&self) -> bool {
        match self.child.lock().await.as_mut() {
            Some(child) => matches!(child.try_wait(), Ok(None)),
            None => false,
        }
    }

    fn agent_path(&self, agent: &str) -> PathBuf {
        self.config
            .executor
            .rollout_home
            .join(".config/opencode")
            .join(AGENT_DIR)
            .join(format!("{agent}.md"))
    }
}

#[async_trait]
impl AgentFileSink for Supervisor {
    /// Writes the regenerated agent file, then restarts the server so it is loaded.
    ///
    /// The write is skipped when the contents are unchanged: a needless restart would
    /// throw away the server's warm state for no gain.
    async fn publish(&self, agent: &str, contents: &str) -> Result<()> {
        let path = self.agent_path(agent);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let unchanged = matches!(
            tokio::fs::read_to_string(&path).await,
            Ok(existing) if existing == contents
        );
        if unchanged && self.running().await {
            return Ok(());
        }
        tokio::fs::write(&path, contents).await?;
        tracing::info!(agent, path = %path.display(), "regenerated the rollout agent file");
        self.restart().await
    }
}

/// `http://127.0.0.1:4096` to `("127.0.0.1", 4096)`.
pub fn split_host_port(base_url: &str) -> Result<(String, u16)> {
    let rest = base_url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(base_url)
        .trim_end_matches('/');
    let (host, port) = rest
        .rsplit_once(':')
        .ok_or_else(|| anyhow::anyhow!("executor.base_url needs an explicit port: {base_url}"))?;
    Ok((
        host.to_string(),
        port.parse()
            .map_err(|_| anyhow::anyhow!("executor.base_url has a non-numeric port: {base_url}"))?,
    ))
}

/// Refuses to run rollouts unsandboxed unless the config says so out loud.
pub fn check_isolation(config: &Config, isolation: &dyn Isolation) -> Result<()> {
    if isolation.name() == "none" && !config.sandbox.disabled {
        anyhow::bail!(
            "no isolation is available on this host, but sandbox.disabled is false. \
             Rollouts run untrusted model output with a bash tool; set sandbox.disabled \
             to true only for CI on a host you are willing to hand to the model."
        );
    }
    if config.sandbox.disabled {
        tracing::warn!(
            "sandbox.disabled is true: rollouts run WITHOUT isolation and can read and \
             write anything this user can, including the vault"
        );
    }
    Ok(())
}

/// Path of the generated agent file, for `wikiskilld status` and the cockpit.
pub fn agent_file_path(config: &Config) -> PathBuf {
    config
        .executor
        .rollout_home
        .join(".config/opencode")
        .join(AGENT_DIR)
        .join(format!("{}.md", config.executor.agent))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use wikiskill_core::model::openai_compat;
    use wikiskill_core::sandbox::NoIsolation;

    fn config(dir: &Path) -> Config {
        Config::sample(dir.join("Vault"), dir.join("state"))
    }

    #[test]
    fn base_urls_split_into_host_and_port() {
        assert_eq!(
            split_host_port("http://127.0.0.1:4096").unwrap(),
            ("127.0.0.1".to_string(), 4096)
        );
        assert_eq!(
            split_host_port("127.0.0.1:9/").unwrap(),
            ("127.0.0.1".to_string(), 9)
        );
        assert!(split_host_port("http://127.0.0.1").is_err());
        assert!(split_host_port("http://127.0.0.1:abc").is_err());
    }

    #[tokio::test]
    async fn the_rollout_config_denies_the_native_skill_tool() {
        let dir = tempfile::tempdir().unwrap();
        let supervisor =
            Supervisor::new(config(dir.path()), Arc::new(NoIsolation)).unwrap();
        supervisor.write_rollout_config().await.unwrap();

        let text = tokio::fs::read_to_string(
            dir.path()
                .join("state/rollout-home/.config/opencode/opencode.json"),
        )
        .await
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        let rules = value["permissions"].as_array().expect("V2 takes an ordered array");
        let effect_of = |action: &str| {
            rules
                .iter()
                .rev()
                .find(|r| r["action"] == action)
                .and_then(|r| r["effect"].as_str())
        };
        assert_eq!(effect_of("skill"), Some("deny"));
        assert_eq!(effect_of("webfetch"), Some("deny"));
        assert_eq!(effect_of("external_directory"), Some("deny"));
        assert_eq!(value["mcp"]["servers"], serde_json::json!({}));
        assert_eq!(value["update"], "disable");
        assert_eq!(value["snapshots"], serde_json::json!(false));
        // V1's keys. Writing one now would be silently ignored, which is worse than a rejection.
        for stale in ["provider", "permission", "tools", "autoupdate"] {
            assert!(
                value.get(stale).is_none(),
                "`{stale}` is a V1 key and does nothing in V2"
            );
        }
    }

    /// A pinned, dated model id is often absent from the executor's bundled catalog even when the
    /// provider serves it. Declaring it keeps the pin — and the measurement — intact.
    #[tokio::test]
    async fn the_rollout_config_declares_the_pinned_deploy_model() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path());
        let supervisor = Supervisor::new(cfg.clone(), Arc::new(NoIsolation)).unwrap();
        supervisor.write_rollout_config().await.unwrap();

        let text = tokio::fs::read_to_string(
            cfg.executor.rollout_home.join(".config/opencode/opencode.json"),
        )
        .await
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        let entry = &value["providers"][&cfg.executor.model_provider];
        assert!(
            entry["models"].get(&cfg.models.inference.id).is_some(),
            "the deploy model must be declared, got {}",
            entry["models"]
        );
        assert_eq!(
            entry["models"][&cfg.models.inference.id]["capabilities"]["tools"],
            serde_json::json!(true)
        );
        assert_eq!(
            entry["package"],
            "@opencode/ai/providers/openai-compatible"
        );
        assert_eq!(entry["settings"]["baseURL"], openai_compat::FIREWORKS_BASE_URL);
        // The rollout can read this file: the key must stay a *name* the daemon resolves from
        // the executor's environment, never the value.
        assert_eq!(entry["env"], serde_json::json!(["FIREWORKS_API_KEY"]));
        assert!(!text.contains("sk-"), "no credential belongs in the rollout config");
        assert!(!text.contains("fw_"), "no credential belongs in the rollout config");
        // The default model must resolve through our own provider. Left unset, V2 falls back to
        // "the newest available supported model", which is a different measurement.
        assert_eq!(
            value["model"],
            format!("{}/{}", cfg.executor.model_provider, cfg.models.inference.id)
        );
    }

    /// The password is the only thing that lets the client talk to the server at all, and the one
    /// place it must never appear is argv, which `ps` publishes to every user on the host.
    #[tokio::test]
    async fn the_server_password_is_generated_and_never_lands_in_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path());
        let a = Supervisor::new(cfg.clone(), Arc::new(NoIsolation)).unwrap();
        let b = Supervisor::new(cfg.clone(), Arc::new(NoIsolation)).unwrap();
        assert_ne!(a.password, b.password, "a stale server must not answer for this daemon");
        assert_eq!(a.password.len(), 32);

        a.write_rollout_config().await.unwrap();
        let text = tokio::fs::read_to_string(
            cfg.executor.rollout_home.join(".config/opencode/opencode.json"),
        )
        .await
        .unwrap();
        assert!(
            !text.contains(&a.password),
            "the rollout can read its own config; the password belongs only in the child's env"
        );
    }

    #[tokio::test]
    async fn publishing_an_agent_file_writes_it_where_opencode_looks() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path());
        let supervisor = Supervisor::new(cfg.clone(), Arc::new(NoIsolation)).unwrap();
        // No server to restart here, so the restart attempt fails; the file must still
        // be on disk, because the next start reads it.
        let _ = supervisor.publish("wiki-inference", "# agent\n").await;
        assert_eq!(
            tokio::fs::read_to_string(agent_file_path(&cfg)).await.unwrap(),
            "# agent\n"
        );
    }

    #[test]
    fn running_without_isolation_must_be_declared() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = config(dir.path());
        let err = check_isolation(&cfg, &NoIsolation).unwrap_err().to_string();
        assert!(err.contains("sandbox.disabled is false"), "{err}");
        cfg.sandbox.disabled = true;
        assert!(check_isolation(&cfg, &NoIsolation).is_ok());
    }
}
