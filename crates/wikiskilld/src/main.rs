//! `wikiskilld` — the daemon that owns the WikiSkill loop.
//!
//! The cockpit is a viewer; this is the thing that runs. It supervises the rollout
//! executor, drives iterations, writes the vault, and serves a localhost API. An
//! eight-iteration run takes hours, so it must survive the app being closed — hence a
//! daemon with durable state rather than logic inside a GUI.

mod api;
mod app;
mod daily;
mod launchd;
mod obsidian;
mod secrets;
mod supervisor;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use clap::{Parser, Subcommand};

use wikiskill_core::config::{default_state_dir, Config};
use wikiskill_core::eval::{Split, Task};
use wikiskill_core::git::Repo;
use wikiskill_core::state::{RunStatus, Store};
use wikiskill_core::vault::Vault;

#[derive(Parser)]
#[command(name = "wikiskilld", about = "WikiSkill harness daemon", version)]
struct Cli {
    /// Config file. Defaults to `<support dir>/config.json`.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create the config, the vault layout, its git repo, and an eval template.
    Init {
        /// Vault path. Must be absolute and outside Documents, Desktop and iCloud Drive.
        #[arg(long)]
        vault: PathBuf,
        /// Overwrite an existing config.
        #[arg(long)]
        force: bool,
    },
    /// Measure the empty-skill validation score: phase 0's exit criterion and the gate's
    /// starting bar.
    Baseline,
    /// Run the loop to completion, printing each phase.
    Run {
        /// Override `models.inference.id` for this run.
        #[arg(long)]
        model: Option<String>,
    },
    /// Serve the localhost API and the schedules.
    Serve,
    /// Print what the daemon knows, without starting anything.
    Status,
    /// Print the Seatbelt profile rollouts run under, for review.
    Profile,
    /// Print the localhost API bearer token.
    Token,
    /// Manage provider credentials in the macOS Keychain.
    #[command(subcommand)]
    Credential(CredentialCommand),
    /// Install and load the launchd user agent.
    InstallAgent,
    /// Unload and remove the launchd user agent.
    UninstallAgent,
}

#[derive(Subcommand)]
enum CredentialCommand {
    /// Which credentials exist. Values are never printed.
    List,
    /// Store one credential. The value is read from the terminal, never from an argument:
    /// anything in argv is visible to every process on the machine through `ps`.
    Set {
        /// Keychain service name, e.g. `wikiskill-fireworks`.
        service: String,
    },
    /// Remove one credential from the Keychain.
    Unset {
        service: String,
    },
}

