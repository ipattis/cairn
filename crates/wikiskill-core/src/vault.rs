//! The vault: an Obsidian folder that is also a git repo. The daemon is the only
//! process that writes to it; Obsidian and the cockpit read it.
//!
//! Layout (from the rationale):
//!
//! ```text
//! raw/iter-NNN/<task>.md      condensed trace + Jev frontmatter
//! raw/_jsonl/                 full transcripts (redacted)
//! wiki/index.md               static catalog, maintainer-owned
//! wiki/logs.md                append-only, maintainer-owned
//! wiki/skill-impact.md        append-only, daemon-owned
//! wiki/patterns/*.md          one pattern per note
//! skills/<name>/SKILL.md      what the agent executes
//! skills/<name>/PURPOSE.md    [[links]] to motivating patterns
//! eval/train.jsonl, eval/val.jsonl, eval/test.jsonl
//! ```

use std::path::{Path, PathBuf};

use crate::Result;

/// Which subtree a writer is allowed to touch. Curator tools resolve real paths and
/// check them against one of these, so bash can no longer route around path rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Area {
    /// Maintainer-owned: `wiki/`.
    Wiki,
    /// Proposer-owned: `skills/`.
    Skills,
    /// Daemon-only: `raw/`.
    Raw,
    /// Daemon-only: `eval/`.
    Eval,
}

impl Area {
    pub fn dir_name(self) -> &'static str {
        match self {
            Area::Wiki => "wiki",
            Area::Skills => "skills",
            Area::Raw => "raw",
            Area::Eval => "eval",
        }
    }
}

/// A skill as it exists on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    /// Folder name under `skills/`.
    pub name: String,
    /// Contents of `SKILL.md`, which is what gets injected.
    pub body: String,
    /// Contents of `PURPOSE.md` if present.
    pub purpose: Option<String>,
}

/// Handle on the vault directory.
#[derive(Debug, Clone)]
pub struct Vault {
    root: PathBuf,
}

