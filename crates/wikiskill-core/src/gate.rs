//! The gate: a skill change is kept only if the validation score strictly beats the
//! best so far, scored by the deterministic verifier on the deploy model.
//!
//! Every outcome appends an entry to `skill-impact.md`; the wiki is never rolled back.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::config::GateRule;
use crate::verifier::mean;
use crate::verifier::Score;

/// Why a proposal was rejected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectReason {
    /// The diff touched more than one `skills/<name>/` folder. Rejected before any
    /// validation run.
    DiffScope { folders: Vec<String> },
    /// The proposer changed nothing.
    NoChange,
    /// Validation did not strictly beat the best score.
    NotBetter,
}

impl RejectReason {
    pub fn summary(&self) -> String {
        match self {
            RejectReason::DiffScope { folders } => format!(
                "diff touched {} skill folders ({}); one per iteration",
                folders.len(),
                folders.join(", ")
            ),
            RejectReason::NoChange => "the proposal changed nothing".into(),
            RejectReason::NotBetter => "validation did not strictly beat the best score".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Accepted,
    Rejected(RejectReason),
}

impl Decision {
    pub fn accepted(&self) -> bool {
        matches!(self, Decision::Accepted)
    }

    pub fn label(&self) -> &'static str {
        match self {
            Decision::Accepted => "Accepted",
            Decision::Rejected(_) => "Rejected",
        }
    }
}

/// The Jev repeat check's result. It warns; it never gates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RepeatCheck {
    /// The earlier iteration this resembles.
    pub repeat_of: u32,
    pub probability: f32,
    /// Whether the proposer was warned before it ran.
    pub warned: bool,
}

/// Everything one gate decision needs.
#[derive(Debug, Clone)]
pub struct GateInput {
    pub iteration: u32,
    /// Skill folder the proposal touched, if exactly one did.
    pub skill: Option<String>,
    /// Skill folders the diff touched, straight from git.
    pub changed_folders: BTreeSet<String>,
    /// One entry per validation run (two under [`GateRule::MeanOfTwo`]).
    pub val_runs: Vec<Vec<Score>>,
    /// Best validation score so far; starts at the empty-skill baseline.
    pub best: f64,
    pub rule: GateRule,
    /// Deploy model the rollouts ran on, recorded so an ID change is traceable.
    pub model_id: String,
    /// Unified diff of the touched skill folder.
    pub diff: String,
    pub repeat: Option<RepeatCheck>,
}

/// The gate's output.
#[derive(Debug, Clone)]
pub struct GateOutcome {
    pub decision: Decision,
    /// Mean of the validation runs, or `None` when the proposal was rejected unscored.
    pub val_score: Option<f64>,
    /// Best score after this decision.
    pub best: f64,
    /// Markdown block for `skill-impact.md`.
    pub impact_entry: String,
}

/// Applies the diff-scope rule, then the paper's acceptance rule.
pub fn decide(input: &GateInput) -> GateOutcome {
    // Scope first: a wider diff is rejected before any validation run is spent.
    if input.changed_folders.len() > 1 {
        let reason = RejectReason::DiffScope {
            folders: input.changed_folders.iter().cloned().collect(),
        };
        return finish(input, Decision::Rejected(reason), None);
    }
    if input.changed_folders.is_empty() {
        return finish(input, Decision::Rejected(RejectReason::NoChange), None);
    }

    let per_run: Vec<f64> = input.val_runs.iter().map(|scores| mean(scores)).collect();
    let val_score = if per_run.is_empty() {
        0.0
    } else {
        per_run.iter().sum::<f64>() / per_run.len() as f64
    };

    // Strictly higher, as the paper has it: neutral changes are discarded.
    let decision = if val_score > input.best {
        Decision::Accepted
    } else {
        Decision::Rejected(RejectReason::NotBetter)
    };
    finish(input, decision, Some(val_score))
}

fn finish(input: &GateInput, decision: Decision, val_score: Option<f64>) -> GateOutcome {
    let best = match (&decision, val_score) {
        (Decision::Accepted, Some(score)) => score.max(input.best),
        _ => input.best,
    };
    let impact_entry = render_impact_entry(input, &decision, val_score, input.best);
    GateOutcome {
        decision,
        val_score,
        best,
        impact_entry,
    }
}

/// The impact entry, in the shape the rationale specifies.
pub fn render_impact_entry(
    input: &GateInput,
    decision: &Decision,
    val_score: Option<f64>,
    best_before: f64,
) -> String {
    let skill = input
        .skill
        .clone()
        .or_else(|| input.changed_folders.iter().next().cloned())
        .unwrap_or_else(|| "(no skill)".into());

    let mut out = String::new();
    out.push_str(&format!(
        "### iter-{:03} · {} · {}\n",
        input.iteration,
        skill,
        decision.label()
    ));
    match val_score {
        Some(score) => out.push_str(&format!(
            "- val: {:.3} (best {:.3}) · rule: {}\n",
            score,
            best_before,
            input.rule.label()
        )),
        None => out.push_str(&format!(
            "- val: not run (best {:.3}) · rule: {}\n",
            best_before,
            input.rule.label()
        )),
    }
    out.push_str(&format!("- model: {}\n", input.model_id));
    if let Decision::Rejected(reason) = decision {
        out.push_str(&format!("- reason: {}\n", reason.summary()));
    }
    if let Some(repeat) = &input.repeat {
        out.push_str(&format!(
            "- jev_repeat_of: iter-{:03} (p={:.2}, {})\n",
            repeat.repeat_of,
            repeat.probability,
            if repeat.warned { "warned" } else { "not warned" }
        ));
    }
    if input.val_runs.len() > 1 {
        let runs = input
            .val_runs
            .iter()
            .map(|scores| format!("{:.3}", mean(scores)))
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!("- runs: {runs}\n"));
    }
    out.push_str("- diff:\n\n```diff\n");
    out.push_str(input.diff.trim_end());
    out.push_str("\n```\n");
    out
}