async fn credential(config_path: &PathBuf, command: CredentialCommand) -> Result<()> {
    match command {
        CredentialCommand::List => {
            // The config decides which keys a run actually needs, so read it if it is there.
            let (jev, curators) = match Config::load(config_path).await {
                Ok(config) => (
                    config.jev.enabled,
                    matches!(
                        config.models.maintainer.endpoint,
                        wikiskill_core::config::Endpoint::BedrockMantle
                    ) || matches!(
                        config.models.proposer.endpoint,
                        wikiskill_core::config::Endpoint::BedrockMantle
                    ),
                ),
                Err(_) => (false, true),
            };
            for status in secrets::status(jev, curators).await? {
                println!(
                    "{:<26} {:<24} {}{}",
                    status.service,
                    status.env,
                    if status.in_keychain { "set" } else { "MISSING" },
                    if status.required { "" } else { " (not required by this config)" }
                );
            }
            Ok(())
        }
        CredentialCommand::Set { service } => {
            let spec = secrets::spec_for(&service).ok_or_else(|| {
                anyhow::anyhow!(
                    "`{service}` is not one of this daemon's credentials. Known: {}",
                    secrets::SPECS
                        .iter()
                        .map(|s| s.service)
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?;
            eprintln!("{} — {}", spec.label, spec.purpose);
            eprintln!("Sets the Keychain item read into {}.", spec.env);
            // `security` prompts for the value itself, with echo off, twice. The value never
            // passes through this process, so it cannot be logged here by accident.
            secrets::write_interactive(&service).await?;
            println!("stored as `{service}`; a running daemon picks it up on restart");
            Ok(())
        }
        CredentialCommand::Unset { service } => {
            secrets::spec_for(&service)
                .ok_or_else(|| anyhow::anyhow!("`{service}` is not one of this daemon's credentials"))?;
            if secrets::delete(&service).await? {
                println!("removed `{service}`");
            } else {
                println!("`{service}` was not set");
            }
            Ok(())
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,wikiskill_core=info".into()),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();
    let config_path = cli
        .config
        .clone()
        .unwrap_or_else(|| default_state_dir().join("config.json"));

    match cli.command {
        Command::Init { vault, force } => init(&config_path, &vault, force).await,
        Command::Baseline => baseline(&config_path).await,
        Command::Run { model } => run(&config_path, model).await,
        Command::Serve => serve(&config_path).await,
        Command::Status => status(&config_path).await,
        Command::Profile => {
            let app = app::App::load(&config_path).await?;
            println!("{}", app.render_profile()?);
            Ok(())
        }
        Command::Token => {
            let config = Config::load(&config_path).await?;
            let store = Store::new(config.daemon.state_dir.clone());
            println!("{}", store.ensure_token().await?);
            Ok(())
        }
        Command::Credential(command) => credential(&config_path, command).await,
        Command::InstallAgent => {
            let binary = std::env::current_exe()?;
            let log_dir = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()))
                .join("Library/Logs/wikiskill");
            let path = launchd::install(&binary, &config_path, &log_dir).await?;
            println!("loaded {}", path.display());
            println!("logs: {}/wikiskilld.log", log_dir.display());
            Ok(())
        }
        Command::UninstallAgent => {
            launchd::uninstall().await?;
            println!("unloaded {}", launchd::LABEL);
            Ok(())
        }
    }
}

async fn init(config_path: &PathBuf, vault: &PathBuf, force: bool) -> Result<()> {
    if config_path.exists() && !force {
        anyhow::bail!(
            "{} already exists; pass --force to overwrite",
            config_path.display()
        );
    }
    let config = Config::sample(vault.clone(), default_state_dir());
    config.validate()?;

    let vault_handle = Vault::init(vault).await?;
    let repo = Repo::open(vault);
    repo.init().await?;
    seed_eval_template(&vault_handle).await?;
    // The vault is an Obsidian folder as well as a git repo, and the notes link with
    // [[wikilinks]] — so the folder is configured for that before anyone opens it.
    obsidian::prepare(vault).await?;
    repo.commit_all("wikiskill: initialise vault layout and eval template")
        .await?;

    tokio::fs::create_dir_all(&config.daemon.state_dir).await?;
    config.save(config_path).await?;
    let token = Store::new(config.daemon.state_dir.clone())
        .ensure_token()
        .await?;

    println!("vault:  {}", vault.display());
    println!("config: {}", config_path.display());
    println!("token:  {token}");
    println!();
    println!("Next:");
    println!("  1. Set the model IDs in the config (the placeholders are not real IDs).");
    println!("  2. Add credentials, from the cockpit's Setup tab or here:");
    for spec in secrets::SPECS {
        println!("       wikiskilld credential set {}", spec.service);
    }
    println!("  3. Fill in {}/eval/*.jsonl — 30 train, 15 val, 15 test, no overlap.", vault.display());
    println!("  4. `wikiskilld profile` and read the sandbox policy before running anything.");
    println!("  5. `wikiskilld baseline` — the empty-skill score everything is measured against.");
    println!();
    println!("Open the vault in Obsidian once, so the cockpit's note links work:");
    println!("  Obsidian → vault switcher → \"Open folder as vault\" → {}", vault.display());
    Ok(())
}

/// Writes an eval template that documents the shape rather than inventing tasks: the task
/// set is the part of this system that cannot be generated, and a wrong verifier silently
/// corrupts every gate decision downstream.
async fn seed_eval_template(vault: &Vault) -> Result<()> {
    let dir = vault.area_root(wikiskill_core::vault::Area::Eval);
    tokio::fs::create_dir_all(&dir).await?;

    let example = Task {
        id: "example-fix-failing-test".into(),
        prompt: "The test suite has one failing test. Find it and fix the cause, not the \
                 test."
            .into(),
        workspace: dir.join("workspaces/example-fix-failing-test"),
        check_program: "/bin/sh".into(),
        check_args: vec![
            "-c".into(),
            // The documented contract: print `passed/total` on the last line.
            "cargo test 2>/dev/null | awk '/test result:/ {print $4\"/\"($4+$6)}' | tail -1"
                .into(),
        ],
        expect: wikiskill_core::eval::Expect::Fraction,
        tags: vec!["rust".into(), "debugging".into()],
    };

    for split in [Split::Train, Split::Val, Split::Test] {
        let path = dir.join(split.file_name());
        if path.exists() {
            continue;
        }
        // Only train gets the example, so `validate()` complains about the empty splits
        // instead of a run quietly gating on one task.
        let tasks: Vec<Task> = if split == Split::Train {
            vec![example.clone()]
        } else {
            vec![]
        };
        wikiskill_core::eval::write_jsonl(&path, &tasks).await?;
    }

    let readme = "# Eval\n\n\
        One JSON task per line. `train.jsonl` 30, `val.jsonl` 15, `test.jsonl` 15, with no \
        overlap between splits — the daemon refuses to run if a task id appears twice.\n\n\
        Fields:\n\n\
        - `id` — stable, used in raw-note filenames.\n\
        - `prompt` — what the rollout agent is asked to do.\n\
        - `workspace` — a directory copied fresh for every rollout. Never mutated.\n\
        - `check_program` / `check_args` — the verifier. Deterministic, no network.\n\
        - `expect` — `fraction` (print `passed/total`), `score` (print 0-1), `exit-zero`, \
        or `stdout-contains`.\n\
        - `tags` — free-form, for the cockpit.\n\n\
        The verifier is the ground truth for every gate decision: 15 validation tasks give \
        6.7-point granularity, so a one-task flake looks exactly like a real improvement. \
        Prefer checks that cannot flake over checks that are easy to write.\n";
    let readme_path = dir.join("README.md");
    if !readme_path.exists() {
        tokio::fs::write(readme_path, readme).await?;
    }
    tokio::fs::create_dir_all(dir.join("workspaces")).await?;
    Ok(())
}

async fn baseline(config_path: &PathBuf) -> Result<()> {
    secrets::load_into_env().await?;
    let app = app::App::load(config_path).await?;
    let run = app.baseline().await?;
    println!(
        "baseline validation score: {:.3}  (run {})",
        run.baseline_val.unwrap_or_default(),
        run.id
    );
    println!(
        "the gate starts here: a proposal is accepted only if it strictly beats {:.3}",
        run.best
    );
    Ok(())
}

async fn run(config_path: &PathBuf, model: Option<String>) -> Result<()> {
    secrets::load_into_env().await?;
    let app = app::App::load(config_path).await?;
    let id = app.start_run(model).await?;
    println!("started {id}");

    // Follow it: the CLI path is also how phase 1 is exercised, and a silent hour is
    // indistinguishable from a hang.
    let mut last_phase = String::new();
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let run = app.store.load_run(&id).await?;
        if run.phase != last_phase {
            println!("  {}", run.phase);
            last_phase = run.phase.clone();
        }
        if run.status.terminal() {
            println!();
            println!("status:   {:?}", run.status);
            println!("baseline: {:?}", run.baseline_val);
            println!("best:     {:.3}", run.best);
            println!("test:     {:?}", run.test_score);
            println!("accepted: {} of {}", run.accepted_count(), run.iterations.len());
            if let Some(error) = &run.error {
                println!("error:    {error}");
            }
            println!(
                "impact log: {}",
                app.vault.obsidian_link("wiki/skill-impact.md")
            );
            if run.status == RunStatus::Failed {
                std::process::exit(1);
            }
            return Ok(());
        }
    }
}

async fn status(config_path: &PathBuf) -> Result<()> {
    let config = Config::load(config_path).await?;
    println!("config:     {}", config_path.display());
    println!("vault:      {}", config.vault.display());
    println!("state dir:  {}", config.daemon.state_dir.display());
    println!("bind:       {}", config.daemon.bind);
    println!(
        "gate rule:  {} ({} validation run(s) per iteration)",
        config.r#loop.gate_rule.label(),
        config.r#loop.gate_rule.validation_runs()
    );
    println!(
        "jev:        {}",
        match (config.jev.enabled, config.jev.shadow_mode) {
            (false, _) => "disabled".to_string(),
            (true, true) => format!("{} (shadow mode: logged, never acted on)", config.jev.version),
            (true, false) => format!("{} (live)", config.jev.version),
        }
    );
    println!(
        "executor:   {} pinned to {}",
        config.executor.binary, config.executor.pinned_version
    );

    let found = secrets::load_into_env().await?;
    for (env, present) in &found {
        println!("credential: {env} {}", if *present { "found" } else { "MISSING" });
    }

    match Vault::open(&config.vault) {
        Ok(vault) => {
            let skills = vault.active_skills().await?;
            println!("skills:     {}", skills.len());
            for skill in &skills {
                println!("  - {}", skill.name);
            }
        }
        Err(e) => println!("vault:      NOT READY: {e:#}"),
    }

    let store = Store::new(config.daemon.state_dir.clone());
    let runs = store.list_runs().await?;
    println!("runs:       {}", runs.len());
    for run in runs.iter().take(5) {
        println!(
            "  {} {:?} best {:.3} — {}",
            run.id, run.status, run.best, run.phase
        );
    }
    let pending = store.load_pending_labels().await?;
    let unresolved = pending.iter().filter(|l| l.human_verdict.is_none()).count();
    if unresolved > 0 {
        println!("labels:     {unresolved} awaiting a human pass/fail");
    }
    Ok(())
}

async fn serve(config_path: &PathBuf) -> Result<()> {
    secrets::load_into_env().await?;
    let app = app::App::load(config_path).await?;
    let bind = app.config.daemon.bind.clone();

    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .map_err(|e| anyhow::anyhow!("binding {bind}: {e}"))?;
    tracing::info!("listening on http://{bind}");
    tracing::info!("token: {}", app.token);

    let scheduler = tokio::spawn(daily::scheduler(Arc::clone(&app)));

    let router = api::router(Arc::clone(&app))
        .layer(tower_http::trace::TraceLayer::new_for_http());
    let serve_app = Arc::clone(&app);
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            tracing::info!("shutting down; stopping the rollout server");
            if let Err(e) = serve_app.supervisor.stop().await {
                tracing::warn!("stopping the rollout server: {e:#}");
            }
        })
        .await?;

    scheduler.abort();
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(e) => tracing::warn!("cannot listen for SIGTERM: {e}"),
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}
