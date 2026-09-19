//! OpenCode V1 (1.18.x) as the executor, over its HTTP server.
//!
//! The rationale's plan is to generate a Rust client from the pinned OpenAPI spec with
//! `progenitor`. Until that spec is vendored into the repo, this hand-written client
//! stands in: every path and payload shape lives in [`paths`] and [`wire`] so the
//! generated client can replace them in one place. The pinned version is enforced at
//! startup, because V2 revises this API.

use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::{
    Executor, ExecutorInfo, RolloutSpec, SessionId, Transcript, TranscriptMessage,
    TranscriptToolCall,
};
use crate::model::Usage;
use crate::vault::Skill;
use crate::Result;

/// Endpoint paths of the pinned V1 server API.
pub mod paths {
    pub const APP: &str = "/app";
    pub const SESSION: &str = "/session";
    pub fn session_message(id: &str) -> String {
        format!("/session/{id}/message")
    }
    pub fn session(id: &str) -> String {
        format!("/session/{id}")
    }
}

pub struct OpencodeClient {
    http: reqwest::Client,
    base_url: String,
    pinned_version: String,
}

impl OpencodeClient {
    pub fn new(base_url: impl Into<String>, pinned_version: impl Into<String>) -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                // A rollout can legitimately run for many minutes; the loop applies its
                // own per-rollout timeout on top.
                .timeout(Duration::from_secs(3600))
                .build()?,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            pinned_version: pinned_version.into(),
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    /// Fails when the server has left the pinned line, e.g. a V2 binary on the port.
    pub async fn check_pinned(&self) -> Result<ExecutorInfo> {
        let info = self.info().await?;
        if !version_matches(&info.version, &self.pinned_version) {
            anyhow::bail!(
                "opencode at {} reports version {}, which is outside the pinned line {}. \
                 V2 revises the server API: pin the release or write a V2 Executor.",
                self.base_url,
                info.version,
                self.pinned_version
            );
        }
        Ok(info)
    }
}

/// True when `version` is in the `pinned` line, e.g. `1.18.4` in `1.18`.
pub fn version_matches(version: &str, pinned: &str) -> bool {
    let v: Vec<&str> = version.trim_start_matches('v').split('.').collect();
    let p: Vec<&str> = pinned.trim_start_matches('v').split('.').collect();
    if p.is_empty() || v.len() < p.len() {
        return false;
    }
    v.iter().zip(p.iter()).all(|(a, b)| a == b)
}

/// Wire shapes of the pinned V1 API.
mod wire {
    use super::*;

    #[derive(Debug, Deserialize)]
    pub struct App {
        #[serde(default)]
        pub version: Option<String>,
    }

    #[derive(Debug, Serialize)]
    pub struct CreateSession<'a> {
        pub title: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub directory: Option<&'a str>,
    }

    #[derive(Debug, Deserialize)]
    pub struct Session {
        pub id: String,
    }

    #[derive(Debug, Serialize)]
    pub struct PromptRequest<'a> {
        pub agent: &'a str,
        pub model: &'a str,
        pub parts: Vec<Part<'a>>,
    }

    #[derive(Debug, Serialize)]
    pub struct Part<'a> {
        #[serde(rename = "type")]
        pub kind: &'a str,
        pub text: &'a str,
    }

    #[derive(Debug, Deserialize)]
    pub struct MessageEnvelope {
        #[serde(default)]
        pub info: Option<MessageInfo>,
        #[serde(default)]
        pub parts: Vec<MessagePart>,
    }

    #[derive(Debug, Deserialize)]
    pub struct MessageInfo {
        #[serde(default)]
        pub role: Option<String>,
        #[serde(rename = "modelID", default)]
        pub model_id: Option<String>,
        #[serde(default)]
        pub tokens: Option<Tokens>,
    }

    #[derive(Debug, Deserialize)]
    pub struct Tokens {
        #[serde(default)]
        pub input: u64,
        #[serde(default)]
        pub output: u64,
        #[serde(default)]
        pub cache: Option<Cache>,
    }

    #[derive(Debug, Deserialize)]
    pub struct Cache {
        #[serde(default)]
        pub read: u64,
    }

    #[derive(Debug, Deserialize)]
    pub struct MessagePart {
        #[serde(rename = "type")]
        pub kind: String,
        #[serde(default)]
        pub text: Option<String>,
        #[serde(default)]
        pub tool: Option<String>,
        #[serde(default)]
        pub state: Option<ToolState>,
    }

    #[derive(Debug, Deserialize)]
    pub struct ToolState {
        #[serde(default)]
        pub status: Option<String>,
        #[serde(default)]
        pub input: Option<serde_json::Value>,
        #[serde(default)]
        pub output: Option<String>,
        #[serde(default)]
        pub error: Option<String>,
    }
}

