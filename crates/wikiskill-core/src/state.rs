//! Run state on disk, under `~/Library/Application Support/wikiskill/`.
//!
//! An eight-iteration run takes hours, so runs and schedules must survive closing the
//! app. Nothing here depends on the GUI: the daemon owns it and the cockpit reads it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::gate::{Decision, RepeatCheck};
use crate::model::Usage;
use crate::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Queued,
    Running,
    /// Paused from the cockpit; the daemon stops at the next iteration boundary.
    Paused,
    Completed,
    /// Stopped early because validation reached 100%.
    CompletedPerfect,
    Failed,
    Cancelled,
}

impl RunStatus {
    pub fn terminal(self) -> bool {
        matches!(
            self,
            RunStatus::Completed
                | RunStatus::CompletedPerfect
                | RunStatus::Failed
                | RunStatus::Cancelled
        )
    }
}

/// Which role spent which tokens, for the Runs view's cost column.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleUsage {
    pub inference: Usage,
    pub maintainer: Usage,
    pub proposer: Usage,
}

impl RoleUsage {
    pub fn add(&mut self, other: &RoleUsage) {
        self.inference.add(&other.inference);
        self.maintainer.add(&other.maintainer);
        self.proposer.add(&other.proposer);
    }
}

/// What happened in one iteration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IterationRecord {
    pub iteration: u32,
    pub train_score: f64,
    /// None when the proposal was rejected unscored.
    pub val_score: Option<f64>,
    /// Best score after this iteration's gate.
    pub best: f64,
    pub skill: Option<String>,
    pub decision: Decision,
    pub repeat: Option<RepeatCheck>,
    pub usage: RoleUsage,
    /// Vault-relative paths of this iteration's raw notes.
    #[serde(default)]
    pub raw_notes: Vec<String>,
    /// Unified diff of the proposal, so the Gate view can show it after the fact.
    #[serde(default)]
    pub diff: String,
    pub started: chrono::DateTime<chrono::Utc>,
    pub finished: Option<chrono::DateTime<chrono::Utc>>,
}

/// One full run of the loop.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunState {
    pub id: String,
    pub status: RunStatus,
    /// Deploy model the rollouts ran on. Recorded so a provider swapping the model
    /// behind an ID is visible after the fact.
    pub model_id: String,
    pub gate_rule: crate::config::GateRule,
    /// Validation score with no skills — where the best score starts.
    pub baseline_val: Option<f64>,
    /// Test-split score with no skills, measured once in phase 0.
    pub baseline_test: Option<f64>,
    /// Best validation score so far.
    pub best: f64,
    pub iterations_planned: u32,
    pub iterations: Vec<IterationRecord>,
    /// Final test score, touched once per run.
    pub test_score: Option<f64>,
    pub usage: RoleUsage,
    pub started: chrono::DateTime<chrono::Utc>,
    pub finished: Option<chrono::DateTime<chrono::Utc>>,
    /// Last error, when `status` is `Failed`.
    pub error: Option<String>,
    /// Human-readable phase, for the cockpit's progress line.
    pub phase: String,
}

impl RunState {
    pub fn new(id: impl Into<String>, model_id: impl Into<String>, planned: u32, rule: crate::config::GateRule) -> Self {
        Self {
            id: id.into(),
            status: RunStatus::Queued,
            model_id: model_id.into(),
            gate_rule: rule,
            baseline_val: None,
            baseline_test: None,
            best: 0.0,
            iterations_planned: planned,
            iterations: Vec::new(),
            test_score: None,
            usage: RoleUsage::default(),
            started: chrono::Utc::now(),
            finished: None,
            error: None,
            phase: "queued".into(),
        }
    }

    pub fn accepted_count(&self) -> usize {
        self.iterations
            .iter()
            .filter(|i| i.decision.accepted())
            .count()
    }

    /// Iterations that changed the best score, for the Skills view's version list.
    pub fn accepted_skills(&self) -> Vec<(u32, String)> {
        self.iterations
            .iter()
            .filter(|i| i.decision.accepted())
            .filter_map(|i| i.skill.clone().map(|s| (i.iteration, s)))
            .collect()
    }
}

