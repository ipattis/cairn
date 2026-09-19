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
    /// Shown in the cockpit's Setup tab, so the person pasting a key knows which one.
    pub label: &'static str,
    /// What the daemon does with it, and which component gets to see it.
    pub purpose: &'static str,
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
        label: "Fireworks (deploy model)",
        purpose: "Runs the rollouts. This is the one key `opencode serve` is given, and the \
                  only one that reaches a sandboxed process.",
    },
    SecretSpec {
        service: "wikiskill-bedrock-mantle",
        env: "BEDROCK_MANTLE_API_KEY",
        required_for: Requirement::Curators,
        label: "Bedrock Mantle (curators)",
        purpose: "Runs the Wiki Maintainer and the Skill Proposer, inside the daemon. Not \
                  needed when both curator roles are configured for another endpoint.",
    },
    SecretSpec {
        service: "wikiskill-jev",
        env: "JEV_API_KEY",
        required_for: Requirement::Jev,
        label: "Jev (labelling)",
        purpose: "Labels traces and runs the repeat check. Only read when `jev.enabled` is \
                  true, and traces are redacted before they are sent.",
    },
];

pub fn spec_for(service: &str) -> Option<&'static SecretSpec> {
    SPECS.iter().find(|spec| spec.service == service)
}

/// What the cockpit is told about one credential. Deliberately no value field: this struct
/// is what leaves the daemon, and a key that cannot be read back cannot be exfiltrated
/// through the webview.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CredentialStatus {
    pub service: &'static str,
    pub env: &'static str,
    pub label: &'static str,
    pub purpose: &'static str,
    /// Present in the Keychain.
    pub in_keychain: bool,
    /// Loaded into the daemon's environment, so a run can use it without a restart.
    pub in_daemon_env: bool,
    /// Needed by a run, given the config this daemon loaded.
    pub required: bool,
}

/// Which credentials exist, without reading any of their values out to the caller.
pub async fn status(jev_enabled: bool, curators_need_keychain: bool) -> Result<Vec<CredentialStatus>> {
    let mut out = Vec::new();
    for spec in SPECS {
        out.push(CredentialStatus {
            service: spec.service,
            env: spec.env,
            label: spec.label,
            purpose: spec.purpose,
            in_keychain: read(spec.service).await?.is_some(),
            in_daemon_env: std::env::var_os(spec.env).is_some(),
            required: match spec.required_for {
                Requirement::Rollouts => true,
                Requirement::Curators => curators_need_keychain,
                Requirement::Jev => jev_enabled,
            },
        });
    }
    Ok(out)
}

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

/// Rejects a value that would be stored but never work, before it is written anywhere.
///
/// The length ceiling is not a guess about providers: it is there because the value arrives
/// over the API, and an unbounded string would be written into the Keychain and then into
/// this process's environment.
pub fn validate_value(value: &str) -> Result<()> {
    if value.is_empty() {
        anyhow::bail!("the value is empty; use DELETE to remove a credential instead");
    }
    if value.len() > 4096 {
        anyhow::bail!("the value is {} bytes; API keys are not", value.len());
    }
    if value.chars().any(|c| c.is_control()) {
        anyhow::bail!(
            "the value contains a newline or control character — it looks like a whole file \
             or a shell line was pasted rather than the key itself"
        );
    }
    if value.trim() != value {
        anyhow::bail!("the value has leading or trailing whitespace");
    }
    Ok(())
}