impl Vault {
    /// Opens an existing vault, checking the layout is present.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        if !root.is_dir() {
            anyhow::bail!("vault does not exist: {}", root.display());
        }
        let vault = Self { root };
        for area in [Area::Wiki, Area::Skills, Area::Raw, Area::Eval] {
            let dir = vault.area_root(area);
            if !dir.is_dir() {
                anyhow::bail!(
                    "vault is missing {}/ — run `wikiskilld init` first",
                    area.dir_name()
                );
            }
        }
        Ok(vault)
    }

    /// Creates the layout, idempotently, and seeds the maintainer-owned notes.
    pub async fn init(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        for dir in [
            root.join("raw/_jsonl"),
            root.join("wiki/patterns"),
            root.join("skills"),
            root.join("eval"),
        ] {
            tokio::fs::create_dir_all(&dir).await?;
        }
        let vault = Self { root };
        // `git clean -fd -- skills` after a rejected proposal deletes the folder itself
        // once it is empty; a tracked `.gitkeep` keeps the layout intact across rollbacks.
        for keep in ["skills/.gitkeep", "raw/_jsonl/.gitkeep", "eval/.gitkeep"] {
            vault.seed(keep, "").await?;
        }
        vault
            .seed(
                "wiki/index.md",
                "# Wiki index\n\nStatic catalog of patterns. Maintainer-owned.\n",
            )
            .await?;
        vault
            .seed(
                "wiki/logs.md",
                "# Wiki logs\n\nAppend-only. Maintainer-owned.\n",
            )
            .await?;
        vault
            .seed(
                "wiki/skill-impact.md",
                "# Skill impact\n\nAppend-only. Daemon-owned: one entry per gate outcome.\n",
            )
            .await?;
        vault
            .seed(
                ".gitignore",
                // Obsidian's workspace churn should not show up in skill diffs.
                ".obsidian/workspace*\n.DS_Store\n",
            )
            .await?;
        Ok(vault)
    }

    async fn seed(&self, rel: &str, contents: &str) -> Result<()> {
        let path = self.root.join(rel);
        if !path.exists() {
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::write(path, contents).await?;
        }
        Ok(())
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn area_root(&self, area: Area) -> PathBuf {
        self.root.join(area.dir_name())
    }

    pub fn skills_dir(&self) -> PathBuf {
        self.area_root(Area::Skills)
    }

    pub fn wiki_dir(&self) -> PathBuf {
        self.area_root(Area::Wiki)
    }

    pub fn raw_dir(&self) -> PathBuf {
        self.area_root(Area::Raw)
    }

    pub fn eval_dir(&self) -> PathBuf {
        self.area_root(Area::Eval)
    }

    pub fn impact_log(&self) -> PathBuf {
        self.wiki_dir().join("skill-impact.md")
    }

    pub fn wiki_index(&self) -> PathBuf {
        self.wiki_dir().join("index.md")
    }

    /// `raw/iter-NNN/`.
    pub fn raw_iter_dir(&self, iteration: u32) -> PathBuf {
        self.raw_dir().join(format!("iter-{iteration:03}"))
    }

    /// Resolves `rel` inside `area`, refusing anything that escapes the subtree.
    ///
    /// Components are checked *and* the resolved parent is canonicalised, so neither
    /// `../` nor a symlink planted inside the vault can point a curator write outside
    /// its own area.
    pub fn resolve_in(&self, area: Area, rel: &str) -> Result<PathBuf> {
        let rel_path = Path::new(rel);
        if rel_path.is_absolute() {
            anyhow::bail!("path must be relative to {}/: {rel}", area.dir_name());
        }
        for component in rel_path.components() {
            match component {
                std::path::Component::Normal(_) | std::path::Component::CurDir => {}
                _ => anyhow::bail!("path may not traverse upward or use a prefix: {rel}"),
            }
        }
        let area_root = self.area_root(area);
        let real_area_root = area_root
            .canonicalize()
            .map_err(|e| anyhow::anyhow!("resolving {}: {e}", area_root.display()))?;
        let target = real_area_root.join(rel_path);

        // Canonicalise the deepest existing ancestor: the file itself may not exist yet.
        let mut probe = target.clone();
        loop {
            match probe.canonicalize() {
                Ok(real) => {
                    if !real.starts_with(&real_area_root) {
                        anyhow::bail!(
                            "path resolves outside {}/: {rel}",
                            area.dir_name()
                        );
                    }
                    break;
                }
                Err(_) => match probe.parent() {
                    Some(parent) if parent != probe => probe = parent.to_path_buf(),
                    _ => anyhow::bail!("path resolves outside {}/: {rel}", area.dir_name()),
                },
            }
        }
        Ok(target)
    }

    /// Lists active skills in a stable order, so the injected block is byte-identical
    /// across every rollout in an iteration and Fireworks prompt caching can absorb it.
    pub async fn active_skills(&self) -> Result<Vec<Skill>> {
        let dir = self.skills_dir();
        let mut names = Vec::new();
        // No `skills/` at all means no active skills — the empty-skill baseline, and the
        // state a rollback can leave behind.
        let mut entries = match tokio::fs::read_dir(&dir).await {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        while let Some(entry) = entries.next_entry().await? {
            if !entry.file_type().await?.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || name.starts_with('_') {
                continue;
            }
            if entry.path().join("SKILL.md").is_file() {
                names.push(name);
            }
        }
        names.sort();

        let mut skills = Vec::with_capacity(names.len());
        for name in names {
            let folder = dir.join(&name);
            let body = tokio::fs::read_to_string(folder.join("SKILL.md")).await?;
            let purpose = match tokio::fs::read_to_string(folder.join("PURPOSE.md")).await {
                Ok(p) => Some(p),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(e.into()),
            };
            skills.push(Skill {
                name,
                body,
                purpose,
            });
        }
        Ok(skills)
    }

    /// Writes a raw note and marks it read-only, so a curator cannot rewrite history
    /// even if a tool check were bypassed.
    pub async fn write_raw_note(&self, iteration: u32, task_id: &str, body: &str) -> Result<PathBuf> {
        let dir = self.raw_iter_dir(iteration);
        tokio::fs::create_dir_all(&dir).await?;
        let path = dir.join(format!("{}.md", sanitize_file_stem(task_id)));
        write_read_only(&path, body.as_bytes()).await?;
        Ok(path)
    }

    /// Writes a captured daily-coding session to `raw/daily/<date>/<slug>.md`.
    ///
    /// Daily sessions are traces like any other, but they are not iterations: they are
    /// dated, and the nightly maintainer reads a day's worth at a time.
    pub async fn write_daily_note(
        &self,
        date: &str,
        session_id: &str,
        body: &str,
    ) -> Result<PathBuf> {
        let dir = self
            .resolve_in(Area::Raw, &format!("daily/{}", sanitize_file_stem(date)))?;
        tokio::fs::create_dir_all(&dir).await?;
        let path = dir.join(format!("{}.md", sanitize_file_stem(session_id)));
        write_read_only(&path, body.as_bytes()).await?;
        Ok(path)
    }

    /// Daily notes for one date, oldest first. Empty when nothing was captured.
    pub async fn daily_notes(&self, date: &str) -> Result<Vec<PathBuf>> {
        let dir = self
            .resolve_in(Area::Raw, &format!("daily/{}", sanitize_file_stem(date)))?;
        let mut entries = match tokio::fs::read_dir(&dir).await {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut paths = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("md") {
                paths.push(path);
            }
        }
        paths.sort();
        Ok(paths)
    }

    /// Writes a full (redacted) transcript to `raw/_jsonl/`, also read-only.
    pub async fn write_raw_jsonl(
        &self,
        iteration: u32,
        task_id: &str,
        jsonl: &str,
    ) -> Result<PathBuf> {
        let dir = self.raw_dir().join("_jsonl");
        tokio::fs::create_dir_all(&dir).await?;
        let path = dir.join(format!(
            "iter-{iteration:03}.{}.jsonl",
            sanitize_file_stem(task_id)
        ));
        write_read_only(&path, jsonl.as_bytes()).await?;
        Ok(path)
    }

    /// Appends to an append-only note under `wiki/`.
    pub async fn append_wiki(&self, rel: &str, block: &str) -> Result<()> {
        use tokio::io::AsyncWriteExt;
        let path = self.resolve_in(Area::Wiki, rel)?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;
        let mut text = String::new();
        if !block.starts_with('\n') {
            text.push('\n');
        }
        text.push_str(block);
        if !block.ends_with('\n') {
            text.push('\n');
        }
        file.write_all(text.as_bytes()).await?;
        file.flush().await?;
        Ok(())
    }

    /// Vault-relative form of an absolute path inside the vault.
    ///
    /// Paths that came back from [`Self::resolve_in`] are canonicalised, and on macOS the
    /// vault root often is not (`/var/...` vs `/private/var/...`), so a plain `strip_prefix`
    /// against the configured root silently fails and leaves an absolute path — which then
    /// produces a broken Obsidian link and a path no curator tool will accept.
    pub fn relative(&self, path: &Path) -> String {
        for root in [
            self.root.clone(),
            self.root.canonicalize().unwrap_or_else(|_| self.root.clone()),
        ] {
            if let Ok(rest) = path.strip_prefix(&root) {
                return rest.to_string_lossy().to_string();
            }
        }
        path.to_string_lossy().to_string()
    }

    /// `obsidian://open` link for a vault-relative path, used by the cockpit.
    pub fn obsidian_link(&self, rel: &str) -> String {
        let vault_name = self
            .root
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "WikiSkill".into());
        format!(
            "obsidian://open?vault={}&file={}",
            urlencode(&vault_name),
            urlencode(rel)
        )
    }
}