/// Maps the server's message envelopes into our transcript format. Public so a captured
/// response body can be replayed against it when the spec moves.
pub fn parse_messages(session: &str, body: &str) -> Result<Transcript> {
    let envelopes: Vec<wire::MessageEnvelope> = serde_json::from_str(body)
        .map_err(|e| anyhow::anyhow!("opencode: unparseable message list: {e}"))?;

    let mut messages = Vec::new();
    for envelope in envelopes {
        let info = envelope.info.unwrap_or(wire::MessageInfo {
            role: None,
            model_id: None,
            tokens: None,
        });
        let mut message = TranscriptMessage {
            role: info.role.unwrap_or_else(|| "unknown".into()),
            model: info.model_id,
            tokens: info.tokens.map(|t| Usage {
                input_tokens: t.input,
                output_tokens: t.output,
                cached_input_tokens: t.cache.map(|c| c.read).unwrap_or(0),
            }),
            ..Default::default()
        };

        let mut text = String::new();
        let mut reasoning = String::new();
        for part in envelope.parts {
            match part.kind.as_str() {
                "text" => {
                    if let Some(t) = part.text {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(&t);
                    }
                }
                "reasoning" => {
                    if let Some(t) = part.text {
                        if !reasoning.is_empty() {
                            reasoning.push('\n');
                        }
                        reasoning.push_str(&t);
                    }
                }
                "tool" => {
                    let state = part.state.unwrap_or(wire::ToolState {
                        status: None,
                        input: None,
                        output: None,
                        error: None,
                    });
                    let ok = state.error.is_none()
                        && matches!(state.status.as_deref(), Some("completed") | None);
                    message.tool_calls.push(TranscriptToolCall {
                        name: part.tool.unwrap_or_else(|| "unknown".into()),
                        input: state.input.unwrap_or(serde_json::Value::Null),
                        output: state
                            .error
                            .or(state.output)
                            .unwrap_or_default(),
                        ok,
                    });
                }
                _ => {}
            }
        }
        message.text = text;
        if !reasoning.is_empty() {
            message.reasoning = Some(reasoning);
        }
        messages.push(message);
    }

    Ok(Transcript {
        session: session.to_string(),
        messages,
    })
}

#[async_trait]
impl Executor for OpencodeClient {
    fn name(&self) -> &str {
        "opencode"
    }

    async fn info(&self) -> Result<ExecutorInfo> {
        let body = self
            .http
            .get(self.url(paths::APP))
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("opencode at {} is unreachable: {e}", self.base_url))?
            .error_for_status()?
            .text()
            .await?;
        let app: wire::App = serde_json::from_str(&body)
            .map_err(|e| anyhow::anyhow!("opencode: unparseable /app response: {e}"))?;
        Ok(ExecutorInfo {
            name: "opencode".into(),
            version: app.version.unwrap_or_else(|| "unknown".into()),
        })
    }

    async fn create_session(&self, title: &str, work_dir: &Path) -> Result<SessionId> {
        let body = self
            .http
            .post(self.url(paths::SESSION))
            .json(&wire::CreateSession {
                title,
                directory: work_dir.to_str(),
            })
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        let session: wire::Session = serde_json::from_str(&body)
            .map_err(|e| anyhow::anyhow!("opencode: unparseable session response: {e}"))?;
        Ok(SessionId(session.id))
    }

    async fn prompt(&self, session: &SessionId, spec: &RolloutSpec) -> Result<()> {
        let response = self
            .http
            .post(self.url(&paths::session_message(&session.0)))
            .json(&wire::PromptRequest {
                agent: &spec.agent,
                model: &spec.model,
                parts: vec![wire::Part {
                    kind: "text",
                    text: &spec.prompt,
                }],
            })
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!(
                "opencode prompt failed ({status}): {}",
                body.chars().take(1000).collect::<String>()
            );
        }
        Ok(())
    }

    async fn transcript(&self, session: &SessionId) -> Result<Transcript> {
        let body = self
            .http
            .get(self.url(&paths::session_message(&session.0)))
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        parse_messages(&session.0, &body)
    }

    async fn delete_session(&self, session: &SessionId) -> Result<()> {
        self.http
            .delete(self.url(&paths::session(&session.0)))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }
}