/// Whether the loop should stop early: validation reached 100%.
pub fn perfect(val_score: f64) -> bool {
    val_score >= 1.0 - f64::EPSILON
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scores(values: &[f64]) -> Vec<Score> {
        values
            .iter()
            .enumerate()
            .map(|(i, v)| Score {
                task_id: format!("v{i}"),
                score: *v,
                exit_code: Some(0),
                timed_out: false,
                output_tail: String::new(),
            })
            .collect()
    }

    fn input(val: &[f64], best: f64, folders: &[&str]) -> GateInput {
        GateInput {
            iteration: 4,
            skill: folders.first().map(|s| s.to_string()),
            changed_folders: folders.iter().map(|s| s.to_string()).collect(),
            val_runs: if val.is_empty() {
                vec![]
            } else {
                vec![scores(val)]
            },
            best,
            rule: GateRule::Strict,
            model_id: "fireworks-ai/deepseek-v4-pro".into(),
            diff: "--- a/skills/s/SKILL.md\n+++ b/skills/s/SKILL.md\n+new line".into(),
            repeat: None,
        }
    }

    #[test]
    fn strictly_better_is_accepted() {
        let outcome = decide(&input(&[1.0, 1.0, 0.0], 0.6, &["s"]));
        assert!(outcome.decision.accepted());
        assert!((outcome.val_score.unwrap() - 0.6667).abs() < 0.001);
        assert!((outcome.best - 0.6667).abs() < 0.001);
    }

    #[test]
    fn a_tie_is_rejected_because_the_rule_is_strict() {
        let outcome = decide(&input(&[1.0, 0.0], 0.5, &["s"]));
        assert_eq!(
            outcome.decision,
            Decision::Rejected(RejectReason::NotBetter)
        );
        assert_eq!(outcome.best, 0.5);
        assert!(outcome.impact_entry.contains("strictly beat"));
    }

    #[test]
    fn a_two_folder_diff_is_rejected_unscored() {
        let mut i = input(&[], 0.5, &["a", "b"]);
        i.val_runs.clear();
        let outcome = decide(&i);
        match &outcome.decision {
            Decision::Rejected(RejectReason::DiffScope { folders }) => {
                assert_eq!(folders, &vec!["a".to_string(), "b".to_string()]);
            }
            other => panic!("expected a scope rejection, got {other:?}"),
        }
        assert!(outcome.val_score.is_none());
        assert!(outcome.impact_entry.contains("val: not run"));
        assert!(outcome.impact_entry.contains("one per iteration"));
    }

    #[test]
    fn an_empty_diff_is_rejected() {
        let outcome = decide(&input(&[1.0], 0.0, &[]));
        assert_eq!(outcome.decision, Decision::Rejected(RejectReason::NoChange));
    }

    #[test]
    fn mean_of_two_averages_the_runs_and_records_them() {
        let mut i = input(&[1.0, 0.0], 0.4, &["s"]);
        i.rule = GateRule::MeanOfTwo;
        i.val_runs = vec![scores(&[1.0, 0.0]), scores(&[1.0, 1.0])];
        let outcome = decide(&i);
        assert_eq!(outcome.val_score, Some(0.75));
        assert!(outcome.decision.accepted());
        assert!(outcome.impact_entry.contains("rule: strict, mean of two runs"));
        assert!(outcome.impact_entry.contains("- runs: 0.500, 1.000"));
    }

    #[test]
    fn the_impact_entry_matches_the_documented_shape() {
        let mut i = input(&[0.6], 0.667, &["break-repetition-loop"]);
        i.val_runs = vec![scores(&[0.6])];
        i.repeat = Some(RepeatCheck {
            repeat_of: 1,
            probability: 0.82,
            warned: true,
        });
        let entry = decide(&i).impact_entry;
        assert!(entry.starts_with("### iter-004 · break-repetition-loop · Rejected\n"));
        assert!(entry.contains("- val: 0.600 (best 0.667) · rule: strict, single run"));
        assert!(entry.contains("- jev_repeat_of: iter-001 (p=0.82, warned)"));
        assert!(entry.contains("- model: fireworks-ai/deepseek-v4-pro"));
        assert!(entry.contains("```diff"));
    }

    #[test]
    fn perfect_validation_stops_the_loop() {
        assert!(perfect(1.0));
        assert!(!perfect(0.999));
    }
}
