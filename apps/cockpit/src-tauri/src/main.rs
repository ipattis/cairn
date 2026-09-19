//! The cockpit: a window onto the daemon, and nothing more.
//!
//! Every button here is a call to `wikiskilld` on 127.0.0.1. The Rust side is a relay: it
//! holds the bearer token and forwards paths the webview names, so the webview never sees
//! the token and cannot be talked into reading a provider key. Closing this window does not
//! stop a run — the daemon owns the run.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;

use serde::Serialize;
use tauri::Manager;

/// Where the daemon keeps its config, token and runs.
fn state_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join("Library/Application Support/wikiskill")
}

struct Daemon {
    base: String,
    token: String,
    http: reqwest::Client,
}

impl Daemon {
    /// Reads the daemon's own config and token from disk. Both are 0600-ish local files the
    /// user already owns; the point is that they stop here and do not reach the webview.
    fn load() -> Result<Self, String> {
        let dir = state_dir();
        // Matches `wikiskilld --config`: a daemon pointed at a non-default config can still
        // be driven from the cockpit.
        let config_path = std::env::var_os("WIKISKILL_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|| dir.join("config.json"));
        let text = std::fs::read_to_string(&config_path).map_err(|e| {
            format!(
                "cannot read {}: {e}. Run `wikiskilld init --vault <path>` first.",
                config_path.display()
            )
        })?;
        let config: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| format!("{} is not valid JSON: {e}", config_path.display()))?;

        let state_dir = config["daemon"]["state_dir"]
            .as_str()
            .map(PathBuf::from)
            .unwrap_or(dir);
        let bind = config["daemon"]["bind"]
            .as_str()
            .unwrap_or("127.0.0.1:8787")
            .to_string();
        // A token in the config wins, matching the daemon's own precedence.
        let token = match config["daemon"]["token"].as_str() {
            Some(token) if !token.is_empty() => token.to_string(),
            _ => std::fs::read_to_string(state_dir.join("api-token"))
                .map_err(|e| format!("cannot read the API token: {e}. Is the daemon installed?"))?
                .trim()
                .to_string(),
        };

        Ok(Self {
            base: format!("http://{bind}"),
            token,
            http: reqwest::Client::builder()
                // Long enough for `/v1/baseline`, which runs a whole validation split.
                .timeout(std::time::Duration::from_secs(3600))
                .build()
                .map_err(|e| e.to_string())?,
        })
    }

    async fn send(
        &self,
        method: &str,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<Reply, String> {
        // The webview names a path, never a host: it cannot point this at another machine.
        if !path.starts_with("/v1/") && path != "/health" {
            return Err(format!("refusing to relay to `{path}`"));
        }
        let method = reqwest::Method::from_bytes(method.as_bytes())
            .map_err(|_| format!("bad method `{method}`"))?;
        let mut request = self
            .http
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(&self.token);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.map_err(|e| {
            format!("{e}. Is the daemon running? `wikiskilld serve`, or `wikiskilld install-agent`.")
        })?;
        let status = response.status().as_u16();
        let text = response.text().await.map_err(|e| e.to_string())?;
        Ok(Reply { status, text })
    }
}

#[derive(Serialize)]
struct Reply {
    status: u16,
    /// The daemon's body verbatim. The Gate view renders the impact log as committed rather
    /// than reformatting it, so what a reviewer reads is what is in the vault.
    text: String,
}

#[tauri::command]
async fn api(
    state: tauri::State<'_, Daemon>,
    method: String,
    path: String,
    body: Option<serde_json::Value>,
) -> Result<Reply, String> {
    state.send(&method, &path, body).await
}

/// Hands an `obsidian://` link to the OS. Restricted to that scheme: a general "open this
/// URL" command in a window that renders model output is a way to launch anything.
#[tauri::command]
fn open_obsidian(link: String) -> Result<(), String> {
    if !link.starts_with("obsidian://") {
        return Err("only obsidian:// links can be opened from here".into());
    }
    std::process::Command::new("/usr/bin/open")
        .arg(&link)
        .status()
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Surfaced in the footer so a failure to reach the daemon is diagnosable.
#[tauri::command]
fn where_is_the_daemon(state: tauri::State<'_, Daemon>) -> String {
    state.base.clone()
}

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            match Daemon::load() {
                Ok(daemon) => {
                    app.manage(daemon);
                }
                Err(e) => {
                    // Without the token nothing can be relayed; say so in a dialog-free way
                    // the frontend can render, rather than panicking into a blank window.
                    eprintln!("cockpit: {e}");
                    app.manage(Daemon {
                        base: String::new(),
                        token: String::new(),
                        http: reqwest::Client::new(),
                    });
                }
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![api, open_obsidian, where_is_the_daemon])
        .run(tauri::generate_context!())
        .expect("running the cockpit");
}
