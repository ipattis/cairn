//! Phase 4: daily coding feeds the same loop.
//!
//! Sessions the user actually runs are captured by the OpenCode plugin and posted here;
//! the daemon redacts them, labels them, and files them under `raw/daily/<date>/`. A
//! nightly maintainer folds the day into the wiki. A weekly proposer suggests one skill
//! change — but daily work has no validation split, so the proposal is captured, rolled
//! back, and left for a human in the cockpit rather than auto-accepted.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use wikiskill_core::agent::{maintainer, proposer};
use wikiskill_core::executor::Transcript;
use wikiskill_core::jev::{questions, Answer};
use wikiskill_core::model;
use wikiskill_core::redact;
use wikiskill_core::state::{PendingProposal, Schedule, ScheduleKind};
use wikiskill_core::trace::{self, RawNote, TraceLabels};
use wikiskill_core::verifier::Score;

use crate::app::App;

/// What the capture plugin posts. Deliberately close to the executor's own transcript
/// shape, so the plugin does no interpreting: it reads the session and forwards it.
#[derive(Debug, Clone, Deserialize)]
pub struct CaptureRequest {
    pub session: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub directory: Option<String>,
    #[serde(default)]
    pub messages: Vec<wikiskill_core::executor::TranscriptMessage>,
}

#[derive(Debug, Serialize)]
pub struct CaptureResponse {
    pub note: String,
    pub obsidian_link: String,
    pub redactions: String,
}

/// Files one captured session. Redaction happens before anything is written or sent to
/// Jev — a daily session is far more likely to contain real credentials than a rollout on
/// a synthetic task is.
pub async fn capture(app: &Arc<App>, request: CaptureRequest) -> Result<CaptureResponse> {
    if request.messages.is_empty() {
        anyhow::bail!("captured session `{}` had no messages", request.session);
    }
    let transcript = Transcript {
        session: request.session.clone(),
        messages: request.messages,
    };

    let (jsonl, jsonl_report) = redact::redact(&transcript.to_jsonl()?);
    let condensed_raw = trace::condense(&transcript, 6, 24_000, |_| false);
    let (condensed, mut report) = redact::redact(&condensed_raw);
    report.merge(&jsonl_report);

    let mut labels = TraceLabels::default();
    if app.config.jev.enabled {
        let jev = wikiskill_core::jev::JevClient::new(
            app.config.jev.clone(),
            std::env::var("JEV_API_KEY").unwrap_or_default(),
        )?;
        if let Some(outcome) = jev.ask(&condensed, &questions::task_completed()).await? {
            labels.absorb(&outcome);
        }
        // Phase 4's own question: was a relevant skill actually loaded? A "no" here is the
        // signal that the skill exists but its description does not match real work.
        if let Some(outcome) = jev
            .ask(&condensed, &questions::relevant_skill_loaded())
            .await?
        {
            if let Answer::Noul { value: false, .. } = outcome.answer {
                tracing::info!(
                    session = %request.session,
                    "no relevant skill was loaded for this session"
                );
            }
        }
    }

    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    let jsonl_dir = app.vault.raw_dir().join("_jsonl");
    tokio::fs::create_dir_all(&jsonl_dir).await?;
    let jsonl_path = jsonl_dir.join(format!(
        "daily-{date}.{}.jsonl",
        wikiskill_core::vault::sanitize_file_stem(&request.session)
    ));
    wikiskill_core::vault::write_read_only(&jsonl_path, jsonl.as_bytes()).await?;

    let note = RawNote {
        iteration: 0,
        task_id: request
            .title
            .clone()
            .unwrap_or_else(|| request.session.clone()),
        model: request.directory.clone().unwrap_or_default(),
        // Daily work has no verifier: there is no ground truth for "did this help".
        score: Score::zero(&request.session, "daily session: not scored"),
        labels,
        condensed,
        redaction: report.clone(),
        jsonl_path: format!(
            "raw/_jsonl/{}",
            jsonl_path.file_name().unwrap().to_string_lossy()
        ),
        created: chrono::Utc::now(),
    };
    let path = app
        .vault
        .write_daily_note(&date, &request.session, &note.render())
        .await?;
    let relative = app.vault.relative(&path);

    Ok(CaptureResponse {
        obsidian_link: app.vault.obsidian_link(&relative),
        note: relative,
        redactions: report.summary(),
    })
}

/// Checks schedules once a minute. Schedules live in the daemon, not launchd, so the
/// cockpit can change them without touching a plist.
pub async fn scheduler(app: Arc<App>) {
    loop {
        tokio::time::sleep(Duration::from_secs(60)).await;
        if let Err(e) = tick(&app).await {
            tracing::warn!("scheduler tick failed: {e:#}");
        }
    }
}

