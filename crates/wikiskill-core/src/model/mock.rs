//! A scripted model client, so the curator loops, the gate and a whole iteration can be
//! tested without spending a token.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use super::{Completion, CompletionRequest, ModelClient, StopReason, ToolCall, Usage};
use crate::Result;

/// Returns queued completions in order; panics-free once exhausted (it ends the turn).
#[derive(Clone, Default)]
pub struct ScriptedClient {
    model: String,
    script: Arc<Mutex<Vec<Completion>>>,
    /// Every request the loop made, for assertions about prompts and tool schemas.
    pub seen: Arc<Mutex<Vec<CompletionRequest>>>,
}

impl ScriptedClient {
    pub fn new(model: impl Into<String>, script: Vec<Completion>) -> Self {
        Self {
            model: model.into(),
            script: Arc::new(Mutex::new(script)),
            seen: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Convenience: one tool call, then a final answer.
    pub fn tool_then_answer(
        model: impl Into<String>,
        call: ToolCall,
        answer: impl Into<String>,
    ) -> Self {
        Self::new(
            model,
            vec![
                Completion {
                    text: String::new(),
                    tool_calls: vec![call],
                    stop_reason: StopReason::ToolUse,
                    usage: Usage {
                        input_tokens: 10,
                        output_tokens: 5,
                        cached_input_tokens: 0,
                    },
                },
                Completion {
                    text: answer.into(),
                    tool_calls: vec![],
                    stop_reason: StopReason::EndTurn,
                    usage: Usage {
                        input_tokens: 12,
                        output_tokens: 3,
                        cached_input_tokens: 8,
                    },
                },
            ],
        )
    }

    pub fn answer_only(model: impl Into<String>, answer: impl Into<String>) -> Self {
        Self::new(
            model,
            vec![Completion {
                text: answer.into(),
                tool_calls: vec![],
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
            }],
        )
    }

    pub fn requests_made(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
}

#[async_trait]
impl ModelClient for ScriptedClient {
    fn model_id(&self) -> &str {
        &self.model
    }

    async fn complete(&self, request: &CompletionRequest) -> Result<Completion> {
        self.seen.lock().unwrap().push(request.clone());
        let mut script = self.script.lock().unwrap();
        if script.is_empty() {
            return Ok(Completion {
                text: "(scripted client exhausted)".into(),
                tool_calls: vec![],
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
            });
        }
        Ok(script.remove(0))
    }
}

/// A client that always fails, for retry and error-path tests.
pub struct FailingClient {
    pub message: String,
}

#[async_trait]
impl ModelClient for FailingClient {
    fn model_id(&self) -> &str {
        "failing"
    }

    async fn complete(&self, _request: &CompletionRequest) -> Result<Completion> {
        anyhow::bail!("{}", self.message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn scripted_client_replays_in_order_then_ends_the_turn() {
        let client = ScriptedClient::tool_then_answer(
            "m",
            ToolCall {
                id: "c1".into(),
                name: "list".into(),
                arguments: serde_json::json!({}),
            },
            "final",
        );
        let request = CompletionRequest {
            system: None,
            messages: vec![],
            tools: vec![],
            max_tokens: 10,
            temperature: None,
        };
        assert_eq!(
            client.complete(&request).await.unwrap().stop_reason,
            StopReason::ToolUse
        );
        assert_eq!(client.complete(&request).await.unwrap().text, "final");
        assert_eq!(
            client.complete(&request).await.unwrap().stop_reason,
            StopReason::EndTurn
        );
        assert_eq!(client.requests_made(), 3);
    }
}