/// Renders `agents/wiki-inference.md`: the rollout agent, with every active SKILL.md
/// concatenated into its body.
///
/// The `skill` tool stays denied during evolution, so skills arrive only through full
/// injection, as in the paper. The block is byte-identical across an iteration's
/// rollouts so prompt caching can absorb it.
pub fn render_agent_file(agent: &str, model: &str, skills: &[Skill]) -> String {
    let mut out = String::new();
    out.push_str("---\n");
    out.push_str("description: WikiSkill rollouts with injected skills\n");
    out.push_str("mode: primary\n");
    out.push_str(&format!("model: {model}\n"));
    out.push_str("permission:\n");
    out.push_str("  skill: deny\n");
    out.push_str("  external_directory: deny\n");
    out.push_str("  webfetch: deny\n");
    out.push_str("---\n");
    out.push_str(&format!(
        "<!-- generated by wikiskilld for agent `{agent}`; do not edit by hand -->\n"
    ));

    if skills.is_empty() {
        out.push_str(
            "\nYou have no additional skills. Solve the task with the tools you have.\n",
        );
        return out;
    }

    out.push_str("\n# Skills\n\nApply these where they fit the task at hand.\n");
    for skill in skills {
        out.push_str(&format!("\n## {}\n\n", skill.name));
        out.push_str(skill.body.trim_end());
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_version_line_accepts_patches_only() {
        assert!(version_matches("1.18.4", "1.18"));
        assert!(version_matches("v1.18.0", "1.18"));
        assert!(!version_matches("1.19.0", "1.18"));
        assert!(!version_matches("2.0.1", "1.18"));
        assert!(!version_matches("1.18", "1.18.4"));
        assert!(!version_matches("unknown", "1.18"));
    }

    #[test]
    fn parses_reasoning_tool_calls_and_tokens() {
        let body = r#"[
          {"info": {"role": "user"}, "parts": [{"type": "text", "text": "fix it"}]},
          {"info": {"role": "assistant", "modelID": "deepseek-v4-pro",
                    "tokens": {"input": 900, "output": 50, "cache": {"read": 850}}},
           "parts": [
             {"type": "reasoning", "text": "the ordering assert is wrong"},
             {"type": "tool", "tool": "bash",
              "state": {"status": "error", "input": {"command": "cargo test"},
                        "error": "exit 101"}},
             {"type": "tool", "tool": "edit",
              "state": {"status": "completed", "input": {"path": "src/lib.rs"},
                        "output": "ok"}},
             {"type": "text", "text": "done"}
           ]}
        ]"#;
        let transcript = parse_messages("ses_1", body).unwrap();
        assert_eq!(transcript.messages.len(), 2);
        let assistant = &transcript.messages[1];
        assert_eq!(assistant.reasoning.as_deref(), Some("the ordering assert is wrong"));
        assert_eq!(assistant.text, "done");
        assert_eq!(assistant.tool_calls.len(), 2);
        assert!(!assistant.tool_calls[0].ok);
        assert_eq!(assistant.tool_calls[0].output, "exit 101");
        assert!(assistant.tool_calls[1].ok);
        assert_eq!(transcript.usage().cached_input_tokens, 850);
        assert_eq!(transcript.final_answer(), Some("done"));
    }

    #[test]
    fn unknown_part_types_are_ignored_not_fatal() {
        let body = r#"[{"info":{"role":"assistant"},"parts":[
            {"type":"snapshot","text":"x"},{"type":"text","text":"hi"}]}]"#;
        let transcript = parse_messages("s", body).unwrap();
        assert_eq!(transcript.messages[0].text, "hi");
    }

    #[test]
    fn a_bad_body_is_an_error_with_context() {
        let err = parse_messages("s", "not json").unwrap_err().to_string();
        assert!(err.contains("unparseable message list"), "{err}");
    }

    #[test]
    fn agent_file_denies_the_skill_tool_and_injects_every_skill() {
        let skills = vec![
            Skill {
                name: "break-repetition-loop".into(),
                body: "When a command fails twice identically, change approach.\n".into(),
                purpose: None,
            },
            Skill {
                name: "read-before-edit".into(),
                body: "Read a file before editing it.".into(),
                purpose: None,
            },
        ];
        let text = render_agent_file("wiki-inference", "fireworks-ai/deepseek-v4-pro", &skills);
        assert!(text.starts_with("---\n"));
        assert!(text.contains("model: fireworks-ai/deepseek-v4-pro"));
        assert!(text.contains("  skill: deny"));
        assert!(text.contains("  external_directory: deny"));
        assert!(text.contains("  webfetch: deny"));
        assert!(text.contains("## break-repetition-loop"));
        assert!(text.contains("## read-before-edit"));
        assert!(text.contains("change approach."));
    }

    #[test]
    fn agent_file_is_byte_identical_for_the_same_skills() {
        let skills = vec![Skill {
            name: "s".into(),
            body: "do the thing".into(),
            purpose: None,
        }];
        assert_eq!(
            render_agent_file("wiki-inference", "m", &skills),
            render_agent_file("wiki-inference", "m", &skills)
        );
    }

    #[test]
    fn the_empty_skill_baseline_still_renders_a_usable_agent() {
        let text = render_agent_file("wiki-inference", "m", &[]);
        assert!(text.contains("no additional skills"));
        assert!(!text.contains("# Skills"));
    }
}