/// Writes one Keychain generic password, replacing an existing item.
///
/// The value goes in on stdin, not as an argument: `security`'s own usage note calls `-w
/// <value>` insecure, and anything in argv is readable by every process on the machine
/// through `ps`. `security` prompts twice, so the value is written twice.
pub async fn write(service: &str, value: &str) -> Result<()> {
    validate_value(value)?;
    let account = std::env::var("USER").unwrap_or_else(|_| "wikiskill".into());
    let mut child = tokio::process::Command::new("/usr/bin/security")
        // `-U` updates in place; without it a second save fails because the item exists.
        .args(["add-generic-password", "-U", "-a", &account, "-s", service, "-w"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    {
        use tokio::io::AsyncWriteExt;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("no stdin for /usr/bin/security"))?;
        stdin.write_all(format!("{value}\n{value}\n").as_bytes()).await?;
        stdin.flush().await?;
    }
    let output = child.wait_with_output().await?;
    if !output.status.success() {
        // The value is never interpolated into an error: errors reach logs and the webview.
        anyhow::bail!(
            "writing keychain item `{service}` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// The CLI's way in: `security` keeps the terminal and does its own hidden double prompt.
///
/// Nothing reads the value in this process, so it is never in this program's memory, its
/// argv, or the user's shell history.
pub async fn write_interactive(service: &str) -> Result<()> {
    let account = std::env::var("USER").unwrap_or_else(|_| "wikiskill".into());
    let status = tokio::process::Command::new("/usr/bin/security")
        .args(["add-generic-password", "-U", "-a", &account, "-s", service, "-w"])
        .status()
        .await?;
    if !status.success() {
        anyhow::bail!("`security` exited with {status}; nothing was stored");
    }
    Ok(())
}

/// Removes one Keychain generic password. Absent is success: the caller asked for it gone.
pub async fn delete(service: &str) -> Result<bool> {
    let output = tokio::process::Command::new("/usr/bin/security")
        .args(["delete-generic-password", "-s", service])
        .output()
        .await?;
    if output.status.success() {
        return Ok(true);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("could not be found") {
        return Ok(false);
    }
    anyhow::bail!("deleting keychain item `{service}`: {}", stderr.trim())
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

/// Mirrors a just-written key into this process's environment, so setting a credential in the
/// cockpit takes effect on the next run rather than after a daemon restart.
///
/// The daemon refuses this while a run is active. Mutating the environment of a running
/// process is only safe because of that: a rollout that had already read the old value would
/// otherwise be scored under a different credential than the one recorded.
pub fn remember_in_env(spec: &SecretSpec, value: &str) {
    std::env::set_var(spec.env, value);
}

pub fn forget_in_env(spec: &SecretSpec) {
    std::env::remove_var(spec.env);
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

    #[test]
    fn a_pasted_file_or_shell_line_is_refused_before_it_is_stored() {
        assert!(validate_value("fw_1a2b3c4d5e6f").is_ok());
        // The mistakes this actually catches, in the order people make them.
        for bad in [
            "",                                    // cleared the field and hit save
            "fw_key\n",                            // copied a whole line
            "export FIREWORKS_API_KEY=fw_key\n",   // copied the shell command
            " fw_key",                             // copied with a leading space
            "fw_key ",
        ] {
            assert!(
                validate_value(bad).is_err(),
                "should have been refused: {bad:?}"
            );
        }
        assert!(validate_value(&"x".repeat(4097)).is_err());
    }

    #[test]
    fn every_spec_is_addressable_by_its_service_name() {
        // The API takes a service name from the caller; an unknown one has to be a 404
        // rather than a Keychain write under an attacker-chosen service.
        for spec in SPECS {
            assert_eq!(spec_for(spec.service).unwrap().env, spec.env);
        }
        assert!(spec_for("wikiskill-not-a-thing").is_none());
        assert!(spec_for("").is_none());
    }

    #[tokio::test]
    async fn an_absent_keychain_item_is_not_an_error() {
        // A service nobody would create: the point is `Ok(None)`, not `Err`.
        assert!(read("wikiskill-does-not-exist-91a2").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn deleting_something_that_is_not_there_is_not_an_error() {
        // The cockpit's Remove button should not fail on a credential that was never set.
        assert_eq!(delete("wikiskill-does-not-exist-91a2").await.unwrap(), false);
    }
}
