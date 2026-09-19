//! The task set and its splits.
//!
//! 60 tasks from your own work that a script can check, split 30 train / 15 validation
//! / 15 test with no overlap. Those sizes are the rationale's starting point, not the
//! paper's, so they are checked as warnings rather than hard rules — except the overlap
//! rule, which is fatal: an overlapping split silently invalidates the gate.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::Result;

/// How a verifier turns a finished session into a score from 0 to 1.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Expect {
    /// 1.0 if the check command exits 0, else 0.0.
    ExitZero,
    /// 1.0 if the command's stdout contains this text.
    StdoutContains { text: String },
    /// The command prints `passed/total`; the score is the fraction.
    ///
    /// This is the shape the rationale names: "the fraction of target tests passing".
    Fraction,
    /// The command prints a bare float in 0..=1 on its last line.
    Score,
}

/// One checkable task.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Task {
    /// Stable ID; also the raw note's file stem.
    pub id: String,
    /// What the inference agent is asked to do.
    pub prompt: String,
    /// Directory copied into the rollout work dir before the session starts. The agent
    /// edits the copy, never this template.
    pub workspace: PathBuf,
    /// Program run by the verifier after the session, inside the work dir copy.
    pub check_program: String,
    #[serde(default)]
    pub check_args: Vec<String>,
    pub expect: Expect,
    /// Free-form tags used for stratified sampling of traces.
    #[serde(default)]
    pub tags: Vec<String>,
}

/// Which split a task belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Split {
    Train,
    Val,
    Test,
}

impl Split {
    pub fn file_name(self) -> &'static str {
        match self {
            Split::Train => "train.jsonl",
            Split::Val => "val.jsonl",
            Split::Test => "test.jsonl",
        }
    }

    /// The rationale's starting sizes.
    pub fn intended_size(self) -> usize {
        match self {
            Split::Train => 30,
            Split::Val | Split::Test => 15,
        }
    }
}

/// All three splits, loaded from `eval/`.
#[derive(Debug, Clone, Default)]
pub struct TaskSet {
    pub train: Vec<Task>,
    pub val: Vec<Task>,
    pub test: Vec<Task>,
}

impl TaskSet {
    pub async fn load(eval_dir: &Path) -> Result<Self> {
        let set = Self {
            train: read_jsonl(&eval_dir.join(Split::Train.file_name())).await?,
            val: read_jsonl(&eval_dir.join(Split::Val.file_name())).await?,
            test: read_jsonl(&eval_dir.join(Split::Test.file_name())).await?,
        };
        set.validate()?;
        Ok(set)
    }

    pub fn split(&self, split: Split) -> &[Task] {
        match split {
            Split::Train => &self.train,
            Split::Val => &self.val,
            Split::Test => &self.test,
        }
    }

    pub fn total(&self) -> usize {
        self.train.len() + self.val.len() + self.test.len()
    }

    /// Fatal: duplicate IDs within a split, or any task appearing in two splits.
    /// Warning: split sizes that differ from the plan.
    pub fn validate(&self) -> Result<()> {
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        for (split, tasks) in [
            (Split::Train, &self.train),
            (Split::Val, &self.val),
            (Split::Test, &self.test),
        ] {
            for task in tasks {
                if !seen.insert(task.id.as_str()) {
                    anyhow::bail!(
                        "task `{}` appears in more than one split (found again in {:?}); \
                         splits must not overlap",
                        task.id,
                        split
                    );
                }
                if task.id.trim().is_empty() {
                    anyhow::bail!("a task in {:?} has an empty id", split);
                }
            }
            if tasks.len() != split.intended_size() {
                tracing::warn!(
                    "{:?} split has {} tasks; the plan's starting size is {}",
                    split,
                    tasks.len(),
                    split.intended_size()
                );
            }
        }
        if self.val.is_empty() {
            anyhow::bail!("the validation split is empty; the gate would have nothing to score");
        }
        Ok(())
    }

