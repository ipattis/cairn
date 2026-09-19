//! Obsidian: the vault's other reader.
//!
//! The vault is a git repo and an Obsidian folder at the same time, because the wiki is meant
//! to be read by a person while the loop is running. The daemon can prepare the folder so
//! Obsidian treats it the way the notes expect — `[[wikilinks]]`, not Markdown links — but it
//! cannot register the vault with Obsidian: there is no supported interface for that, and
//! `obsidian.json` belongs to another running application. So the last step is the user's, and
//! the cockpit asks for it explicitly instead of failing silently the first time someone
//! clicks an `obsidian://` link.

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Serialize;

/// Where Obsidian records the vaults it knows about. Absent until the first vault is opened.
pub fn registry_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join("Library/Application Support/obsidian/obsidian.json")
}

/// The installed app, if it is installed. Both locations are checked because a per-user
/// install is as valid as a system one.
pub fn app_path() -> Option<PathBuf> {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    [
        PathBuf::from("/Applications/Obsidian.app"),
        PathBuf::from(&home).join("Applications/Obsidian.app"),
    ]
    .into_iter()
    .find(|path| path.exists())
}

/// Whether Obsidian already knows this folder as a vault.
///
/// Paths are compared canonicalised: the registry stores what the user picked in the file
/// dialog, which on macOS is frequently `/private/var/...` where the config says `/var/...`.
pub async fn registered(vault: &Path) -> Result<bool> {
    let path = registry_path();
    let text = match tokio::fs::read_to_string(&path).await {
        Ok(text) => text,
        // No registry means Obsidian has never opened a vault. Not an error: it is exactly
        // the state a fresh install is in, and the cockpit has something to say about it.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    let registry: serde_json::Value = serde_json::from_str(&text)?;
    let target = vault.canonicalize().unwrap_or_else(|_| vault.to_path_buf());
    let Some(vaults) = registry.get("vaults").and_then(|v| v.as_object()) else {
        return Ok(false);
    };
    Ok(vaults.values().any(|entry| {
        entry
            .get("path")
            .and_then(|p| p.as_str())
            .map(|p| {
                let known = PathBuf::from(p);
                known.canonicalize().unwrap_or(known) == target
            })
            .unwrap_or(false)
    }))
}

/// `.obsidian/app.json`, which is what makes Obsidian agree with the notes.
///
/// Only the settings the vault's own contents depend on are written. `PURPOSE.md` links a
/// skill to its patterns with `[[shortest-name]]`, so Markdown links and absolute link
/// formats would both break the graph the wiki is meant to be read through.
fn app_settings() -> serde_json::Value {
    serde_json::json!({
        "alwaysUpdateLinks": true,
        "useMarkdownLinks": false,
        "newLinkFormat": "shortest",
        "attachmentFolderPath": "raw/_attachments",
        // The curators write through the daemon, and raw notes are 0444. Nothing in here
        // should be edited by hand while a run is in progress.
        "showUnsupportedFiles": true,
    })
}

pub fn is_prepared(vault: &Path) -> bool {
    vault.join(".obsidian/app.json").is_file()
}

/// Writes the vault's Obsidian configuration. Idempotent, and it never overwrites settings a
/// user has already changed: only missing keys are filled in.
pub async fn prepare(vault: &Path) -> Result<Vec<String>> {
    let dir = vault.join(".obsidian");
    tokio::fs::create_dir_all(&dir).await?;
    let path = dir.join("app.json");

    let mut existing: serde_json::Map<String, serde_json::Value> =
        match tokio::fs::read_to_string(&path).await {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::Map::new(),
            Err(e) => return Err(e.into()),
        };

    let mut written = Vec::new();
    for (key, value) in app_settings().as_object().expect("literal object") {
        if !existing.contains_key(key) {
            existing.insert(key.clone(), value.clone());
            written.push(key.clone());
        }
    }
    let body = serde_json::to_vec_pretty(&serde_json::Value::Object(existing))?;
    tokio::fs::write(&path, body).await?;
    Ok(written)
}

/// What the cockpit's Setup tab renders.
#[derive(Debug, Clone, Serialize)]
pub struct Setup {
    pub installed: bool,
    pub app_path: Option<String>,
    pub vault_path: String,
    /// The name an `obsidian://open?vault=…` link uses — the folder's own name.
    pub vault_name: String,
    /// Obsidian has this folder in its vault list, so the cockpit's links will work.
    pub registered: bool,
    /// `.obsidian/app.json` is in place.
    pub prepared: bool,
    pub open_link: String,
    /// The one thing the daemon cannot do for the user, spelled out. Empty once it is done.
    pub steps: Vec<String>,
}

pub async fn setup(vault: &Path, open_link: String) -> Result<Setup> {
    let app = app_path();
    let registered = registered(vault).await?;
    let vault_path = vault.display().to_string();
    let vault_name = vault
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();

    let mut steps = Vec::new();
    if app.is_none() {
        steps.push("Install Obsidian from obsidian.md — the vault is readable without it, but the links in this window need it.".into());
    } else if !registered {
        steps.push("Open Obsidian.".into());
        steps.push(
            "In the vault switcher (the icon at the bottom of the left sidebar), choose \
             \"Open folder as vault\"."
                .into(),
        );
        steps.push(format!("Select {vault_path}."));
        steps.push(
            "Come back and press Re-check. Obsidian only records a vault once you have opened \
             it, so this window cannot do it for you."
                .into(),
        );
    }

    Ok(Setup {
        installed: app.is_some(),
        app_path: app.map(|p| p.display().to_string()),
        vault_path,
        vault_name,
        registered,
        prepared: is_prepared(vault),
        open_link,
        steps,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn preparing_a_vault_writes_wikilink_settings() {
        let dir = tempfile::tempdir().unwrap();
        let written = prepare(dir.path()).await.unwrap();
        assert!(written.contains(&"useMarkdownLinks".to_string()));

        let text = tokio::fs::read_to_string(dir.path().join(".obsidian/app.json"))
            .await
            .unwrap();
        let settings: serde_json::Value = serde_json::from_str(&text).unwrap();
        // The wiki links patterns to skills with [[name]]; Markdown links would break it.
        assert_eq!(settings["useMarkdownLinks"], serde_json::json!(false));
        assert_eq!(settings["newLinkFormat"], serde_json::json!("shortest"));
        assert!(is_prepared(dir.path()));
    }

    #[tokio::test]
    async fn preparing_twice_keeps_a_setting_the_user_changed() {
        let dir = tempfile::tempdir().unwrap();
        prepare(dir.path()).await.unwrap();
        let path = dir.path().join(".obsidian/app.json");
        tokio::fs::write(
            &path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "useMarkdownLinks": false,
                "attachmentFolderPath": "somewhere/else",
                "spellcheck": true,
            }))
            .unwrap(),
        )
        .await
        .unwrap();

        let written = prepare(dir.path()).await.unwrap();
        assert!(
            !written.contains(&"attachmentFolderPath".to_string()),
            "a setting already in the file must not be rewritten: {written:?}"
        );
        let settings: serde_json::Value =
            serde_json::from_str(&tokio::fs::read_to_string(&path).await.unwrap()).unwrap();
        assert_eq!(settings["attachmentFolderPath"], serde_json::json!("somewhere/else"));
        assert_eq!(settings["spellcheck"], serde_json::json!(true));
    }

    /// The two tests below repoint `HOME`, which every test in this process shares, so they
    /// have to take turns and put it back.
    static HOME: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct HomeGuard {
        previous: Option<std::ffi::OsString>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl HomeGuard {
        fn point_at(dir: &Path) -> Self {
            let lock = HOME.lock().unwrap_or_else(|e| e.into_inner());
            let previous = std::env::var_os("HOME");
            std::env::set_var("HOME", dir);
            Self { previous, _lock: lock }
        }
    }

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(previous) => std::env::set_var("HOME", previous),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    #[tokio::test]
    async fn a_missing_registry_means_no_vault_is_registered() {
        // A fresh Obsidian install has no obsidian.json at all. That is a prompt, not a fault.
        let dir = tempfile::tempdir().unwrap();
        let _home = HomeGuard::point_at(dir.path());
        assert!(!registered(dir.path()).await.unwrap());
    }

    #[tokio::test]
    async fn a_registered_vault_is_recognised_through_a_symlinked_home() {
        let dir = tempfile::tempdir().unwrap();
        let vault = dir.path().join("vault");
        tokio::fs::create_dir_all(&vault).await.unwrap();
        let registry = dir.path().join("Library/Application Support/obsidian");
        tokio::fs::create_dir_all(&registry).await.unwrap();

        // Obsidian stores the path the file dialog handed it, which on macOS is the
        // /private/var form of a /var path. Both must match.
        let as_obsidian_sees_it = vault.canonicalize().unwrap();
        tokio::fs::write(
            registry.join("obsidian.json"),
            serde_json::to_vec(&serde_json::json!({
                "vaults": {
                    "abc123": { "path": as_obsidian_sees_it, "ts": 1_700_000_000_000u64 },
                    "def456": { "path": "/Users/someone/Other vault", "ts": 1u64 },
                }
            }))
            .unwrap(),
        )
        .await
        .unwrap();

        let _home = HomeGuard::point_at(dir.path());
        let found = registered(&vault).await.unwrap();
        let other = registered(&dir.path().join("not-a-vault")).await.unwrap();
        assert!(found, "the vault is in the registry and should be recognised");
        assert!(!other);
    }
}
