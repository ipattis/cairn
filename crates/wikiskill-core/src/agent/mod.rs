//! The native curator agent loop.
//!
//! "The loop is small: send tool schemas, run the requested tools, append results, and
//! stop on a final answer or a turn cap." That is all this is — no sub-agents, no shell,
//! no web, so injected instructions in tool output have nowhere to go.

pub mod maintainer;
pub mod proposer;
pub mod tools;

use crate::model::{
    Completion, CompletionRequest, Message, ModelClient, Role, StopReason, ToolCall, Usage,
};
use crate::Result;

pub use tools::{FileTools, ToolOutput};

/// Why the loop stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stop {
    /// The model gave a final answer.
    FinalAnswer,
    /// The turn cap was hit; whatever edits were made still stand.
    TurnCap,
    /// The model hit its output token cap mid-turn.
    MaxTokens,
}

/// What one curator run produced.
#[derive(Debug, Clone)]
pub struct AgentRun {
    pub final_text: String,
    pub stop: Stop,
    pub turns: u32,
    pub usage: Usage,
    /// Full message list, kept so the daemon can write it into `raw/_jsonl/`.
    pub transcript: Vec<Message>,
    /// Every tool call made, in order, as `(name, ok)`.
    pub tool_calls: Vec<(String, bool)>,
}

/// One curator role: a model, a system prompt, and the four file tools.
pub struct Agent<'a> {
    pub client: &'a dyn ModelClient,
    pub tools: &'a FileTools,
    pub system_prompt: String,
    pub max_tokens: u32,
    pub temperature: Option<f32>,
    pub turn_cap: u32,
}