    /// One validation task moves the score by this many points — the noise figure the
    /// rationale calls out (6.7 points at 15 tasks).
    pub fn val_granularity_points(&self) -> f64 {
        if self.val.is_empty() {
            0.0
        } else {
            100.0 / self.val.len() as f64
        }
    }

    /// Total rollouts a full run will spend, for the budget line in the cockpit:
    /// baseline + iterations × (train + val × runs) + final test.
    pub fn planned_rollouts(&self, iterations: u32, validation_runs: usize) -> usize {
        let per_iter = self.train.len() + self.val.len() * validation_runs;
        self.val.len() + iterations as usize * per_iter + self.test.len()
    }
}

async fn read_jsonl(path: &Path) -> Result<Vec<Task>> {
    let text = match tokio::fs::read_to_string(path).await {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            anyhow::bail!("missing task file {}", path.display())
        }
        Err(e) => return Err(e.into()),
    };
    let mut tasks = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("//") {
            continue;
        }
        let task: Task = serde_json::from_str(line)
            .map_err(|e| anyhow::anyhow!("{}:{}: {e}", path.display(), i + 1))?;
        tasks.push(task);
    }
    Ok(tasks)
}

/// Writes a split back out, one JSON object per line.
pub async fn write_jsonl(path: &Path, tasks: &[Task]) -> Result<()> {
    let mut text = String::new();
    for task in tasks {
        text.push_str(&serde_json::to_string(task)?);
        text.push('\n');
    }
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(path, text).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: &str) -> Task {
        Task {
            id: id.into(),
            prompt: "fix the failing test".into(),
            workspace: PathBuf::from("/tmp/ws"),
            check_program: "/bin/echo".into(),
            check_args: vec!["3/4".into()],
            expect: Expect::Fraction,
            tags: vec![],
        }
    }

    #[test]
    fn overlapping_splits_are_fatal() {
        let set = TaskSet {
            train: vec![task("a")],
            val: vec![task("a")],
            test: vec![],
        };
        let err = set.validate().unwrap_err().to_string();
        assert!(err.contains("more than one split"), "{err}");
    }

    #[test]
    fn empty_validation_split_is_fatal() {
        let set = TaskSet {
            train: vec![task("a")],
            val: vec![],
            test: vec![],
        };
        assert!(set.validate().is_err());
    }

    #[test]
    fn disjoint_splits_pass_and_report_noise() {
        let set = TaskSet {
            train: (0..30).map(|i| task(&format!("t{i}"))).collect(),
            val: (0..15).map(|i| task(&format!("v{i}"))).collect(),
            test: (0..15).map(|i| task(&format!("s{i}"))).collect(),
        };
        set.validate().unwrap();
        assert_eq!(set.total(), 60);
        assert!((set.val_granularity_points() - 6.666).abs() < 0.01);
        // 15 baseline + 8 × (30 + 15) + 15 test = 390.
        assert_eq!(set.planned_rollouts(8, 1), 390);
        assert_eq!(set.planned_rollouts(8, 2), 510);
    }

    #[tokio::test]
    async fn jsonl_round_trips_and_skips_comments() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("train.jsonl");
        write_jsonl(&path, &[task("a"), task("b")]).await.unwrap();
        let mut text = tokio::fs::read_to_string(&path).await.unwrap();
        text.insert_str(0, "// a comment\n\n");
        tokio::fs::write(&path, text).await.unwrap();
        let back = read_jsonl(&path).await.unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(back[1].id, "b");
    }

    #[tokio::test]
    async fn bad_json_reports_the_line_number() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("val.jsonl");
        tokio::fs::write(&path, "{\"id\":\"a\"}\nnot json\n")
            .await
            .unwrap();
        let err = read_jsonl(&path).await.unwrap_err().to_string();
        assert!(err.contains("val.jsonl:1"), "{err}");
    }
}
