//! Git over the vault: commit on accept, `git restore skills/` on reject.
//!
//! This drives the `git` CLI rather than linking `gix`/`git2`. The rationale lists
//! those as candidates to confirm, and the daemon needs exactly five operations, all
//! of which are plumbing-stable; the CLI keeps the build free of a C dependency and
//! matches whatever the user's Obsidian-side git tooling does.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::Result;

#[derive(Debug, Clone)]
pub struct Repo {
    root: PathBuf,
}

impl Repo {
    pub fn open(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    async fn run(&self, args: &[&str]) -> Result<String> {
        let output = tokio::process::Command::new("git")
            .args(args)
            .current_dir(&self.root)
            .output()
            .await
            .map_err(|e| anyhow::anyhow!("spawning git {}: {e}", args.join(" ")))?;
        if !output.status.success() {
            anyhow::bail!(
                "git {} failed ({}): {}",
                args.join(" "),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    pub async fn is_repo(&self) -> bool {
        self.run(&["rev-parse", "--git-dir"]).await.is_ok()
    }

    /// Initialises the vault repo if needed and makes a first commit.
    pub async fn init(&self) -> Result<()> {
        if self.is_repo().await {
            return Ok(());
        }
        self.run(&["init", "--quiet"]).await?;
        self.run(&["add", "-A"]).await?;
        // `commit` fails on an empty tree; tolerate that.
        let _ = self
            .run(&["commit", "-q", "-m", "wikiskill: initialise vault"])
            .await;
        Ok(())
    }

    /// Unified diff of `skills/`, including untracked files so a brand-new skill
    /// folder shows up.
    pub async fn skills_diff(&self) -> Result<String> {
        // Stage into the index (without committing) so untracked files are visible to
        // `git diff --cached`; the caller either commits or restores afterwards.
        self.run(&["add", "-A", "--", "skills"]).await?;
        self.run(&["diff", "--cached", "--", "skills"]).await
    }

    /// Skill folder names touched by the current (staged) `skills/` diff.
    pub async fn changed_skill_folders(&self) -> Result<BTreeSet<String>> {
        self.run(&["add", "-A", "--", "skills"]).await?;
        let out = self
            .run(&["diff", "--cached", "--name-only", "--", "skills"])
            .await?;
        let mut folders = BTreeSet::new();
        for line in out.lines().filter(|l| !l.trim().is_empty()) {
            let mut parts = line.split('/');
            if parts.next() != Some("skills") {
                continue;
            }
            if let Some(name) = parts.next() {
                folders.insert(name.to_string());
            }
        }
        Ok(folders)
    }

    /// Accept: stage everything the iteration produced and commit it.
    pub async fn commit_all(&self, message: &str) -> Result<Option<String>> {
        self.run(&["add", "-A"]).await?;
        let staged = self.run(&["diff", "--cached", "--name-only"]).await?;
        if staged.trim().is_empty() {
            return Ok(None);
        }
        self.run(&["commit", "-q", "-m", message]).await?;
        Ok(Some(self.run(&["rev-parse", "HEAD"]).await?.trim().to_string()))
    }

    /// Reject: roll `skills/` back to HEAD and drop untracked skill files. The wiki
    /// and raw notes are left alone — the paper's rule is that skills roll back while
    /// the wiki persists — and they are committed so the rollback is not lost.
    pub async fn restore_skills(&self) -> Result<()> {
        self.run(&["reset", "-q", "--", "skills"]).await.ok();
        self.run(&["checkout", "--", "skills"]).await.ok();
        self.run(&["clean", "-qfd", "--", "skills"]).await?;
        Ok(())
    }

    /// Drops `skills/` from the index without touching the working tree.
    ///
    /// [`Self::skills_diff`] stages `skills/` so untracked files show up in the diff. On
    /// the reject path the wiki and raw notes are committed *before* the rollback, and a
    /// commit takes the whole index — so the staged proposal has to come out of the index
    /// first, or it would be committed and survive the rollback.
    pub async fn unstage_skills(&self) -> Result<()> {
        self.run(&["reset", "-q", "--", "skills"]).await.ok();
        Ok(())
    }

    /// Commit path: used after a rejection so wiki and raw changes still land.
    pub async fn commit_paths(&self, paths: &[&str], message: &str) -> Result<Option<String>> {
        // `git add` fails outright on a pathspec that matches nothing, and a vault can
        // legitimately lack an area (no `eval/` before a task set is seeded).
        let paths: Vec<&str> = paths
            .iter()
            .copied()
            .filter(|p| self.root.join(p).exists())
            .collect();
        if paths.is_empty() {
            return Ok(None);
        }
        let mut args = vec!["add", "-A", "--"];
        args.extend_from_slice(&paths);
        self.run(&args).await?;
        let mut diff_args = vec!["diff", "--cached", "--name-only", "--"];
        diff_args.extend_from_slice(&paths);
        if self.run(&diff_args).await?.trim().is_empty() {
            return Ok(None);
        }
        self.run(&["commit", "-q", "-m", message]).await?;
        Ok(Some(self.run(&["rev-parse", "HEAD"]).await?.trim().to_string()))
    }

    /// Applies a unified diff to the working tree.
    ///
    /// Used to replay a reviewed proposal that was rolled back when it was captured: the
    /// diff, not the model, is the thing a human approved, so re-running the proposer would
    /// be applying something else.
    pub async fn apply_patch(&self, diff: &str) -> Result<()> {
        use tokio::io::AsyncWriteExt;
        let mut child = tokio::process::Command::new("git")
            .args(["apply", "--index", "--whitespace=nowarn", "-"])
            .current_dir(&self.root)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        child
            .stdin
            .as_mut()
            .expect("stdin is piped")
            .write_all(diff.as_bytes())
            .await?;
        // Drop stdin so `git apply` sees EOF.
        drop(child.stdin.take());
        let output = child.wait_with_output().await?;
        if !output.status.success() {
            anyhow::bail!(
                "git apply failed ({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    }

    /// Commits that touched a given skill folder, newest first — backs the cockpit's
    /// "revert to an earlier accepted version" action.
    pub async fn skill_history(&self, skill: &str) -> Result<Vec<(String, String)>> {
        let pathspec = format!("skills/{skill}");
        let out = self
            .run(&[
                "log",
                "--format=%H%x1f%s",
                "--",
                pathspec.as_str(),
            ])
            .await?;
        Ok(out
            .lines()
            .filter_map(|line| line.split_once('\u{1f}'))
            .map(|(h, s)| (h.to_string(), s.to_string()))
            .collect())
    }

    /// Restores one skill folder from an earlier commit.
    pub async fn revert_skill_to(&self, skill: &str, commit: &str) -> Result<()> {
        let pathspec = format!("skills/{skill}");
        self.run(&["checkout", commit, "--", pathspec.as_str()])
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn repo() -> (tempfile::TempDir, Repo) {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::open(dir.path());
        repo.init().await.unwrap();
        // Keep the test independent of the machine's git identity.
        repo.run(&["config", "user.email", "test@example.invalid"])
            .await
            .unwrap();
        repo.run(&["config", "user.name", "wikiskill test"])
            .await
            .unwrap();
        (dir, repo)
    }

    async fn write(root: &Path, rel: &str, body: &str) {
        let path = root.join(rel);
        tokio::fs::create_dir_all(path.parent().unwrap()).await.unwrap();
        tokio::fs::write(path, body).await.unwrap();
    }

    #[tokio::test]
    async fn diff_sees_new_skill_folders() {
        let (dir, repo) = repo().await;
        write(dir.path(), "skills/loop-breaker/SKILL.md", "# v1\n").await;
        let diff = repo.skills_diff().await.unwrap();
        assert!(diff.contains("skills/loop-breaker/SKILL.md"), "{diff}");
        assert_eq!(
            repo.changed_skill_folders().await.unwrap(),
            BTreeSet::from(["loop-breaker".to_string()])
        );
    }

    #[tokio::test]
    async fn restore_drops_a_rejected_proposal_but_keeps_the_wiki() {
        let (dir, repo) = repo().await;
        write(dir.path(), "skills/keep/SKILL.md", "# keep\n").await;
        write(dir.path(), "wiki/logs.md", "# logs\n").await;
        repo.commit_all("accept keep").await.unwrap();

        write(dir.path(), "skills/keep/SKILL.md", "# tampered\n").await;
        write(dir.path(), "skills/rejected/SKILL.md", "# new\n").await;
        write(dir.path(), "wiki/logs.md", "# logs\n- pattern seen\n").await;

        repo.restore_skills().await.unwrap();

        assert_eq!(
            tokio::fs::read_to_string(dir.path().join("skills/keep/SKILL.md"))
                .await
                .unwrap(),
            "# keep\n"
        );
        assert!(!dir.path().join("skills/rejected").exists());
        assert!(
            tokio::fs::read_to_string(dir.path().join("wiki/logs.md"))
                .await
                .unwrap()
                .contains("pattern seen")
        );
    }

    /// The reject path's exact order: diff (which stages skills), commit the wiki, roll
    /// back. Without `unstage_skills` the wiki commit takes the staged proposal with it
    /// and the rollback restores the proposal instead of dropping it.
    #[tokio::test]
    async fn committing_the_wiki_before_a_rollback_does_not_preserve_the_proposal() {
        let (dir, repo) = repo().await;
        write(dir.path(), "wiki/logs.md", "# logs\n").await;
        repo.commit_all("baseline").await.unwrap();

        write(dir.path(), "skills/rejected/SKILL.md", "# new\n").await;
        write(dir.path(), "wiki/logs.md", "# logs\n- pattern seen\n").await;
        let diff = repo.skills_diff().await.unwrap();
        assert!(diff.contains("skills/rejected/SKILL.md"));

        repo.unstage_skills().await.unwrap();
        repo.commit_paths(&["wiki", "raw", "eval"], "wiki only")
            .await
            .unwrap();
        repo.restore_skills().await.unwrap();

        assert!(!dir.path().join("skills/rejected").exists());
        assert!(
            tokio::fs::read_to_string(dir.path().join("wiki/logs.md"))
                .await
                .unwrap()
                .contains("pattern seen")
        );
        // And the proposal is in no commit.
        let log = repo.run(&["log", "--name-only", "--format="]).await.unwrap();
        assert!(!log.contains("skills/rejected"), "{log}");
    }

    #[tokio::test]
    async fn multi_folder_proposals_are_detectable() {
        let (dir, repo) = repo().await;
        write(dir.path(), "skills/a/SKILL.md", "# a\n").await;
        write(dir.path(), "skills/b/SKILL.md", "# b\n").await;
        assert_eq!(repo.changed_skill_folders().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn history_and_revert_round_trip() {
        let (dir, repo) = repo().await;
        write(dir.path(), "skills/s/SKILL.md", "# v1\n").await;
        repo.commit_all("iter-000 accept").await.unwrap();
        write(dir.path(), "skills/s/SKILL.md", "# v2\n").await;
        repo.commit_all("iter-001 accept").await.unwrap();

        let history = repo.skill_history("s").await.unwrap();
        assert_eq!(history.len(), 2);
        let (oldest, _) = history.last().unwrap();
        repo.revert_skill_to("s", oldest).await.unwrap();
        assert_eq!(
            tokio::fs::read_to_string(dir.path().join("skills/s/SKILL.md"))
                .await
                .unwrap(),
            "# v1\n"
        );
    }

    #[tokio::test]
    async fn a_rolled_back_proposal_can_be_replayed_from_its_diff() {
        let (dir, repo) = repo().await;
        write(dir.path(), "wiki/logs.md", "# logs\n").await;
        repo.commit_all("baseline").await.unwrap();

        write(dir.path(), "skills/proposed/SKILL.md", "# proposed\n").await;
        let diff = repo.skills_diff().await.unwrap();
        repo.unstage_skills().await.unwrap();
        repo.restore_skills().await.unwrap();
        assert!(!dir.path().join("skills/proposed").exists());

        repo.apply_patch(&diff).await.unwrap();
        assert_eq!(
            tokio::fs::read_to_string(dir.path().join("skills/proposed/SKILL.md"))
                .await
                .unwrap(),
            "# proposed\n"
        );
        assert!(repo.commit_all("accept after review").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn a_patch_that_does_not_apply_is_an_error() {
        let (_d, repo) = repo().await;
        let err = repo
            .apply_patch("diff --git a/skills/x/SKILL.md b/skills/x/SKILL.md\nnonsense\n")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("git apply failed"), "{err}");
    }

    #[tokio::test]
    async fn commit_all_is_a_noop_on_a_clean_tree() {
        let (_d, repo) = repo().await;
        assert!(repo.commit_all("nothing").await.unwrap().is_none());
    }
}