impl<'a> Agent<'a> {
    pub async fn run(&self, first_message: impl Into<String>) -> Result<AgentRun> {
        let mut messages = vec![Message::user(first_message)];
        let mut usage = Usage::default();
        let mut tool_calls = Vec::new();
        let schemas = self.tools.schemas();

        for turn in 1..=self.turn_cap {
            let request = CompletionRequest {
                system: Some(self.system_prompt.clone()),
                messages: messages.clone(),
                tools: schemas.clone(),
                max_tokens: self.max_tokens,
                temperature: self.temperature,
            };
            let completion: Completion = self.client.complete(&request).await?;
            usage.add(&completion.usage);

            messages.push(Message {
                role: Role::Assistant,
                content: completion.text.clone(),
                tool_calls: completion.tool_calls.clone(),
                tool_call_id: None,
                is_error: false,
            });

            if completion.tool_calls.is_empty() {
                let stop = match completion.stop_reason {
                    StopReason::MaxTokens => Stop::MaxTokens,
                    _ => Stop::FinalAnswer,
                };
                return Ok(AgentRun {
                    final_text: completion.text,
                    stop,
                    turns: turn,
                    usage,
                    transcript: messages,
                    tool_calls,
                });
            }

            for call in &completion.tool_calls {
                let ToolCall {
                    id,
                    name,
                    arguments,
                } = call;
                let output = self.tools.run(name, arguments).await;
                tool_calls.push((name.clone(), !output.is_error));
                tracing::debug!(tool = %name, ok = !output.is_error, "curator tool call");
                messages.push(Message::tool_result(
                    id.clone(),
                    output.content,
                    output.is_error,
                ));
            }
        }

        Ok(AgentRun {
            final_text: messages
                .iter()
                .rev()
                .find(|m| m.role == Role::Assistant)
                .map(|m| m.content.clone())
                .unwrap_or_default(),
            stop: Stop::TurnCap,
            turns: self.turn_cap,
            usage,
            transcript: messages,
            tool_calls,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::mock::{FailingClient, ScriptedClient};
    use crate::model::{StopReason, Usage};
    use crate::vault::{Area, Vault};

    async fn fixture() -> (tempfile::TempDir, Vault) {
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::init(dir.path().join("V")).await.unwrap();
        (dir, vault)
    }

    #[tokio::test]
    async fn runs_tools_then_stops_on_a_final_answer() {
        let (_d, vault) = fixture().await;
        let tools = FileTools::new(vault.clone(), Area::Wiki, vec![Area::Raw]);
        let client = ScriptedClient::tool_then_answer(
            "sonnet",
            ToolCall {
                id: "c1".into(),
                name: "create".into(),
                arguments: serde_json::json!({
                    "path": "wiki/patterns/repeat-loop.md",
                    "contents": "# repeat loop\noccurrences: 1\n"
                }),
            },
            "wrote one pattern",
        );
        let agent = Agent {
            client: &client,
            tools: &tools,
            system_prompt: "you are the wiki maintainer".into(),
            max_tokens: 1024,
            temperature: Some(0.2),
            turn_cap: 8,
        };

        let run = agent.run("here are the traces").await.unwrap();
        assert_eq!(run.stop, Stop::FinalAnswer);
        assert_eq!(run.turns, 2);
        assert_eq!(run.tool_calls, vec![("create".to_string(), true)]);
        assert_eq!(run.usage.input_tokens, 22);
        assert!(vault
            .wiki_dir()
            .join("patterns/repeat-loop.md")
            .is_file());
        // The tool schemas and system prompt go out on every request.
        let seen = client.seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].tools.len(), 4);
        assert_eq!(seen[1].system.as_deref(), Some("you are the wiki maintainer"));
    }

    #[tokio::test]
    async fn a_failed_tool_is_reported_back_and_the_loop_continues() {
        let (_d, vault) = fixture().await;
        let tools = FileTools::new(vault, Area::Wiki, vec![]);
        let client = ScriptedClient::tool_then_answer(
            "sonnet",
            ToolCall {
                id: "c1".into(),
                name: "read".into(),
                arguments: serde_json::json!({"path": "wiki/missing.md"}),
            },
            "recovered",
        );
        let agent = Agent {
            client: &client,
            tools: &tools,
            system_prompt: "sys".into(),
            max_tokens: 100,
            temperature: None,
            turn_cap: 4,
        };
        let run = agent.run("go").await.unwrap();
        assert_eq!(run.tool_calls, vec![("read".to_string(), false)]);
        assert_eq!(run.final_text, "recovered");
        let error_message = run
            .transcript
            .iter()
            .find(|m| m.role == Role::Tool)
            .unwrap();
        assert!(error_message.is_error);
        assert!(error_message.content.contains("no such file"));
    }

    #[tokio::test]
    async fn the_turn_cap_ends_a_looping_agent() {
        let (_d, vault) = fixture().await;
        let tools = FileTools::new(vault, Area::Wiki, vec![]);
        let looping = std::iter::repeat_with(|| Completion {
            text: "still working".into(),
            tool_calls: vec![ToolCall {
                id: "c".into(),
                name: "list".into(),
                arguments: serde_json::json!({"path": "wiki"}),
            }],
            stop_reason: StopReason::ToolUse,
            usage: Usage::default(),
        })
        .take(10)
        .collect();
        let client = ScriptedClient::new("sonnet", looping);
        let agent = Agent {
            client: &client,
            tools: &tools,
            system_prompt: "sys".into(),
            max_tokens: 100,
            temperature: None,
            turn_cap: 3,
        };
        let run = agent.run("go").await.unwrap();
        assert_eq!(run.stop, Stop::TurnCap);
        assert_eq!(run.turns, 3);
        assert_eq!(run.tool_calls.len(), 3);
    }

    #[tokio::test]
    async fn a_model_error_aborts_the_run() {
        let (_d, vault) = fixture().await;
        let tools = FileTools::new(vault, Area::Wiki, vec![]);
        let client = FailingClient {
            message: "bedrock throttled".into(),
        };
        let agent = Agent {
            client: &client,
            tools: &tools,
            system_prompt: "sys".into(),
            max_tokens: 100,
            temperature: None,
            turn_cap: 3,
        };
        let err = agent.run("go").await.unwrap_err().to_string();
        assert!(err.contains("throttled"), "{err}");
    }

    #[tokio::test]
    async fn hitting_the_output_cap_is_distinguishable_from_a_finished_answer() {
        let (_d, vault) = fixture().await;
        let tools = FileTools::new(vault, Area::Wiki, vec![]);
        let client = ScriptedClient::new(
            "sonnet",
            vec![Completion {
                text: "truncated".into(),
                tool_calls: vec![],
                stop_reason: StopReason::MaxTokens,
                usage: Usage::default(),
            }],
        );
        let agent = Agent {
            client: &client,
            tools: &tools,
            system_prompt: "sys".into(),
            max_tokens: 10,
            temperature: None,
            turn_cap: 3,
        };
        assert_eq!(agent.run("go").await.unwrap().stop, Stop::MaxTokens);
    }
}
