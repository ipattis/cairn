//! Raw-layer notes: condensing a transcript, labeling it, and stratified sampling.
//!
//! Traces are condensed before they reach Jev or the maintainer: failed tool calls, the
//! final turns, and — once Jev is live — chunks a per-chunk Noul marks relevant.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::executor::Transcript;
use crate::jev::{Answer, AskOutcome};
use crate::redact::RedactionReport;
use crate::verifier::Score;

/// Jev's labels for one trace, written into the note's frontmatter.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TraceLabels {
    /// Noul: task completed?
    pub completed: Option<bool>,
    /// Choice: failure type, with `other` always available.
    pub failure_type: Option<String>,
    /// Score: severity.
    pub severity: Option<f32>,
    /// Lowest confidence across the three answers.
    pub confidence: Option<f32>,
    /// Pinned Jev version.
    pub labeled_by: Option<String>,
    /// True when any answer fell below the floor: the trace goes to the cockpit's
    /// Labels view for a human pass or fail.
    pub needs_review: bool,
}

impl TraceLabels {
    /// Folds one Jev answer in, tracking the lowest confidence seen.
    pub fn absorb(&mut self, outcome: &AskOutcome) {
        let confidence = outcome.answer.confidence();
        self.confidence = Some(match self.confidence {
            Some(current) => current.min(confidence),
            None => confidence,
        });
        self.labeled_by = Some(outcome.labeled_by.clone());
        if !outcome.confident {
            self.needs_review = true;
        }
        match &outcome.answer {
            Answer::Noul { value, .. } => self.completed = Some(*value),
            Answer::Choice { choice, .. } => self.failure_type = Some(choice.clone()),
            Answer::Score { value, .. } => self.severity = Some(*value),
        }
    }
}

/// One condensed trace, ready to be written to `raw/iter-NNN/<task>.md`.
#[derive(Debug, Clone)]
pub struct RawNote {
    pub iteration: u32,
    pub task_id: String,
    pub model: String,
    pub score: Score,
    pub labels: TraceLabels,
    pub condensed: String,
    pub redaction: RedactionReport,
    /// Vault-relative path of the full transcript.
    pub jsonl_path: String,
    pub created: chrono::DateTime<chrono::Utc>,
}

impl RawNote {
    /// Frontmatter plus the condensed trace. Obsidian reads this; the maintainer reads
    /// it through its `read` tool.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str("---\n");
        out.push_str(&format!("iteration: {}\n", self.iteration));
        out.push_str(&format!("task: {}\n", yaml_string(&self.task_id)));
        out.push_str(&format!("model: {}\n", yaml_string(&self.model)));
        out.push_str(&format!("score: {:.3}\n", self.score.score));
        if let Some(code) = self.score.exit_code {
            out.push_str(&format!("verifier_exit: {code}\n"));
        }
        if self.score.timed_out {
            out.push_str("verifier_timed_out: true\n");
        }
        match self.labels.completed {
            Some(completed) => out.push_str(&format!("completed: {completed}\n")),
            None => out.push_str("completed: null\n"),
        }
        if let Some(failure) = &self.labels.failure_type {
            out.push_str(&format!("failure_type: {}\n", yaml_string(failure)));
        }
        if let Some(severity) = self.labels.severity {
            out.push_str(&format!("severity: {severity:.2}\n"));
        }
        if let Some(confidence) = self.labels.confidence {
            out.push_str(&format!("label_confidence: {confidence:.2}\n"));
        }
        if let Some(labeled_by) = &self.labels.labeled_by {
            out.push_str(&format!("labeled_by: {}\n", yaml_string(labeled_by)));
        }
        if self.labels.needs_review {
            out.push_str("needs_review: true\n");
        }
        out.push_str(&format!("redactions: {}\n", yaml_string(&self.redaction.summary())));
        out.push_str(&format!("full_transcript: {}\n", yaml_string(&self.jsonl_path)));
        out.push_str(&format!("created: {}\n", self.created.to_rfc3339()));
        out.push_str("---\n\n");
        out.push_str(&format!("# {}\n\n", self.task_id));
        out.push_str(&self.condensed);
        if !self.condensed.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("\n## Verifier output\n\n```\n");
        out.push_str(self.score.output_tail.trim_end());
        out.push_str("\n```\n");
        out
    }
}

/// One slice of a transcript, as the condenser and the per-chunk Noul see it.
#[derive(Debug, Clone, PartialEq)]
pub struct Chunk {
    pub label: String,
    pub text: String,
    /// Failed tool calls and the final turns are kept unconditionally.
    pub always_keep: bool,
}