/// A schedule the daemon owns: nightly maintainer, weekly proposer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Schedule {
    pub id: String,
    pub kind: ScheduleKind,
    pub enabled: bool,
    /// Local hour, 0-23.
    pub hour: u32,
    /// For weekly schedules: 0 = Monday.
    pub weekday: Option<u32>,
    pub last_run: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleKind {
    /// Fold the day's captured daily-coding sessions into the wiki.
    NightlyMaintainer,
    /// Propose one skill change from the week's patterns.
    WeeklyProposer,
}

impl Schedule {
    pub fn defaults() -> Vec<Schedule> {
        vec![
            Schedule {
                id: "nightly-maintainer".into(),
                kind: ScheduleKind::NightlyMaintainer,
                enabled: false,
                hour: 3,
                weekday: None,
                last_run: None,
            },
            Schedule {
                id: "weekly-proposer".into(),
                kind: ScheduleKind::WeeklyProposer,
                enabled: false,
                hour: 4,
                weekday: Some(6),
                last_run: None,
            },
        ]
    }

    /// Whether this schedule is due at `now`, given `last_run`.
    pub fn due(&self, now: chrono::DateTime<chrono::Local>) -> bool {
        use chrono::{Datelike, Timelike};
        if !self.enabled {
            return false;
        }
        if now.hour() != self.hour {
            return false;
        }
        if let Some(weekday) = self.weekday {
            if now.weekday().num_days_from_monday() != weekday {
                return false;
            }
        }
        match self.last_run {
            None => true,
            Some(last) => now.signed_duration_since(last) > chrono::Duration::hours(23),
        }
    }
}

/// A trace whose Jev labels fell below the confidence floor, awaiting a human pass/fail.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingLabel {
    pub note_path: String,
    pub task_id: String,
    pub iteration: u32,
    pub confidence: Option<f32>,
    pub suggested_failure_type: Option<String>,
    /// Set once a human decides.
    pub human_verdict: Option<bool>,
}

/// A skill change the weekly proposer produced from daily-coding patterns.
///
/// Phase 4 has no validation split to gate against — there is no verifier for "the user's
/// actual work" — so the daemon cannot decide this itself. It captures the diff, rolls
/// `skills/` back, and waits for a human to accept or reject it in the cockpit. That keeps
/// the invariant that nothing reaches `skills/` unless something or someone approved it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingProposal {
    pub id: String,
    pub skill: Option<String>,
    pub diff: String,
    /// The proposer's own summary of the change.
    pub summary: String,
    /// Which schedule produced it.
    pub source: String,
    pub created: chrono::DateTime<chrono::Utc>,
    /// None while awaiting review.
    pub accepted: Option<bool>,
}

