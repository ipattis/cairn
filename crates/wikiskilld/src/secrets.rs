//! Provider credentials from the macOS Keychain.
//!
//! The threat the rationale names is a prompt-injected rollout, or a compromised webview,
//! reading keys. The control is that keys live in the Keychain, are loaded into the
//! *daemon's* environment only, and are never handed to the webview or to a sandboxed
//! rollout: `opencode serve` gets only the keys rollouts need, and the Seatbelt profile
//! denies it the vault and everything else it has no business reading.

use std::collections::BTreeMap;

use anyhow::Result;

/// One Keychain item mapped to the environment variable the core crate reads.
#[derive(Debug, Clone, Copy)]
pub struct SecretSpec {
    /// `security -s` service name.
    pub service: &'static str,
    pub env: &'static str,
    /// A run cannot start without it (given the roles that are configured).
    pub required_for: Requirement,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Requirement {
    /// Needed by rollouts, so needed by `opencode serve` too.
    Rollouts,
    /// Needed by the curator roles in the daemon.
    Curators,
    /// Only needed when Jev is enabled.
    Jev,
}

pub const SPECS: &[SecretSpec] = &[
    SecretSpec {
        service: "wikiskill-fireworks",
        env: "FIREWORKS_API_KEY",
        required_for: Requirement::Rollouts,
    },
    SecretSpec {
        service: "wikiskill-bedrock-mantle",
        env: "BEDROCK_MANTLE_API_KEY",
        required_for: Requirement::Curators,
    },
    SecretSpec {
        service: "wikiskill-jev",
        env: "JEV_API_KEY",
        required_for: Requirement::Jev,
    },
];

/// Reads one Keychain generic password. `security` prompts on first access and then
/// remembers the grant, which is why the daemon reads every key once at startup rather
/// than mid-run.
pub async fn read(service: &str) -> Result<Option<String>> {
    let output = tokio::process::Command::new("/usr/bin/security")
        .args(["find-generic-password", "-s", service, "-w"])
        .output()
        .await;
    let output = match output {
        Ok(output) => output,
        // Non-macOS hosts have no `security`; treat it as "no keys configured".
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("could not be found") {
            return Ok(None);
        }
        anyhow::bail!("reading keychain item `{service}`: {}", stderr.trim());
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(if value.is_empty() { None } else { Some(value) })
}

/// Loads every configured key into this process's environment and reports which were
/// found. The values are deliberately not returned: nothing but the environment should
/// hold them, so they cannot end up in a log line or an API response by accident.
pub async fn load_into_env() -> Result<BTreeMap<&'static str, bool>> {
    let mut found = BTreeMap::new();
    for spec in SPECS {
        // An already-set variable wins, so a developer can run the daemon from a shell
        // without touching the Keychain.
        if std::env::var_os(spec.env).is_some() {
            found.insert(spec.env, true);
            continue;
        }
        match read(spec.service).await? {
            Some(value) => {
                // SAFETY-equivalent note: done once, at startup, before any task spawns.
                std::env::set_var(spec.env, value);
                found.insert(spec.env, true);
            }
            None => {
                found.insert(spec.env, false);
            }
        }
    }
    Ok(found)
}

/// The subset the rollout server may see. `opencode serve` runs rollouts on the deploy
/// model, so it needs the Fireworks key and nothing else — not the curator credentials,
/// not the Jev key.
pub fn rollout_env() -> Vec<(String, String)> {
    SPECS
        .iter()
        .filter(|spec| spec.required_for == Requirement::Rollouts)
        .filter_map(|spec| std::env::var(spec.env).ok().map(|v| (spec.env.to_string(), v)))
        .collect()
}

/// Names of the keys a run needs but does not have.
pub fn missing_for(jev_enabled: bool, curators_need_keychain: bool) -> Vec<&'static str> {
    SPECS
        .iter()
        .filter(|spec| match spec.required_for {
            Requirement::Rollouts => true,
            Requirement::Curators => curators_need_keychain,
            Requirement::Jev => jev_enabled,
        })
        .filter(|spec| std::env::var_os(spec.env).is_none())
        .map(|spec| spec.env)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both tests below mutate this process's environment, and cargo runs tests in threads
    /// of one process — so they have to take turns or one sees the other's `JEV_API_KEY`.
    static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn only_rollout_keys_reach_the_rollout_server() {
        let _guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("FIREWORKS_API_KEY", "fw_rollout");
        std::env::set_var("JEV_API_KEY", "ts_secret");
        let env = rollout_env();
        assert!(env.iter().any(|(k, _)| k == "FIREWORKS_API_KEY"));
        assert!(
            !env.iter().any(|(k, _)| k == "JEV_API_KEY"),
            "the rollout server must not receive the Jev key: {env:?}"
        );
        std::env::remove_var("JEV_API_KEY");
    }

    #[test]
    fn missing_keys_depend_on_what_is_enabled() {
        let _guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("FIREWORKS_API_KEY", "fw");
        std::env::remove_var("JEV_API_KEY");
        assert!(missing_for(false, false).is_empty());
        assert_eq!(missing_for(true, false), vec!["JEV_API_KEY"]);
    }

    #[tokio::test]
    async fn an_absent_keychain_item_is_not_an_error() {
        // A service nobody would create: the point is `Ok(None)`, not `Err`.
        assert!(read("wikiskill-does-not-exist-91a2").await.unwrap().is_none());
    }
}
