//! One iteration of the WikiSkill loop, and the outer loop over iterations.
//!
//! The order is the rationale's, exactly:
//!
//! 1. Regenerate `wiki-inference.md` from `skills/`, then reload the rollout server.
//! 2. For each train task, run a rollout on the deploy model and fetch its transcript.
//! 3. Score it with the verifier, redact it, label it with Jev, write the raw note.
//! 4. Run the maintainer loop, then the proposer loop, in-process.
//! 5. Check the diff scope and run the Jev repeat check.
//! 6. Run the validation split, gate, append to `skill-impact.md`, commit or restore,
//!    and push status to the cockpit.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;

use crate::agent::{maintainer, proposer};
use crate::config::Config;
use crate::eval::{Split, Task, TaskSet};
use crate::executor::{opencode, Executor, RolloutSpec, Transcript};
use crate::gate::{self, GateInput, RepeatCheck};
use crate::git::Repo;
use crate::jev::{questions, Answer, JevClient};
use crate::model::{ModelClient, Usage};
use crate::redact;
use crate::state::{IterationRecord, PendingLabel, RoleUsage, RunState, RunStatus, Store};
use crate::trace::{self, RawNote, TraceLabels};
use crate::vault::{Skill, Vault};
use crate::verifier::{self, Score, Verifier};
use crate::Result;

/// How the regenerated agent file reaches the executor. The daemon writes it into the
/// rollout `HOME` and restarts `opencode serve`; tests record it instead.
#[async_trait]
pub trait AgentFileSink: Send + Sync {
    /// Publishes the agent file and makes the executor pick it up.
    async fn publish(&self, agent: &str, contents: &str) -> Result<()>;
}

/// A sink that only records, for tests and dry runs.
#[derive(Default)]
pub struct RecordingSink {
    pub published: std::sync::Mutex<Vec<String>>,
}

#[async_trait]
impl AgentFileSink for RecordingSink {
    async fn publish(&self, _agent: &str, contents: &str) -> Result<()> {
        self.published.lock().unwrap().push(contents.to_string());
        Ok(())
    }
}

/// Pause and cancel, driven from the cockpit. Checked at iteration boundaries, so a
/// pause never leaves a half-applied proposal behind.
#[derive(Debug, Default)]
pub struct Control {
    paused: AtomicBool,
    cancelled: AtomicBool,
}

impl Control {
    pub fn pause(&self) {
        self.paused.store(true, Ordering::SeqCst);
    }