/// JSON files under the state dir. Writes are atomic: write a temp file, then rename, so
/// a crash mid-write cannot leave the cockpit reading half a run.
#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn runs_dir(&self) -> PathBuf {
        self.root.join("runs")
    }

    fn run_path(&self, id: &str) -> PathBuf {
        self.runs_dir().join(format!("{id}.json"))
    }

    fn schedules_path(&self) -> PathBuf {
        self.root.join("schedules.json")
    }

    fn labels_path(&self) -> PathBuf {
        self.root.join("pending-labels.json")
    }

    pub async fn save_run(&self, run: &RunState) -> Result<()> {
        write_atomic(&self.run_path(&run.id), &serde_json::to_vec_pretty(run)?).await
    }

    pub async fn load_run(&self, id: &str) -> Result<RunState> {
        let text = tokio::fs::read_to_string(self.run_path(id))
            .await
            .map_err(|e| anyhow::anyhow!("no such run `{id}`: {e}"))?;
        Ok(serde_json::from_str(&text)?)
    }

    /// Newest first.
    pub async fn list_runs(&self) -> Result<Vec<RunState>> {
        let dir = self.runs_dir();
        if !dir.is_dir() {
            return Ok(Vec::new());
        }
        let mut runs = Vec::new();
        let mut entries = tokio::fs::read_dir(&dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            if entry.path().extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            match tokio::fs::read_to_string(entry.path()).await {
                Ok(text) => match serde_json::from_str::<RunState>(&text) {
                    Ok(run) => runs.push(run),
                    Err(e) => tracing::warn!("skipping unreadable run file {:?}: {e}", entry.path()),
                },
                Err(e) => tracing::warn!("skipping run file {:?}: {e}", entry.path()),
            }
        }
        runs.sort_by(|a, b| b.started.cmp(&a.started));
        Ok(runs)
    }

    /// A run left `Running` or `Queued` by a crash or a reboot is not actually running.
    ///
    /// `Queued` belongs here as much as `Running` does: the record is written before the run's own
    /// task takes over, so anything that stops the daemon in that window — or any failure between
    /// the two — leaves a run that the cockpit shows as queued forever, with a Pause and a Cancel
    /// button for a run that nothing will ever pick up.
    pub async fn reconcile_orphans(&self) -> Result<Vec<String>> {
        let mut reconciled = Vec::new();
        for mut run in self.list_runs().await? {
            if matches!(run.status, RunStatus::Running | RunStatus::Queued) {
                run.status = RunStatus::Failed;
                run.error = Some("the daemon stopped while this run was in progress".into());
                run.finished = Some(chrono::Utc::now());
                run.phase = "interrupted".into();
                self.save_run(&run).await?;
                reconciled.push(run.id);
            }
        }
        Ok(reconciled)
    }

    pub async fn load_schedules(&self) -> Result<Vec<Schedule>> {
        match tokio::fs::read_to_string(self.schedules_path()).await {
            Ok(text) => Ok(serde_json::from_str(&text)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Schedule::defaults()),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn save_schedules(&self, schedules: &[Schedule]) -> Result<()> {
        write_atomic(
            &self.schedules_path(),
            &serde_json::to_vec_pretty(schedules)?,
        )
        .await
    }

    pub async fn load_pending_labels(&self) -> Result<Vec<PendingLabel>> {
        match tokio::fs::read_to_string(self.labels_path()).await {
            Ok(text) => Ok(serde_json::from_str(&text)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn save_pending_labels(&self, labels: &[PendingLabel]) -> Result<()> {
        write_atomic(&self.labels_path(), &serde_json::to_vec_pretty(labels)?).await
    }

    pub async fn load_pending_proposals(&self) -> Result<Vec<PendingProposal>> {
        match tokio::fs::read_to_string(self.root.join("pending-proposals.json")).await {
            Ok(text) => Ok(serde_json::from_str(&text)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn save_pending_proposals(&self, proposals: &[PendingProposal]) -> Result<()> {
        write_atomic(
            &self.root.join("pending-proposals.json"),
            &serde_json::to_vec_pretty(proposals)?,
        )
        .await
    }

    /// Occurrence counts per pattern. Jev is unreliable at counting, so the count lives
    /// here in code and Jev only says which cluster a trace belongs to.
    pub async fn load_pattern_counts(&self) -> Result<BTreeMap<String, u32>> {
        match tokio::fs::read_to_string(self.root.join("pattern-counts.json")).await {
            Ok(text) => Ok(serde_json::from_str(&text)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn save_pattern_counts(&self, counts: &BTreeMap<String, u32>) -> Result<()> {
        write_atomic(
            &self.root.join("pattern-counts.json"),
            &serde_json::to_vec_pretty(counts)?,
        )
        .await
    }

    /// Bearer token for the localhost API, generated on first run and stored 0600.
    pub async fn ensure_token(&self) -> Result<String> {
        let path = self.root.join("api-token");
        if let Ok(existing) = tokio::fs::read_to_string(&path).await {
            let trimmed = existing.trim().to_string();
            if !trimmed.is_empty() {
                return Ok(trimmed);
            }
        }
        let token = uuid::Uuid::new_v4().simple().to_string();
        tokio::fs::create_dir_all(&self.root).await?;
        tokio::fs::write(&path, &token).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = tokio::fs::metadata(&path).await?.permissions();
            perms.set_mode(0o600);
            tokio::fs::set_permissions(&path, perms).await?;
        }
        Ok(token)
    }
}

async fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let temp = path.with_extension(format!(
        "tmp-{}",
        uuid::Uuid::new_v4().simple()
    ));
    tokio::fs::write(&temp, bytes).await?;
    tokio::fs::rename(&temp, path).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GateRule;

    fn run() -> RunState {
        RunState::new("run-1", "fireworks-ai/deepseek-v4-pro", 8, GateRule::Strict)
    }

    #[tokio::test]
    async fn runs_round_trip_and_list_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let mut first = run();
        first.started = chrono::Utc::now() - chrono::Duration::hours(2);
        let mut second = run();
        second.id = "run-2".into();
        store.save_run(&first).await.unwrap();
        store.save_run(&second).await.unwrap();

        let runs = store.list_runs().await.unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].id, "run-2");
        assert_eq!(store.load_run("run-1").await.unwrap().id, "run-1");
        assert!(store.load_run("nope").await.is_err());
    }

    #[tokio::test]
    async fn an_interrupted_run_is_reconciled_on_startup() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let mut r = run();
        r.status = RunStatus::Running;
        store.save_run(&r).await.unwrap();

        let reconciled = store.reconcile_orphans().await.unwrap();
        assert_eq!(reconciled, vec!["run-1".to_string()]);
        let loaded = store.load_run("run-1").await.unwrap();
        assert_eq!(loaded.status, RunStatus::Failed);
        assert!(loaded.error.unwrap().contains("daemon stopped"));
        // Idempotent.
        assert!(store.reconcile_orphans().await.unwrap().is_empty());
    }

    /// A run whose record was written but whose task never took over. Observed for real: the
    /// executor accepted a readiness probe without answering it, so the run sat at `queued` with
    /// no error and a Pause button, and the daemon refused every later start.
    #[tokio::test]
    async fn a_run_that_never_left_the_queue_is_reconciled_too() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let r = run();
        assert_eq!(r.status, RunStatus::Queued, "a fresh run starts queued");
        store.save_run(&r).await.unwrap();

        assert_eq!(store.reconcile_orphans().await.unwrap(), vec!["run-1".to_string()]);
        let loaded = store.load_run("run-1").await.unwrap();
        assert_eq!(loaded.status, RunStatus::Failed);
        assert!(loaded.finished.is_some());
    }

    #[tokio::test]
    async fn unreadable_run_files_are_skipped_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        store.save_run(&run()).await.unwrap();
        tokio::fs::write(dir.path().join("runs/garbage.json"), "{not json")
            .await
            .unwrap();
        assert_eq!(store.list_runs().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn the_api_token_is_stable_and_private() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let first = store.ensure_token().await.unwrap();
        assert_eq!(first, store.ensure_token().await.unwrap());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = tokio::fs::metadata(dir.path().join("api-token"))
                .await
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[tokio::test]
    async fn schedules_default_to_off() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let schedules = store.load_schedules().await.unwrap();
        assert_eq!(schedules.len(), 2);
        assert!(schedules.iter().all(|s| !s.enabled));
        store.save_schedules(&schedules).await.unwrap();
        assert_eq!(store.load_schedules().await.unwrap(), schedules);
    }

    #[test]
    fn a_disabled_schedule_is_never_due() {
        use chrono::Timelike;
        let mut schedule = Schedule::defaults()[0].clone();
        let now = chrono::Local::now()
            .with_hour(schedule.hour)
            .unwrap();
        assert!(!schedule.due(now));
        schedule.enabled = true;
        assert!(schedule.due(now));
        schedule.last_run = Some(now.to_utc());
        assert!(!schedule.due(now));
    }

    #[test]
    fn pattern_counts_and_role_usage_accumulate_in_code() {
        let mut usage = RoleUsage::default();
        usage.add(&RoleUsage {
            inference: Usage {
                input_tokens: 5,
                ..Default::default()
            },
            ..Default::default()
        });
        assert_eq!(usage.inference.input_tokens, 5);
        assert_eq!(usage.maintainer.input_tokens, 0);
    }

    #[tokio::test]
    async fn accepted_skills_are_listed_for_the_cockpit() {
        let mut r = run();
        r.iterations.push(IterationRecord {
            iteration: 0,
            train_score: 0.3,
            val_score: Some(0.7),
            best: 0.7,
            skill: Some("break-repetition-loop".into()),
            decision: Decision::Accepted,
            repeat: None,
            usage: RoleUsage::default(),
            raw_notes: vec![],
            diff: String::new(),
            started: chrono::Utc::now(),
            finished: None,
        });
        r.iterations.push(IterationRecord {
            iteration: 1,
            train_score: 0.3,
            val_score: Some(0.6),
            best: 0.7,
            skill: Some("read-before-edit".into()),
            decision: Decision::Rejected(crate::gate::RejectReason::NotBetter),
            repeat: None,
            usage: RoleUsage::default(),
            raw_notes: vec![],
            diff: String::new(),
            started: chrono::Utc::now(),
            finished: None,
        });
        assert_eq!(r.accepted_count(), 1);
        assert_eq!(
            r.accepted_skills(),
            vec![(0, "break-repetition-loop".to_string())]
        );
    }
}