async fn tick(app: &Arc<App>) -> Result<()> {
    let mut schedules = app.store.load_schedules().await?;
    let now = chrono::Local::now();
    let mut changed = false;

    for schedule in &mut schedules {
        if !schedule.due(now) {
            continue;
        }
        // A run in flight owns the vault and the rollout server; a scheduled curator pass
        // would write `skills/` underneath it.
        if app.active_run().await?.is_some() {
            tracing::info!(
                schedule = %schedule.id,
                "skipping: a run is active and owns the vault"
            );
            continue;
        }
        tracing::info!(schedule = %schedule.id, "running scheduled job");
        let result = match schedule.kind {
            ScheduleKind::NightlyMaintainer => nightly_maintainer(app, schedule).await.map(|_| ()),
            ScheduleKind::WeeklyProposer => weekly_proposer(app, schedule).await.map(|_| ()),
        };
        match result {
            Ok(()) => {
                schedule.last_run = Some(chrono::Utc::now());
                changed = true;
            }
            Err(e) => tracing::error!(schedule = %schedule.id, "scheduled job failed: {e:#}"),
        }
    }

    if changed {
        app.store.save_schedules(&schedules).await?;
    }
    Ok(())
}

/// Folds the previous day's captured sessions into the wiki.
pub async fn nightly_maintainer(app: &Arc<App>, schedule: &Schedule) -> Result<String> {
    let date = (chrono::Local::now() - chrono::Duration::days(1))
        .format("%Y-%m-%d")
        .to_string();
    let notes = app.vault.daily_notes(&date).await?;
    if notes.is_empty() {
        tracing::info!("nothing captured on {date}; nothing to fold in");
        return Ok(String::new());
    }

    let trace_paths = notes.iter().map(|path| app.vault.relative(path)).collect();
    let brief = maintainer::MaintainerBrief {
        iteration: 0,
        trace_paths,
        known_patterns: app
            .store
            .load_pattern_counts()
            .await?
            .into_iter()
            .collect(),
        train_score: 0.0,
    };

    let client = model::client_for(&app.config.models.maintainer).await?;
    let run = maintainer::run(
        &app.vault,
        client.as_ref(),
        &brief,
        app.config.models.maintainer.max_tokens,
        app.config.models.maintainer.temperature,
        app.config.r#loop.curator_turn_cap,
    )
    .await?;

    app.repo
        .commit_paths(
            &["wiki", "raw"],
            &format!("{}: fold {date} into the wiki", schedule.id),
        )
        .await?;
    tracing::info!("nightly maintainer: {}", run.final_text);
    Ok(run.final_text)
}

/// Proposes one skill change from the week's patterns, then rolls it back and files it for
/// review: there is no validation split for the user's real work, so no gate can decide.
pub async fn weekly_proposer(app: &Arc<App>, schedule: &Schedule) -> Result<Option<PendingProposal>> {
    let active_skills = app
        .vault
        .active_skills()
        .await?
        .into_iter()
        .map(|s| s.name)
        .collect();
    let mut pattern_paths = Vec::new();
    let patterns_dir = app.vault.wiki_dir().join("patterns");
    if patterns_dir.is_dir() {
        let mut entries = tokio::fs::read_dir(&patterns_dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.ends_with(".md") {
                pattern_paths.push(format!("wiki/patterns/{name}"));
            }
        }
        pattern_paths.sort();
    }
    if pattern_paths.is_empty() {
        tracing::info!("no wiki patterns yet; nothing to propose from");
        return Ok(None);
    }

    let brief = proposer::ProposerBrief {
        iteration: 0,
        active_skills,
        pattern_paths,
        outcomes: Vec::new(),
        best_score: 0.0,
        repeat_warning: Some(
            "This proposal comes from daily-coding patterns, not from a scored run. It \
             will be held for human review rather than validated, so prefer a change whose \
             value is obvious from the patterns alone."
                .into(),
        ),
    };

    let client = model::client_for(&app.config.models.proposer).await?;
    let run = proposer::run(
        &app.vault,
        client.as_ref(),
        &brief,
        app.config.models.proposer.max_tokens,
        app.config.models.proposer.temperature,
        app.config.r#loop.curator_turn_cap,
    )
    .await?;

    let diff = app.repo.skills_diff().await?;
    if diff.trim().is_empty() {
        tracing::info!("the weekly proposer changed nothing");
        return Ok(None);
    }
    let skill = app.repo.changed_skill_folders().await?.into_iter().next();

    // Roll it back: nothing reaches skills/ without approval.
    app.repo.unstage_skills().await?;
    app.repo.restore_skills().await?;

    let proposal = PendingProposal {
        id: format!("proposal-{}", chrono::Local::now().format("%Y%m%d-%H%M%S")),
        skill,
        diff,
        summary: run.final_text,
        source: schedule.id.clone(),
        created: chrono::Utc::now(),
        accepted: None,
    };
    let mut pending = app.store.load_pending_proposals().await?;
    pending.push(proposal.clone());
    app.store.save_pending_proposals(&pending).await?;
    tracing::info!(id = %proposal.id, "weekly proposal awaiting review in the cockpit");
    Ok(Some(proposal))
}

