//! The launchd user agent that keeps the daemon alive.
//!
//! A user agent, not a daemon in `/Library`: everything it touches — the vault, the
//! Keychain, `~/Library/Application Support` — belongs to the logged-in user, and a
//! LaunchDaemon would run as root with no Keychain access and no reason to.

use std::path::{Path, PathBuf};

use anyhow::Result;

pub const LABEL: &str = "ai.wikiskill.wikiskilld";

pub fn plist_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::new()
        .join(home)
        .join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist"))
}

/// `KeepAlive` with `SuccessfulExit: false` restarts the daemon if it crashes but respects
/// a clean shutdown, so `wikiskilld` can be stopped without launchd fighting the user.
pub fn render(binary: &Path, config: &Path, log_dir: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{binary}</string>
    <string>--config</string>
    <string>{config}</string>
    <string>serve</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
  </dict>
  <key>ProcessType</key>
  <string>Background</string>
  <key>EnvironmentVariables</key>
  <dict>
    <key>RUST_LOG</key>
    <string>info</string>
  </dict>
  <key>StandardOutPath</key>
  <string>{log_dir}/wikiskilld.log</string>
  <key>StandardErrorPath</key>
  <string>{log_dir}/wikiskilld.err.log</string>
</dict>
</plist>
"#,
        binary = escape(&binary.display().to_string()),
        config = escape(&config.display().to_string()),
        log_dir = escape(&log_dir.display().to_string()),
    )
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub async fn install(binary: &Path, config: &Path, log_dir: &Path) -> Result<PathBuf> {
    let path = plist_path();
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::create_dir_all(log_dir).await?;
    tokio::fs::write(&path, render(binary, config, log_dir)).await?;

    // `bootout` first so reinstalling picks up an edited plist; it fails when nothing is
    // loaded, which is fine.
    let uid = users_uid().await?;
    let _ = run_launchctl(&["bootout", &format!("gui/{uid}/{LABEL}")]).await;
    run_launchctl(&["bootstrap", &format!("gui/{uid}"), &path.display().to_string()]).await?;
    Ok(path)
}

pub async fn uninstall() -> Result<()> {
    let uid = users_uid().await?;
    let _ = run_launchctl(&["bootout", &format!("gui/{uid}/{LABEL}")]).await;
    let path = plist_path();
    if path.exists() {
        tokio::fs::remove_file(path).await?;
    }
    Ok(())
}

async fn users_uid() -> Result<u32> {
    let output = tokio::process::Command::new("/usr/bin/id")
        .arg("-u")
        .output()
        .await?;
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .map_err(|e| anyhow::anyhow!("reading the current uid: {e}"))
}

async fn run_launchctl(args: &[&str]) -> Result<()> {
    let output = tokio::process::Command::new("/bin/launchctl")
        .args(args)
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("running launchctl {}: {e}", args.join(" ")))?;
    if !output.status.success() {
        anyhow::bail!(
            "launchctl {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_plist_points_at_the_configured_binary_and_config() {
        let text = render(
            Path::new("/usr/local/bin/wikiskilld"),
            Path::new("/Users/x/Library/Application Support/wikiskill/config.json"),
            Path::new("/Users/x/Library/Logs/wikiskill"),
        );
        assert!(text.contains("<string>ai.wikiskill.wikiskilld</string>"));
        assert!(text.contains("<string>/usr/local/bin/wikiskilld</string>"));
        assert!(text.contains("wikiskill/config.json</string>"));
        assert!(text.contains("<string>serve</string>"));
        // A crash restarts; a clean exit does not.
        assert!(text.contains("<key>SuccessfulExit</key>\n    <false/>"));
    }

    #[test]
    fn paths_with_xml_metacharacters_are_escaped() {
        let text = render(
            Path::new("/tmp/a&b"),
            Path::new("/tmp/<c>"),
            Path::new("/tmp/logs"),
        );
        assert!(text.contains("/tmp/a&amp;b"));
        assert!(text.contains("/tmp/&lt;c&gt;"));
    }
}