/// Splits a transcript into chunks: failed tool calls first, then the final turns, then
/// everything else in order.
pub fn chunks(transcript: &Transcript, final_turns: usize) -> Vec<Chunk> {
    let mut out = Vec::new();
    let total = transcript.messages.len();
    let final_start = total.saturating_sub(final_turns);

    for (i, message) in transcript.messages.iter().enumerate() {
        let in_final_turns = i >= final_start;
        for call in &message.tool_calls {
            let text = format!(
                "tool `{}` {}\ninput: {}\noutput: {}",
                call.name,
                if call.ok { "ok" } else { "FAILED" },
                compact_json(&call.input),
                call.output.trim()
            );
            out.push(Chunk {
                label: format!("msg{i}:tool:{}", call.name),
                text,
                always_keep: !call.ok || in_final_turns,
            });
        }
        if let Some(reasoning) = &message.reasoning {
            if !reasoning.trim().is_empty() {
                out.push(Chunk {
                    label: format!("msg{i}:reasoning"),
                    text: format!("reasoning: {}", reasoning.trim()),
                    always_keep: in_final_turns,
                });
            }
        }
        if !message.text.trim().is_empty() {
            out.push(Chunk {
                label: format!("msg{i}:{}", message.role),
                text: format!("{}: {}", message.role, message.text.trim()),
                always_keep: in_final_turns,
            });
        }
    }
    out
}

/// Condenses a transcript to at most `max_chars`, keeping failed tool calls and the
/// final turns. `keep_extra` decides optional chunks — that is where the per-chunk Jev
/// Noul plugs in; pass `|_| false` when Jev is off.
pub fn condense(
    transcript: &Transcript,
    final_turns: usize,
    max_chars: usize,
    keep_extra: impl Fn(&Chunk) -> bool,
) -> String {
    let chunks = chunks(transcript, final_turns);
    let mut kept: Vec<&Chunk> = chunks
        .iter()
        .filter(|c| c.always_keep || keep_extra(c))
        .collect();

    let mut total: usize = kept.iter().map(|c| c.text.len() + 2).sum();
    // Drop optional chunks from the front until it fits; the mandatory ones stay.
    while total > max_chars {
        let Some(position) = kept.iter().position(|c| !c.always_keep) else {
            break;
        };
        total -= kept[position].text.len() + 2;
        kept.remove(position);
    }

    let mut out = String::new();
    for chunk in &kept {
        out.push_str(&chunk.text);
        out.push_str("\n\n");
    }
    let mut text = out.trim_end().to_string();
    if text.len() > max_chars {
        text = crate::jev::truncate_to_tokens(&text, max_chars / 4);
    }
    text
}

/// Picks `k` traces spread across failure types, so the maintainer sees the shape of the
/// iteration rather than whichever traces happen to come first.
pub fn sample_stratified(notes: &[RawNote], k: usize) -> Vec<&RawNote> {
    if notes.len() <= k {
        return notes.iter().collect();
    }
    let mut buckets: BTreeMap<String, Vec<&RawNote>> = BTreeMap::new();
    for note in notes {
        let key = note
            .labels
            .failure_type
            .clone()
            .unwrap_or_else(|| match note.labels.completed {
                Some(true) => "completed".into(),
                Some(false) => "unlabelled-failure".into(),
                None => "unlabelled".into(),
            });
        buckets.entry(key).or_default().push(note);
    }
    // Within a bucket, the worst scores first: they carry the most signal.
    for bucket in buckets.values_mut() {
        bucket.sort_by(|a, b| a.score.score.partial_cmp(&b.score.score).unwrap());
    }

    let mut picked = Vec::new();
    let mut round = 0;
    while picked.len() < k {
        let mut progressed = false;
        for bucket in buckets.values() {
            if let Some(note) = bucket.get(round) {
                picked.push(*note);
                progressed = true;
                if picked.len() == k {
                    break;
                }
            }
        }
        if !progressed {
            break;
        }
        round += 1;
    }
    picked
}

fn compact_json(value: &serde_json::Value) -> String {
    let text = value.to_string();
    if text.len() <= 400 {
        text
    } else {
        format!("{}…", &text[..text.floor_char_boundary_compat(400)])
    }
}

/// `str::floor_char_boundary` is unstable, so keep a local equivalent.
trait FloorBoundary {
    fn floor_char_boundary_compat(&self, index: usize) -> usize;
}

impl FloorBoundary for String {
    fn floor_char_boundary_compat(&self, index: usize) -> usize {
        let mut i = index.min(self.len());
        while i > 0 && !self.is_char_boundary(i) {
            i -= 1;
        }
        i
    }
}