    pub fn resume(&self) {
        self.paused.store(false, Ordering::SeqCst);
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub fn paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    pub fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

/// Everything an iteration needs. Shared behind an `Arc` so rollouts can run in parallel.
pub struct LoopDeps {
    pub config: Config,
    pub vault: Vault,
    pub repo: Repo,
    pub store: Store,
    pub executor: Arc<dyn Executor>,
    pub verifier: Arc<Verifier>,
    pub maintainer_client: Arc<dyn ModelClient>,
    pub proposer_client: Arc<dyn ModelClient>,
    pub jev: Arc<JevClient>,
    pub agent_files: Arc<dyn AgentFileSink>,
    /// Scratch root for per-rollout workspace copies.
    pub work_root: PathBuf,
}

/// Drives one run from baseline to final test score.
pub struct Runner {
    deps: Arc<LoopDeps>,
    tasks: TaskSet,
    pub run: RunState,
    pub control: Arc<Control>,
}

impl Runner {
    pub fn new(deps: Arc<LoopDeps>, tasks: TaskSet, run: RunState, control: Arc<Control>) -> Self {
        Self {
            deps,
            tasks,
            run,
            control,
        }
    }

    /// Phase 0's exit criterion and the gate's starting point: the empty-skill
    /// validation score on the deploy model.
    pub async fn baseline(&mut self) -> Result<f64> {
        self.set_phase("baseline: empty-skill validation").await?;
        let skills = self.deps.vault.active_skills().await?;
        if !skills.is_empty() {
            tracing::warn!(
                "baseline is being measured with {} skill(s) already active; the paper's \
                 baseline is the empty-skill score",
                skills.len()
            );
        }
        self.publish_agent_file(&skills).await?;
        let (scores, _notes, usage) = self
            .run_split(Split::Val, 0, false)
            .await?;
        let score = verifier::mean(&scores);
        self.run.baseline_val = Some(score);
        self.run.best = score;
        self.run.usage.inference.add(&usage);
        self.save().await?;
        Ok(score)
    }

    /// The whole run: baseline, then iterations, then the test split once.
    pub async fn run_all(&mut self) -> Result<()> {
        self.run.status = RunStatus::Running;
        self.save().await?;

        let result = self.run_all_inner().await;

        match result {
            Ok(()) => {
                if self.run.status == RunStatus::Running {
                    self.run.status = RunStatus::Completed;
                }
            }
            Err(e) => {
                self.run.status = RunStatus::Failed;
                self.run.error = Some(e.to_string());
                self.run.phase = "failed".into();
                self.save().await?;
                return Err(e);
            }
        }
        self.run.finished = Some(chrono::Utc::now());
        self.save().await?;
        Ok(())
    }

    async fn run_all_inner(&mut self) -> Result<()> {
        if self.run.baseline_val.is_none() {
            self.baseline().await?;
        }

        for iteration in 0..self.run.iterations_planned {
            if self.control.cancelled() {
                self.run.status = RunStatus::Cancelled;
                self.run.phase = "cancelled".into();
                self.save().await?;
                return Ok(());
            }
            if self.control.paused() {
                self.run.status = RunStatus::Paused;
                self.run.phase = format!("paused before iteration {iteration:03}");
                self.save().await?;
                return Ok(());
            }

            let record = self.run_iteration(iteration).await?;
            let val = record.val_score;
            self.run.iterations.push(record);
            self.save().await?;

            if self.deps.config.r#loop.stop_at_perfect
                && val.map(gate::perfect).unwrap_or(false)
            {
                tracing::info!("validation reached 100%; stopping early");
                self.run.status = RunStatus::CompletedPerfect;
                break;
            }
        }

        // The test split is touched once per run.
        self.set_phase("final test split").await?;
        let skills = self.deps.vault.active_skills().await?;
        self.publish_agent_file(&skills).await?;
        let (scores, _notes, usage) = self.run_split(Split::Test, u32::MAX, false).await?;
        self.run.test_score = Some(verifier::mean(&scores));
        self.run.usage.inference.add(&usage);
        self.save().await?;
        Ok(())
    }

    /// One iteration, steps 1 to 6.
    pub async fn run_iteration(&mut self, iteration: u32) -> Result<IterationRecord> {
        let started = chrono::Utc::now();
        let mut usage = RoleUsage::default();

        // 1. Regenerate the agent file from skills/ and reload the rollout server.
        self.set_phase(&format!("iter-{iteration:03}: injecting skills"))
            .await?;
        let skills = self.deps.vault.active_skills().await?;
        self.publish_agent_file(&skills).await?;

        // 2-3. Train rollouts, scored, redacted, labeled and written to raw/.
        self.set_phase(&format!("iter-{iteration:03}: train rollouts"))
            .await?;
        let (train_scores, notes, inference_usage) =
            self.run_split(Split::Train, iteration, true).await?;
        usage.inference.add(&inference_usage);
        let train_score = verifier::mean(&train_scores);
        let raw_note_paths: Vec<String> = notes
            .iter()
            .map(|n| {
                format!(
                    "raw/iter-{:03}/{}.md",
                    n.iteration,
                    crate::vault::sanitize_file_stem(&n.task_id)
                )
            })
            .collect();

        // Pattern dedup runs before the maintainer, and counting stays in code.
        let pattern_counts = self.dedup_patterns(&notes).await?;

        // 4. Curators, in-process.
        self.set_phase(&format!("iter-{iteration:03}: wiki maintainer"))
            .await?;
        let trace_paths = trace::sample_stratified(&notes, 8)
            .iter()
            .map(|n| {
                format!(
                    "raw/iter-{:03}/{}.md",
                    n.iteration,
                    crate::vault::sanitize_file_stem(&n.task_id)
                )
            })
            .collect();
        let brief = maintainer::MaintainerBrief {
            iteration,
            trace_paths,
            known_patterns: pattern_counts.into_iter().collect(),
            train_score,
        };
        let maintainer_run = maintainer::run(
            &self.deps.vault,
            self.deps.maintainer_client.as_ref(),
            &brief,
            self.deps.config.models.maintainer.max_tokens,
            self.deps.config.models.maintainer.temperature,
            self.deps.config.r#loop.curator_turn_cap,
        )
        .await?;
        usage.maintainer.add(&maintainer_run.usage);

        self.set_phase(&format!("iter-{iteration:03}: skill proposer"))
            .await?;
        let proposer_brief = self.proposer_brief(iteration, None).await?;
        let proposer_run = proposer::run(
            &self.deps.vault,
            self.deps.proposer_client.as_ref(),
            &proposer_brief,
            self.deps.config.models.proposer.max_tokens,
            self.deps.config.models.proposer.temperature,
            self.deps.config.r#loop.curator_turn_cap,
        )
        .await?;
        usage.proposer.add(&proposer_run.usage);

        // 5. Diff scope, then the Jev repeat check.
        let diff = self.deps.repo.skills_diff().await?;
        let changed = self.deps.repo.changed_skill_folders().await?;
        let repeat = self.repeat_check(&diff).await?;

        // 6. Validation, gate, impact log, commit or restore.
        let mut val_runs: Vec<Vec<Score>> = Vec::new();
        if changed.len() == 1 {
            self.set_phase(&format!("iter-{iteration:03}: validation"))
                .await?;
            let skills = self.deps.vault.active_skills().await?;
            self.publish_agent_file(&skills).await?;
            for _ in 0..self.deps.config.r#loop.gate_rule.validation_runs() {
                let (scores, _notes, val_usage) =
                    self.run_split(Split::Val, iteration, false).await?;
                usage.inference.add(&val_usage);
                val_runs.push(scores);
            }
        }

        let input = GateInput {
            iteration,
            skill: changed.iter().next().cloned(),
            changed_folders: changed.clone(),
            val_runs,
            best: self.run.best,
            rule: self.deps.config.r#loop.gate_rule,
            model_id: self.run.model_id.clone(),
            diff: diff.clone(),
            repeat: repeat.clone(),
        };
        let outcome = gate::decide(&input);

        // The impact log is appended on every outcome.
        self.deps
            .vault
            .append_wiki("skill-impact.md", &outcome.impact_entry)
            .await?;

        if outcome.decision.accepted() {
            self.deps
                .repo
                .commit_all(&format!(
                    "iter-{iteration:03}: accept {} (val {:.3})",
                    input.skill.clone().unwrap_or_default(),
                    outcome.val_score.unwrap_or_default()
                ))
                .await?;
        } else {
            // Skills roll back; the wiki and raw notes persist, so commit them first —
            // with the staged proposal taken out of the index so the commit cannot
            // preserve it.
            self.deps.repo.unstage_skills().await?;
            self.deps
                .repo
                .commit_paths(
                    &["wiki", "raw", "eval"],
                    &format!("iter-{iteration:03}: wiki and traces (proposal rejected)"),
                )
                .await?;
            self.deps.repo.restore_skills().await?;
        }

        self.run.best = outcome.best;
        self.run.usage.add(&usage);

        Ok(IterationRecord {
            iteration,
            train_score,
            val_score: outcome.val_score,
            best: outcome.best,
            skill: input.skill,
            decision: outcome.decision,
            repeat,
            usage,
            raw_notes: raw_note_paths,
            diff,
            started,
            finished: Some(chrono::Utc::now()),
        })
    }

    /// Runs one split's rollouts, up to `max_parallel_rollouts` at a time.
    ///
    /// A rollout that errors or times out scores 0 rather than failing the run: one bad
    /// task should not throw away an iteration's other 44 rollouts.
    async fn run_split(
        &self,
        split: Split,
        iteration: u32,
        write_notes: bool,
    ) -> Result<(Vec<Score>, Vec<RawNote>, Usage)> {
        let tasks = self.tasks.split(split).to_vec();
        let semaphore = Arc::new(tokio::sync::Semaphore::new(
            self.deps.config.executor.max_parallel_rollouts,
        ));
        let mut join_set = tokio::task::JoinSet::new();

        for (index, task) in tasks.into_iter().enumerate() {
            let deps = Arc::clone(&self.deps);
            let semaphore = Arc::clone(&semaphore);
            let model = self.run.model_id.clone();
            join_set.spawn(async move {
                let _permit = semaphore.acquire().await.expect("semaphore closed");
                let outcome =
                    rollout_one(&deps, &task, iteration, &model, write_notes).await;
                (index, task.id.clone(), outcome)
            });
        }

        let mut results: Vec<Option<(Score, Option<RawNote>, Usage)>> = Vec::new();
        while let Some(joined) = join_set.join_next().await {
            let (index, task_id, outcome) = joined?;
            if results.len() <= index {
                results.resize_with(index + 1, || None);
            }
            match outcome {
                Ok(value) => results[index] = Some(value),
                Err(e) => {
                    tracing::warn!(task = %task_id, "rollout failed, scoring 0: {e}");
                    results[index] = Some((
                        Score::zero(&task_id, &format!("rollout failed: {e}")),
                        None,
                        Usage::default(),
                    ));
                }
            }
        }

        let mut scores = Vec::new();
        let mut notes = Vec::new();
        let mut usage = Usage::default();
        for entry in results.into_iter().flatten() {
            let (score, note, rollout_usage) = entry;
            scores.push(score);
            if let Some(note) = note {
                notes.push(note);
            }
            usage.add(&rollout_usage);
        }
        Ok((scores, notes, usage))
    }

    async fn publish_agent_file(&self, skills: &[Skill]) -> Result<()> {
        let contents = opencode::render_agent_file(&self.deps.config.executor.agent, skills);
        self.deps
            .agent_files
            .publish(&self.deps.config.executor.agent, &contents)
            .await
    }

    /// Pattern dedup: Jev says which cluster a trace belongs to; the count is ours.
    async fn dedup_patterns(&self, notes: &[RawNote]) -> Result<BTreeMap<String, u32>> {
        let mut counts = self.deps.store.load_pattern_counts().await?;
        if !self.deps.jev.enabled() {
            return Ok(counts);
        }
        let names: Vec<String> = counts.keys().cloned().collect();
        if names.is_empty() {
            return Ok(counts);
        }
        let question = questions::pattern_cluster(names);
        for note in notes {
            let Some(outcome) = self.deps.jev.ask(&note.condensed, &question).await? else {
                continue;
            };
            if !outcome.act() {
                continue;
            }
            if let Answer::Choice { choice, .. } = &outcome.answer {
                if choice != crate::jev::NEW_PATTERN {
                    *counts.entry(choice.clone()).or_insert(0) += 1;
                }
            }
        }
        self.deps.store.save_pattern_counts(&counts).await?;
        Ok(counts)
    }

    /// One Noul per rejected entry so far. High probability warns the proposer; it is
    /// never a hard block.
    async fn repeat_check(&self, diff: &str) -> Result<Option<RepeatCheck>> {
        if !self.deps.jev.enabled() || diff.trim().is_empty() {
            return Ok(None);
        }
        let (state, _) = redact::redact(diff);
        for record in self
            .run
            .iterations
            .iter()
            .filter(|i| !i.decision.accepted())
        {
            let question = questions::repeat_of(record.iteration);
            let Some(outcome) = self.deps.jev.ask(&state, &question).await? else {
                continue;
            };
            if let Answer::Noul { value, probability } = outcome.answer {
                if value && outcome.confident {
                    return Ok(Some(RepeatCheck {
                        repeat_of: record.iteration,
                        probability,
                        warned: outcome.actionable,
                    }));
                }
            }
        }
        Ok(None)
    }

    async fn proposer_brief(
        &self,
        iteration: u32,
        repeat_warning: Option<String>,
    ) -> Result<proposer::ProposerBrief> {
        let active_skills = self
            .deps
            .vault
            .active_skills()
            .await?
            .into_iter()
            .map(|s| s.name)
            .collect();
        let mut pattern_paths = Vec::new();
        let patterns_dir = self.deps.vault.wiki_dir().join("patterns");
        if patterns_dir.is_dir() {
            let mut entries = tokio::fs::read_dir(&patterns_dir).await?;
            while let Some(entry) = entries.next_entry().await? {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.ends_with(".md") && !name.starts_with('.') {
                    pattern_paths.push(format!("wiki/patterns/{name}"));
                }
            }
            pattern_paths.sort();
        }
        let outcomes = self
            .run
            .iterations
            .iter()
            .map(|record| proposer::OutcomeRow {
                iteration: record.iteration,
                skill: record.skill.clone().unwrap_or_else(|| "(none)".into()),
                accepted: record.decision.accepted(),
                val_score: record.val_score.unwrap_or(0.0),
                best_score: record.best,
            })
            .collect();
        Ok(proposer::ProposerBrief {
            iteration,
            active_skills,
            pattern_paths,
            outcomes,
            best_score: self.run.best,
            repeat_warning,
        })
    }

    async fn set_phase(&mut self, phase: &str) -> Result<()> {
        tracing::info!(run = %self.run.id, "{phase}");
        self.run.phase = phase.to_string();
        self.save().await
    }

    async fn save(&self) -> Result<()> {
        self.deps.store.save_run(&self.run).await
    }
}

/// One rollout: prepare a private workspace, run it, score it, redact it, label it, and
/// write the raw note.
async fn rollout_one(
    deps: &LoopDeps,
    task: &Task,
    iteration: u32,
    model: &str,
    write_note: bool,
) -> Result<(Score, Option<RawNote>, Usage)> {
    let work_dir = prepare_work_dir(deps, task, iteration).await?;

    let spec = RolloutSpec {
        agent: deps.config.executor.agent.clone(),
        model: model.to_string(),
        prompt: task.prompt.clone(),
        work_dir: work_dir.clone(),
        title: format!("iter-{iteration:03} {}", task.id),
    };

    let transcript = match tokio::time::timeout(
        std::time::Duration::from_secs(deps.config.r#loop.rollout_timeout_secs),
        deps.executor.run_rollout(&spec),
    )
    .await
    {
        Ok(Ok(transcript)) => transcript,
        Ok(Err(e)) => return Err(e),
        Err(_) => anyhow::bail!(
            "rollout timed out after {}s",
            deps.config.r#loop.rollout_timeout_secs
        ),
    };

    let usage = transcript.usage();
    let score = deps.verifier.score(task, &work_dir).await?;

    if !write_note {
        return Ok((score, None, usage));
    }

    // Redaction runs before anything is written or sent to Jev.
    let (jsonl, jsonl_report) = redact::redact(&transcript.to_jsonl()?);
    let jsonl_path = deps
        .vault
        .write_raw_jsonl(iteration, &task.id, &jsonl)
        .await?;

    let condensed_raw = condense_with_jev(deps, &transcript).await?;
    let (condensed, mut report) = redact::redact(&condensed_raw);
    report.counts.extend(jsonl_report.counts);

    let labels = label_trace(deps, &condensed).await?;

    let note = RawNote {
        iteration,
        task_id: task.id.clone(),
        model: model.to_string(),
        score: score.clone(),
        labels,
        condensed,
        redaction: report,
        jsonl_path: vault_relative(&deps.vault, &jsonl_path),
        created: chrono::Utc::now(),
    };
    deps.vault
        .write_raw_note(iteration, &task.id, &note.render())
        .await?;

    if note.labels.needs_review {
        let mut pending = deps.store.load_pending_labels().await?;
        pending.push(PendingLabel {
            note_path: format!(
                "raw/iter-{iteration:03}/{}.md",
                crate::vault::sanitize_file_stem(&task.id)
            ),
            task_id: task.id.clone(),
            iteration,
            confidence: note.labels.confidence,
            suggested_failure_type: note.labels.failure_type.clone(),
            human_verdict: None,
        });
        deps.store.save_pending_labels(&pending).await?;
    }

    Ok((score, Some(note), usage))
}

/// Condenses a transcript, using a per-chunk Noul to keep optional chunks when Jev is
/// live and confident.
async fn condense_with_jev(deps: &LoopDeps, transcript: &Transcript) -> Result<String> {
    const FINAL_TURNS: usize = 4;
    const MAX_CHARS: usize = 20_000;

    if !deps.jev.enabled() {
        return Ok(trace::condense(transcript, FINAL_TURNS, MAX_CHARS, |_| false));
    }

    let chunks = trace::chunks(transcript, FINAL_TURNS);
    let question = questions::chunk_relevant();
    let mut keep: Vec<String> = Vec::new();
    for chunk in chunks.iter().filter(|c| !c.always_keep) {
        let (state, _) = redact::redact(&chunk.text);
        if let Some(outcome) = deps.jev.ask(&state, &question).await? {
            if let Answer::Noul { value: true, .. } = outcome.answer {
                if outcome.act() {
                    keep.push(chunk.label.clone());
                }
            }
        }
    }
    Ok(trace::condense(transcript, FINAL_TURNS, MAX_CHARS, |chunk| {
        keep.contains(&chunk.label)
    }))
}

/// Trace labeling: completed?, failure type, severity.
async fn label_trace(deps: &LoopDeps, condensed: &str) -> Result<TraceLabels> {
    let mut labels = TraceLabels::default();
    if !deps.jev.enabled() {
        return Ok(labels);
    }
    if let Some(outcome) = deps.jev.ask(condensed, &questions::task_completed()).await? {
        labels.absorb(&outcome);
    }
    let completed = labels.completed.unwrap_or(false);
    if !completed {
        let kinds = deps
            .store
            .load_pattern_counts()
            .await?
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        if let Some(outcome) = deps
            .jev
            .ask(condensed, &questions::failure_type(kinds))
            .await?
        {
            labels.absorb(&outcome);
        }
        if let Some(outcome) = deps.jev.ask(condensed, &questions::severity()).await? {
            labels.absorb(&outcome);
        }
    }
    Ok(labels)
}

/// Each rollout gets its own copy of the task workspace, so parallel rollouts cannot
/// see each other's edits and the template is never mutated.
async fn prepare_work_dir(deps: &LoopDeps, task: &Task, iteration: u32) -> Result<PathBuf> {
    let dir = deps
        .work_root
        .join(format!("iter-{iteration:03}"))
        .join(format!(
            "{}-{}",
            crate::vault::sanitize_file_stem(&task.id),
            uuid::Uuid::new_v4().simple()
        ));
    tokio::fs::create_dir_all(&dir).await?;
    if task.workspace.is_dir() {
        copy_dir_all(&task.workspace, &dir).await?;
    } else {
        tracing::warn!(
            task = %task.id,
            "workspace {} does not exist; the rollout gets an empty directory",
            task.workspace.display()
        );
    }
    Ok(dir)
}

/// Recursive copy. Symlinks are copied as links so a task workspace cannot smuggle a
/// path outside itself into the sandbox's writable tree.
pub async fn copy_dir_all(from: &Path, to: &Path) -> Result<()> {
    let mut stack = vec![(from.to_path_buf(), to.to_path_buf())];
    while let Some((src, dst)) = stack.pop() {
        tokio::fs::create_dir_all(&dst).await?;
        let mut entries = tokio::fs::read_dir(&src).await?;
        while let Some(entry) = entries.next_entry().await? {
            let file_type = entry.file_type().await?;
            let target = dst.join(entry.file_name());
            if file_type.is_dir() {
                stack.push((entry.path(), target));
            } else if file_type.is_symlink() {
                #[cfg(unix)]
                {
                    let link = tokio::fs::read_link(entry.path()).await?;
                    let _ = tokio::fs::remove_file(&target).await;
                    tokio::fs::symlink(link, &target).await?;
                }
            } else {
                tokio::fs::copy(entry.path(), &target).await?;
            }
        }
    }
    Ok(())
}

fn vault_relative(vault: &Vault, path: &Path) -> String {
    // Goes through the vault so a canonicalised path (`/private/var/...`) still comes back
    // relative; curator tools and Obsidian links both need the relative form.
    vault.relative(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GateRule;
    use crate::eval::Expect;
    use crate::gate::Decision;
    use crate::executor::mock::{simple_transcript, MockExecutor};
    use crate::model::mock::ScriptedClient;
    use crate::model::{Completion, StopReason, ToolCall};
    use crate::sandbox::NoIsolation;
    use crate::verifier::VerifierConfig;

    /// A workspace whose check script reports 1/1 when a marker file exists.
    async fn workspace(dir: &Path, passes: bool) -> PathBuf {
        let ws = dir.join("workspace");
        tokio::fs::create_dir_all(&ws).await.unwrap();
        tokio::fs::write(ws.join("marker"), if passes { "1/1" } else { "0/1" })
            .await
            .unwrap();
        ws
    }

    fn task(id: &str, ws: &Path) -> Task {
        Task {
            id: id.into(),
            prompt: format!("solve {id}"),
            workspace: ws.to_path_buf(),
            check_program: "/bin/cat".into(),
            check_args: vec!["marker".into()],
            expect: Expect::Fraction,
            tags: vec![],
        }
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        deps: Arc<LoopDeps>,
        tasks: TaskSet,
        sink: Arc<RecordingSink>,
    }

    async fn fixture(
        passing: bool,
        proposer_script: Vec<Completion>,
    ) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::init(dir.path().join("Vault")).await.unwrap();
        let repo = Repo::open(vault.root());
        repo.init().await.unwrap();
        // Keep the test independent of the machine's git identity.
        for (key, value) in [
            ("user.email", "test@example.invalid"),
            ("user.name", "wikiskill test"),
        ] {
            tokio::process::Command::new("git")
                .args(["config", key, value])
                .current_dir(vault.root())
                .output()
                .await
                .unwrap();
        }

        let ws = workspace(dir.path(), passing).await;
        let tasks = TaskSet {
            train: vec![task("train-1", &ws)],
            val: vec![task("val-1", &ws)],
            test: vec![task("test-1", &ws)],
        };

        let mut config = Config::sample(
            vault.root().to_path_buf(),
            dir.path().join("state"),
        );
        config.r#loop.iterations = 1;
        config.executor.max_parallel_rollouts = 2;

        let sink = Arc::new(RecordingSink::default());
        let deps = Arc::new(LoopDeps {
            config,
            vault: vault.clone(),
            repo,
            store: Store::new(dir.path().join("state")),
            executor: Arc::new(MockExecutor::always("did the thing")),
            verifier: Arc::new(Verifier::new(
                VerifierConfig::default(),
                Box::new(NoIsolation),
                vec![vault.root().to_path_buf()],
            )),
            maintainer_client: Arc::new(ScriptedClient::tool_then_answer(
                "sonnet",
                ToolCall {
                    id: "m1".into(),
                    name: "create".into(),
                    arguments: serde_json::json!({
                        "path": "wiki/patterns/repeat-loop.md",
                        "contents": "# repeat loop\noccurrences: 1\n"
                    }),
                },
                "recorded one pattern",
            )),
            proposer_client: Arc::new(ScriptedClient::new("opus", proposer_script)),
            jev: Arc::new(JevClient::new(Default::default(), "unused").unwrap()),
            agent_files: Arc::clone(&sink) as Arc<dyn AgentFileSink>,
            work_root: dir.path().join("work"),
        });

        Fixture {
            _dir: dir,
            deps,
            tasks,
            sink,
        }
    }

    fn create(path: &str, contents: &str, id: &str) -> Completion {
        Completion {
            text: String::new(),
            tool_calls: vec![ToolCall {
                id: id.into(),
                name: "create".into(),
                arguments: serde_json::json!({"path": path, "contents": contents}),
            }],
            stop_reason: StopReason::ToolUse,
            usage: Usage {
                input_tokens: 100,
                output_tokens: 10,
                cached_input_tokens: 0,
            },
        }
    }

    fn done(text: &str) -> Completion {
        Completion {
            text: text.into(),
            tool_calls: vec![],
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        }
    }

    fn runner(f: &Fixture) -> Runner {
        Runner::new(
            Arc::clone(&f.deps),
            f.tasks.clone(),
            RunState::new(
                "run-test",
                "fireworks-ai/deepseek-v4-pro",
                1,
                GateRule::Strict,
            ),
            Arc::new(Control::default()),
        )
    }

    #[tokio::test]
    async fn a_single_folder_proposal_that_helps_is_accepted_and_committed() {
        // Baseline scores 0 (marker says 0/1); after the proposal the executor's
        // responder is unchanged, so validation must beat 0 to be accepted. The
        // verifier reads the workspace copy, so make it pass.
        let f = fixture(
            true,
            vec![
                create(
                    "skills/break-repetition-loop/SKILL.md",
                    "When a command fails twice identically, change approach.\n",
                    "p1",
                ),
                create(
                    "skills/break-repetition-loop/PURPOSE.md",
                    "[[wiki/patterns/repeat-loop]]\n",
                    "p2",
                ),
                done("break-repetition-loop: stop retrying identical commands"),
            ],
        )
        .await;
        let mut runner = runner(&f);

        // Force a baseline the proposal can beat.
        runner.run.baseline_val = Some(0.0);
        runner.run.best = 0.0;

        let record = runner.run_iteration(0).await.unwrap();

        assert_eq!(record.decision, Decision::Accepted);
        assert_eq!(record.val_score, Some(1.0));
        assert_eq!(record.skill.as_deref(), Some("break-repetition-loop"));
        assert!(record.diff.contains("skills/break-repetition-loop/SKILL.md"));

        // The skill survived the gate and is committed.
        assert!(f
            .deps
            .vault
            .skills_dir()
            .join("break-repetition-loop/SKILL.md")
            .is_file());
        let log = tokio::fs::read_to_string(f.deps.vault.impact_log())
            .await
            .unwrap();
        assert!(log.contains("### iter-000 · break-repetition-loop · Accepted"));
        assert!(log.contains("- model: fireworks-ai/deepseek-v4-pro"));

        // The raw note and full transcript are both on disk, and the note is read-only.
        let note = f.deps.vault.raw_iter_dir(0).join("train-1.md");
        assert!(note.is_file());
        assert!(tokio::fs::metadata(&note)
            .await
            .unwrap()
            .permissions()
            .readonly());
        assert_eq!(
            tokio::fs::read_dir(f.deps.vault.raw_dir().join("_jsonl"))
                .await
                .is_ok(),
            true
        );

        // The maintainer's pattern is in the wiki.
        assert!(f
            .deps
            .vault
            .wiki_dir()
            .join("patterns/repeat-loop.md")
            .is_file());

        // The agent file was regenerated before rollouts, and again before validation.
        let published = f.sink.published.lock().unwrap();
        assert!(published.len() >= 2);
        // The first publish is the empty-skill baseline: the gate's starting point.
        assert!(published[0].contains("You have no additional skills"));
        assert!(
            !published[0].contains("permission"),
            "the agent file lives in the rollout's own writable HOME, so policy it states there \
             is advice; the ruleset travels with the session instead"
        );
        assert!(published
            .last()
            .unwrap()
            .contains("## break-repetition-loop"));
    }

    #[tokio::test]
    async fn a_two_folder_proposal_is_rejected_unscored_and_rolled_back() {
        let f = fixture(
            true,
            vec![
                create("skills/one/SKILL.md", "one\n", "p1"),
                create("skills/two/SKILL.md", "two\n", "p2"),
                done("changed two skills"),
            ],
        )
        .await;
        let mut runner = runner(&f);
        runner.run.best = 0.0;

        let record = runner.run_iteration(0).await.unwrap();

        match &record.decision {
            Decision::Rejected(crate::gate::RejectReason::DiffScope { folders }) => {
                assert_eq!(folders.len(), 2);
            }
            other => panic!("expected a scope rejection, got {other:?}"),
        }
        assert!(record.val_score.is_none(), "no validation should have run");
        // Both folders are gone; the wiki survives.
        assert!(!f.deps.vault.skills_dir().join("one").exists());
        assert!(!f.deps.vault.skills_dir().join("two").exists());
        assert!(f
            .deps
            .vault
            .wiki_dir()
            .join("patterns/repeat-loop.md")
            .is_file());
        let log = tokio::fs::read_to_string(f.deps.vault.impact_log())
            .await
            .unwrap();
        assert!(log.contains("Rejected"));
        assert!(log.contains("one per iteration"));
    }

    #[tokio::test]
    async fn a_proposal_that_does_not_beat_the_best_is_restored() {
        let f = fixture(
            true,
            vec![
                create("skills/no-help/SKILL.md", "no help\n", "p1"),
                done("added no-help"),
            ],
        )
        .await;
        let mut runner = runner(&f);
        // Validation will score 1.0; set the bar at 1.0 so a tie is rejected.
        runner.run.best = 1.0;

        let record = runner.run_iteration(0).await.unwrap();
        assert_eq!(
            record.decision,
            Decision::Rejected(crate::gate::RejectReason::NotBetter)
        );
        assert_eq!(record.val_score, Some(1.0));
        assert!(!f.deps.vault.skills_dir().join("no-help").exists());
        assert_eq!(runner.run.best, 1.0);
    }

    #[tokio::test]
    async fn the_baseline_is_the_empty_skill_validation_score() {
        let f = fixture(false, vec![done("nothing to propose")]).await;
        let mut runner = runner(&f);
        let baseline = runner.baseline().await.unwrap();
        assert_eq!(baseline, 0.0, "marker says 0/1");
        assert_eq!(runner.run.best, 0.0);
        let published = f.sink.published.lock().unwrap();
        assert!(published[0].contains("no additional skills"));
    }

    #[tokio::test]
    async fn a_full_run_ends_with_a_test_score_and_survives_restart() {
        let f = fixture(
            true,
            vec![
                create("skills/s/SKILL.md", "do it\n", "p1"),
                done("added s"),
            ],
        )
        .await;
        let mut runner = runner(&f);
        runner.run_all().await.unwrap();

        // Validation hits 1.0, so the run stops early — and still measures the test split.
        assert_eq!(runner.run.status, RunStatus::CompletedPerfect);
        assert_eq!(runner.run.iterations.len(), 1);
        assert!(runner.run.test_score.is_some());
        assert!(runner.run.finished.is_some());
        // Token cost per role is tracked for the Runs view.
        assert!(runner.run.usage.inference.input_tokens > 0);
        assert!(runner.run.usage.proposer.input_tokens > 0);

        // State is on disk, so the cockpit can reopen it after an app restart.
        let reloaded = f.deps.store.load_run("run-test").await.unwrap();
        assert_eq!(reloaded.status, RunStatus::CompletedPerfect);
        assert_eq!(reloaded.iterations.len(), 1);
    }

    #[tokio::test]
    async fn pausing_stops_at_an_iteration_boundary() {
        let f = fixture(true, vec![done("nothing")]).await;
        let mut runner = runner(&f);
        runner.control.pause();
        runner.run_all().await.unwrap();
        assert_eq!(runner.run.status, RunStatus::Paused);
        assert!(runner.run.iterations.is_empty());
        assert!(runner.run.phase.contains("paused"));
    }

    #[tokio::test]
    async fn cancelling_marks_the_run_cancelled() {
        let f = fixture(true, vec![done("nothing")]).await;
        let mut runner = runner(&f);
        runner.control.cancel();
        runner.run_all().await.unwrap();
        assert_eq!(runner.run.status, RunStatus::Cancelled);
    }

    #[tokio::test]
    async fn a_failing_rollout_scores_zero_without_failing_the_split() {
        let f = fixture(true, vec![done("nothing")]).await;
        let mut deps = LoopDeps {
            config: f.deps.config.clone(),
            vault: f.deps.vault.clone(),
            repo: f.deps.repo.clone(),
            store: f.deps.store.clone(),
            executor: Arc::new(MockExecutor::new(Arc::new(|_spec: &RolloutSpec| {
                simple_transcript("p", "answer".into(), false)
            }))),
            verifier: Arc::new(Verifier::new(
                VerifierConfig::default(),
                Box::new(NoIsolation),
                vec![],
            )),
            maintainer_client: Arc::new(ScriptedClient::answer_only("sonnet", "ok")),
            proposer_client: Arc::new(ScriptedClient::answer_only("opus", "ok")),
            jev: Arc::new(JevClient::new(Default::default(), "unused").unwrap()),
            agent_files: Arc::new(RecordingSink::default()),
            work_root: f.deps.work_root.clone(),
        };
        // A task whose check program does not exist: the rollout runs, the score is 0.
        deps.config.r#loop.rollout_timeout_secs = 5;
        let mut tasks = f.tasks.clone();
        tasks.train[0].check_program = "/nonexistent".into();

        let mut runner = Runner::new(
            Arc::new(deps),
            tasks,
            RunState::new("run-2", "m", 1, GateRule::Strict),
            Arc::new(Control::default()),
        );
        let record = runner.run_iteration(0).await.unwrap();
        assert_eq!(record.train_score, 0.0);
    }

    #[tokio::test]
    async fn workspace_copies_are_private_to_each_rollout() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("src");
        tokio::fs::create_dir_all(source.join("nested")).await.unwrap();
        tokio::fs::write(source.join("nested/file.txt"), "hello")
            .await
            .unwrap();
        let target = dir.path().join("dst");
        copy_dir_all(&source, &target).await.unwrap();
        assert_eq!(
            tokio::fs::read_to_string(target.join("nested/file.txt"))
                .await
                .unwrap(),
            "hello"
        );
        // Mutating the copy leaves the template alone.
        tokio::fs::write(target.join("nested/file.txt"), "changed")
            .await
            .unwrap();
        assert_eq!(
            tokio::fs::read_to_string(source.join("nested/file.txt"))
                .await
                .unwrap(),
            "hello"
        );
    }
}
