//! The executor boundary.
//!
//! Every call the daemon makes to the rollout executor goes through this trait, so a new
//! executor — pi was the runner-up — or a new major version of OpenCode means writing one
//! more implementation and nothing else. That has already paid for itself once: V2's API
//! differs from V1's in authentication, path layout, prompt semantics and where per-session
//! policy lives, and none of it reached the loop.

pub mod mock;
pub mod opencode;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::Result;

/// A session on the executor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionId(pub String);

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What the executor reports about itself. Checked against the pinned version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutorInfo {
    pub name: String,
    pub version: String,
}

/// One tool call as it appears in a transcript.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TranscriptToolCall {
    pub name: String,
    #[serde(default)]
    pub input: serde_json::Value,
    #[serde(default)]
    pub output: String,
    /// False when the executor reported the tool errored — the signal the curator
    /// agents care most about when condensing a trace.
    #[serde(default)]
    pub ok: bool,
}

/// One message in a rollout transcript. This is the parseable per-session format the
/// raw layer stores: reasoning, tool calls, outputs and answers.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TranscriptMessage {
    /// `user`, `assistant`, or a tool role, as the executor reported it.
    pub role: String,
    #[serde(default)]
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<TranscriptToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default)]
    pub tokens: Option<crate::model::Usage>,
}

/// A full rollout transcript.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Transcript {
    pub session: String,
    pub messages: Vec<TranscriptMessage>,
}

impl Transcript {
    /// One JSON object per line — the format written to `raw/_jsonl/`.
    pub fn to_jsonl(&self) -> Result<String> {
        let mut out = String::new();
        for message in &self.messages {
            out.push_str(&serde_json::to_string(message)?);
            out.push('\n');
        }
        Ok(out)
    }

    pub fn final_answer(&self) -> Option<&str> {
        self.messages
            .iter()
            .rev()
            .find(|m| m.role == "assistant" && !m.text.trim().is_empty())
            .map(|m| m.text.as_str())
    }

    pub fn failed_tool_calls(&self) -> Vec<&TranscriptToolCall> {
        self.messages
            .iter()
            .flat_map(|m| m.tool_calls.iter())
            .filter(|c| !c.ok)
            .collect()
    }

    pub fn usage(&self) -> crate::model::Usage {
        let mut total = crate::model::Usage::default();
        for message in &self.messages {
            if let Some(usage) = &message.tokens {
                total.add(usage);
            }
        }
        total
    }
}

/// One rollout to run.
#[derive(Debug, Clone)]
pub struct RolloutSpec {
    /// Agent name in the executor's config, e.g. `wiki-inference`.
    pub agent: String,
    /// Model ID to run it on. Per-run swappable: that is requirement R6.
    pub model: String,
    /// The task prompt.
    pub prompt: String,
    /// Working directory for the session — the agent's private copy of the workspace.
    pub work_dir: std::path::PathBuf,
    /// Session title, for the executor's own UI and logs.
    pub title: String,
}

#[async_trait]
pub trait Executor: Send + Sync {
    fn name(&self) -> &str;

    /// Version and liveness check. The daemon refuses to run if the version leaves the
    /// pinned line.
    async fn info(&self) -> Result<ExecutorInfo>;

    /// Opens a session for one rollout.
    ///
    /// Takes the whole spec rather than a title and a directory because that is where the
    /// executor binds the agent, the model and the permission ruleset: in V2 they are session
    /// properties, not prompt properties.
    async fn create_session(&self, spec: &RolloutSpec) -> Result<SessionId>;

    /// Sends the prompt and waits for the session to go idle.
    async fn prompt(&self, session: &SessionId, spec: &RolloutSpec) -> Result<()>;

    async fn transcript(&self, session: &SessionId) -> Result<Transcript>;

    async fn delete_session(&self, session: &SessionId) -> Result<()>;

    /// Create, prompt, fetch, delete. Overridable where an executor can do better.
    async fn run_rollout(&self, spec: &RolloutSpec) -> Result<Transcript> {
        let session = self.create_session(spec).await?;
        let result = async {
            self.prompt(&session, spec).await?;
            self.transcript(&session).await
        }
        .await;
        // Leave no sessions behind, even on failure.
        if let Err(e) = self.delete_session(&session).await {
            tracing::warn!(%session, "could not delete session: {e}");
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transcript() -> Transcript {
        Transcript {
            session: "ses_1".into(),
            messages: vec![
                TranscriptMessage {
                    role: "user".into(),
                    text: "fix the failing test".into(),
                    ..Default::default()
                },
                TranscriptMessage {
                    role: "assistant".into(),
                    text: String::new(),
                    reasoning: Some("the test asserts on ordering".into()),
                    tool_calls: vec![
                        TranscriptToolCall {
                            name: "bash".into(),
                            input: serde_json::json!({"command": "cargo test"}),
                            output: "1 failed".into(),
                            ok: false,
                        },
                        TranscriptToolCall {
                            name: "edit".into(),
                            input: serde_json::json!({"path": "src/lib.rs"}),
                            output: "edited".into(),
                            ok: true,
                        },
                    ],
                    tokens: Some(crate::model::Usage {
                        input_tokens: 100,
                        output_tokens: 20,
                        cached_input_tokens: 90,
                    }),
                    ..Default::default()
                },
                TranscriptMessage {
                    role: "assistant".into(),
                    text: "fixed".into(),
                    tokens: Some(crate::model::Usage {
                        input_tokens: 40,
                        output_tokens: 5,
                        cached_input_tokens: 30,
                    }),
                    ..Default::default()
                },
            ],
        }
    }

    #[test]
    fn jsonl_is_one_object_per_message() {
        let jsonl = transcript().to_jsonl().unwrap();
        assert_eq!(jsonl.lines().count(), 3);
        for line in jsonl.lines() {
            serde_json::from_str::<TranscriptMessage>(line).unwrap();
        }
    }

    #[test]
    fn transcript_exposes_what_the_curators_need() {
        let t = transcript();
        assert_eq!(t.final_answer(), Some("fixed"));
        assert_eq!(t.failed_tool_calls().len(), 1);
        assert_eq!(t.failed_tool_calls()[0].name, "bash");
        let usage = t.usage();
        assert_eq!(usage.input_tokens, 140);
        assert_eq!(usage.cached_input_tokens, 120);
    }

    #[test]
    fn a_transcript_with_no_answer_reports_none() {
        let mut t = transcript();
        t.messages.retain(|m| m.role != "assistant" || m.text.is_empty());
        assert_eq!(t.final_answer(), None);
    }
}