/// Writes a file and drops the owner write bit (0444).
pub async fn write_read_only(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    // Re-writing an existing read-only file needs the write bit back first.
    if path.exists() {
        let mut perms = tokio::fs::metadata(path).await?.permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            perms.set_mode(0o644);
        }
        #[cfg(not(unix))]
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        tokio::fs::set_permissions(path, perms).await?;
    }
    tokio::fs::write(path, bytes).await?;
    let mut perms = tokio::fs::metadata(path).await?.permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(0o444);
    }
    #[cfg(not(unix))]
    perms.set_readonly(true);
    tokio::fs::set_permissions(path, perms).await?;
    Ok(())
}

/// Keeps task IDs usable as file names without losing readability in Obsidian.
pub fn sanitize_file_stem(id: &str) -> String {
    let mut out = String::with_capacity(id.len());
    for ch in id.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.' {
            out.push(ch);
        } else {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "task".into()
    } else {
        trimmed
    }
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            b'/' => out.push_str("%2F"),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn vault() -> (tempfile::TempDir, Vault) {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::init(dir.path().join("WikiSkill")).await.unwrap();
        (dir, vault)
    }

    #[tokio::test]
    async fn init_is_idempotent_and_open_checks_layout() {
        let (dir, _v) = vault().await;
        let again = Vault::init(dir.path().join("WikiSkill")).await.unwrap();
        assert!(again.impact_log().is_file());
        Vault::open(dir.path().join("WikiSkill")).unwrap();
        assert!(Vault::open(dir.path().join("nope")).is_err());
    }

    #[tokio::test]
    async fn resolve_in_refuses_escapes() {
        let (_d, v) = vault().await;
        assert!(v.resolve_in(Area::Wiki, "../skills/x/SKILL.md").is_err());
        assert!(v.resolve_in(Area::Wiki, "/etc/passwd").is_err());
        assert!(v.resolve_in(Area::Wiki, "patterns/ok.md").is_ok());
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn resolve_in_refuses_symlink_escape() {
        let (_d, v) = vault().await;
        let outside = _d.path().join("outside");
        tokio::fs::create_dir_all(&outside).await.unwrap();
        std::os::unix::fs::symlink(&outside, v.wiki_dir().join("escape")).unwrap();
        assert!(v.resolve_in(Area::Wiki, "escape/evil.md").is_err());
    }

    #[tokio::test]
    async fn active_skills_are_sorted_and_need_skill_md() {
        let (_d, v) = vault().await;
        for name in ["zeta", "alpha"] {
            let folder = v.skills_dir().join(name);
            tokio::fs::create_dir_all(&folder).await.unwrap();
            tokio::fs::write(folder.join("SKILL.md"), format!("# {name}\n"))
                .await
                .unwrap();
        }
        tokio::fs::create_dir_all(v.skills_dir().join("draft-no-skill-md"))
            .await
            .unwrap();
        let skills = v.active_skills().await.unwrap();
        assert_eq!(
            skills.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            vec!["alpha", "zeta"]
        );
    }

    #[tokio::test]
    async fn raw_notes_are_read_only() {
        let (_d, v) = vault().await;
        let path = v.write_raw_note(4, "fix/flaky test", "body").await.unwrap();
        assert!(path.ends_with("iter-004/fix-flaky-test.md"));
        let meta = tokio::fs::metadata(&path).await.unwrap();
        assert!(meta.permissions().readonly());
        // The daemon can still overwrite deliberately.
        v.write_raw_note(4, "fix/flaky test", "body2").await.unwrap();
        assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), "body2");
    }

    #[tokio::test]
    async fn append_wiki_keeps_one_block_per_entry() {
        let (_d, v) = vault().await;
        v.append_wiki("skill-impact.md", "### a").await.unwrap();
        v.append_wiki("skill-impact.md", "### b").await.unwrap();
        let text = tokio::fs::read_to_string(v.impact_log()).await.unwrap();
        assert!(text.contains("\n### a\n\n### b\n"), "{text}");
    }

    /// `resolve_in` canonicalises, and a tempdir root does not, so this is the case that
    /// bit: a daily note whose path came back canonical must still be vault-relative.
    #[tokio::test]
    async fn resolved_paths_come_back_vault_relative() {
        let (_d, v) = vault().await;
        let path = v.write_daily_note("2026-09-19", "ses_demo", "body").await.unwrap();
        assert_eq!(v.relative(&path), "raw/daily/2026-09-19/ses_demo.md");
        assert_eq!(v.daily_notes("2026-09-19").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn obsidian_links_are_escaped() {
        let (_d, v) = vault().await;
        let link = v.obsidian_link("wiki/patterns/loop breaker.md");
        assert_eq!(
            link,
            "obsidian://open?vault=WikiSkill&file=wiki%2Fpatterns%2Floop%20breaker.md"
        );
    }
}