fn yaml_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::{TranscriptMessage, TranscriptToolCall};
    use crate::jev::{questions, Answer, AskOutcome};

    fn transcript() -> Transcript {
        Transcript {
            session: "s".into(),
            messages: vec![
                TranscriptMessage {
                    role: "user".into(),
                    text: "fix the failing test".into(),
                    ..Default::default()
                },
                TranscriptMessage {
                    role: "assistant".into(),
                    text: "let me look".into(),
                    tool_calls: vec![
                        TranscriptToolCall {
                            name: "bash".into(),
                            input: serde_json::json!({"command": "cargo test"}),
                            output: "exit 101".into(),
                            ok: false,
                        },
                        TranscriptToolCall {
                            name: "read".into(),
                            input: serde_json::json!({"path": "src/lib.rs"}),
                            output: "fn main…".into(),
                            ok: true,
                        },
                    ],
                    ..Default::default()
                },
                TranscriptMessage {
                    role: "assistant".into(),
                    text: "fixed the ordering".into(),
                    ..Default::default()
                },
            ],
        }
    }

    fn note(task: &str, failure: Option<&str>, score: f64) -> RawNote {
        RawNote {
            iteration: 2,
            task_id: task.into(),
            model: "fireworks-ai/deepseek-v4-pro".into(),
            score: Score {
                task_id: task.into(),
                score,
                exit_code: Some(1),
                timed_out: false,
                output_tail: "2/4".into(),
            },
            labels: TraceLabels {
                failure_type: failure.map(|f| f.to_string()),
                completed: Some(score >= 1.0),
                ..Default::default()
            },
            condensed: "condensed".into(),
            redaction: RedactionReport::default(),
            jsonl_path: "raw/_jsonl/iter-002.t.jsonl".into(),
            created: chrono::Utc::now(),
        }
    }

    #[test]
    fn failed_tool_calls_and_final_turns_are_always_kept() {
        let text = condense(&transcript(), 2, 10_000, |_| false);
        assert!(text.contains("tool `bash` FAILED"), "{text}");
        assert!(text.contains("fixed the ordering"), "{text}");
        // An optional early chunk is dropped when Jev is not there to keep it.
        assert!(!text.contains("fix the failing test"), "{text}");
    }

    #[test]
    fn the_per_chunk_hook_can_keep_extra_chunks() {
        let text = condense(&transcript(), 1, 10_000, |chunk| {
            chunk.label.ends_with(":user")
        });
        assert!(text.contains("fix the failing test"), "{text}");
    }

    #[test]
    fn condensing_respects_a_tight_budget() {
        let text = condense(&transcript(), 3, 120, |_| true);
        assert!(text.len() <= 120, "{} chars: {text}", text.len());
    }

    #[test]
    fn labels_take_the_lowest_confidence_and_flag_review() {
        let mut labels = TraceLabels::default();
        labels.absorb(&AskOutcome {
            answer: Answer::Noul {
                value: false,
                probability: 0.9,
            },
            confident: true,
            actionable: true,
            labeled_by: "jev-1.13.0".into(),
        });
        labels.absorb(&AskOutcome {
            answer: Answer::Choice {
                choice: "loop".into(),
                probability: 0.4,
            },
            confident: false,
            actionable: true,
            labeled_by: "jev-1.13.0".into(),
        });
        assert_eq!(labels.completed, Some(false));
        assert_eq!(labels.failure_type.as_deref(), Some("loop"));
        assert_eq!(labels.confidence, Some(0.4));
        assert!(labels.needs_review);
        assert_eq!(labels.labeled_by.as_deref(), Some("jev-1.13.0"));
        // The question wording lives in one place.
        assert!(questions::task_completed().text().starts_with("Did the agent"));
    }

    #[test]
    fn a_note_renders_parseable_frontmatter() {
        let mut n = note("fix-flaky", Some("repeat-loop"), 0.5);
        n.labels.severity = Some(0.75);
        n.labels.confidence = Some(0.62);
        n.labels.labeled_by = Some("jev-1.13.0".into());
        n.labels.needs_review = true;
        let text = n.render();
        assert!(text.starts_with("---\n"));
        assert!(text.contains("iteration: 2\n"));
        assert!(text.contains("score: 0.500\n"));
        assert!(text.contains("failure_type: \"repeat-loop\"\n"));
        assert!(text.contains("severity: 0.75\n"));
        assert!(text.contains("needs_review: true\n"));
        assert!(text.contains("full_transcript: \"raw/_jsonl/iter-002.t.jsonl\"\n"));
        assert!(text.contains("# fix-flaky\n"));
        assert!(text.contains("## Verifier output"));
    }

    #[test]
    fn unlabelled_traces_still_render_a_completed_key() {
        let n = note("t", None, 0.0);
        assert!(n.render().contains("completed: false\n"));
    }

    #[test]
    fn sampling_spreads_across_failure_types_worst_first() {
        let notes = vec![
            note("a1", Some("loop"), 0.2),
            note("a2", Some("loop"), 0.0),
            note("b1", Some("wrong-file"), 0.1),
            note("c1", None, 1.0),
        ];
        let picked = sample_stratified(&notes, 3);
        let ids: Vec<_> = picked.iter().map(|n| n.task_id.as_str()).collect();
        assert_eq!(ids.len(), 3);
        // One from each bucket before a second from any.
        assert!(ids.contains(&"a2"), "{ids:?}");
        assert!(ids.contains(&"b1"), "{ids:?}");
        assert!(ids.contains(&"c1"), "{ids:?}");
    }

    #[test]
    fn sampling_returns_everything_when_asked_for_more_than_exists() {
        let notes = vec![note("a", None, 0.0)];
        assert_eq!(sample_stratified(&notes, 5).len(), 1);
    }
}