/// Applies a reviewed proposal by replaying its diff with `git apply`.
pub async fn accept_proposal(app: &Arc<App>, id: &str) -> Result<()> {
    let mut pending = app.store.load_pending_proposals().await?;
    let Some(proposal) = pending.iter_mut().find(|p| p.id == id) else {
        anyhow::bail!("no pending proposal `{id}`");
    };
    if proposal.accepted.is_some() {
        anyhow::bail!("proposal `{id}` was already reviewed");
    }
    app.repo.apply_patch(&proposal.diff).await?;
    app.repo
        .commit_all(&format!(
            "{id}: accept {} after review",
            proposal.skill.clone().unwrap_or_else(|| "skill change".into())
        ))
        .await?;
    proposal.accepted = Some(true);
    app.store.save_pending_proposals(&pending).await?;
    Ok(())
}

pub async fn reject_proposal(app: &Arc<App>, id: &str) -> Result<()> {
    let mut pending = app.store.load_pending_proposals().await?;
    let Some(proposal) = pending.iter_mut().find(|p| p.id == id) else {
        anyhow::bail!("no pending proposal `{id}`");
    };
    proposal.accepted = Some(false);
    app.store.save_pending_proposals(&pending).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wikiskill_core::executor::{TranscriptMessage, TranscriptToolCall};

    fn request() -> CaptureRequest {
        CaptureRequest {
            session: "ses_abc".into(),
            title: Some("fix the flaky test".into()),
            directory: Some("/Users/me/code/thing".into()),
            messages: vec![
                TranscriptMessage {
                    role: "user".into(),
                    text: "deploy with sk-abcdef1234567890abcdef1234567890".into(),
                    ..Default::default()
                },
                TranscriptMessage {
                    role: "assistant".into(),
                    text: "done".into(),
                    tool_calls: vec![TranscriptToolCall {
                        name: "bash".into(),
                        input: serde_json::json!({"command": "cargo test"}),
                        output: "ok".into(),
                        ok: true,
                    }],
                    ..Default::default()
                },
            ],
        }
    }

    /// A daily session is likelier than any rollout to contain a real credential, so the two
    /// things this checks are that the secret is gone from both files it writes, and that the
    /// note it reports is a vault-relative path the rest of the system can use.
    #[tokio::test]
    async fn a_captured_session_is_redacted_before_it_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let app = crate::app::test_app(dir.path()).await.unwrap();

        let response = capture(&app, request()).await.unwrap();
        assert_eq!(response.note, "raw/daily/{date}/ses_abc.md".replace(
            "{date}",
            &chrono::Local::now().format("%Y-%m-%d").to_string(),
        ));
        assert_eq!(response.redactions, "provider-api-key×2");

        let note_path = app.vault.root().join(&response.note);
        let note = tokio::fs::read_to_string(&note_path).await.unwrap();
        assert!(!note.contains("sk-abcdef"), "{note}");
        assert!(note.contains("[REDACTED:provider-api-key]"), "{note}");
        // Raw notes are history: read-only, so nothing downstream can rewrite them.
        assert!(tokio::fs::metadata(&note_path)
            .await
            .unwrap()
            .permissions()
            .readonly());

        let jsonl = tokio::fs::read_to_string(
            app.vault
                .raw_dir()
                .join("_jsonl")
                .join(format!(
                    "daily-{}.ses_abc.jsonl",
                    chrono::Local::now().format("%Y-%m-%d")
                )),
        )
        .await
        .unwrap();
        assert!(!jsonl.contains("sk-abcdef"), "{jsonl}");
        assert_eq!(jsonl.lines().count(), 2);
    }

    #[tokio::test]
    async fn an_empty_session_is_not_filed() {
        let dir = tempfile::tempdir().unwrap();
        let app = crate::app::test_app(dir.path()).await.unwrap();
        let mut request = request();
        request.messages.clear();
        let err = match capture(&app, request).await {
            Ok(_) => panic!("an empty capture should be refused"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("no messages"), "{err}");
    }

    /// The nightly job must be a no-op on a day with nothing captured, rather than calling a
    /// model with an empty brief — the daemon runs this unattended at 3am.
    #[tokio::test]
    async fn the_nightly_maintainer_does_nothing_on_an_empty_day() {
        let dir = tempfile::tempdir().unwrap();
        let app = crate::app::test_app(dir.path()).await.unwrap();
        let schedule = &Schedule::defaults()[0];
        assert!(nightly_maintainer(&app, schedule).await.unwrap().is_empty());
    }

    /// Likewise the weekly proposer: no patterns means nothing to propose from, and it must
    /// reach that conclusion without a model call.
    #[tokio::test]
    async fn the_weekly_proposer_needs_patterns_to_propose_from() {
        let dir = tempfile::tempdir().unwrap();
        let app = crate::app::test_app(dir.path()).await.unwrap();
        let schedule = &Schedule::defaults()[1];
        assert!(weekly_proposer(&app, schedule).await.unwrap().is_none());
    }
}
