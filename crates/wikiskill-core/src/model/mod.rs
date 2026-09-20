//! Model clients for the curator roles.
//!
//! Two client paths, as the rationale sets out: OpenCode's own config serves rollouts
//! and daily coding, while the daemon's curator agents call models directly — Bedrock
//! through the AWS SDK's Converse API, Fireworks through an OpenAI-compatible client with
//! a custom base URL.

pub mod openai_compat;

#[cfg(feature = "bedrock")]
pub mod bedrock;

pub mod mock;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    /// Result of a tool the agent loop ran.
    Tool,
}

/// A request from the model to run one tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    #[serde(default)]
    pub content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// Set on [`Role::Tool`] messages: which call this answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Set on [`Role::Tool`] messages: whether the tool failed.
    #[serde(default)]
    pub is_error: bool,
}

impl Message {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            is_error: false,
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            is_error: false,
        }
    }

    pub fn tool_result(id: impl Into<String>, content: impl Into<String>, is_error: bool) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: Some(id.into()),
            is_error,
        }
    }
}

/// A tool as advertised to the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    /// JSON Schema for the tool's arguments.
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Cached input tokens, where the provider reports them. The injected skill block
    /// is identical across an iteration's rollouts, so this is the number that shows
    /// whether prompt caching is absorbing it.
    #[serde(default)]
    pub cached_input_tokens: u64,
}

impl Usage {
    pub fn add(&mut self, other: &Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cached_input_tokens += other.cached_input_tokens;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// The model produced a final answer.
    EndTurn,
    /// The model asked for tools.
    ToolUse,
    /// Output token cap hit.
    MaxTokens,
    Other,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Completion {
    pub text: String,
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    pub stop_reason: StopReason,
    #[serde(default)]
    pub usage: Usage,
}

#[derive(Debug, Clone)]
pub struct CompletionRequest {
    pub system: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSchema>,
    pub max_tokens: u32,
    pub temperature: Option<f32>,
}

/// One model behind one endpoint.
#[async_trait]
pub trait ModelClient: Send + Sync {
    fn model_id(&self) -> &str;
    async fn complete(&self, request: &CompletionRequest) -> Result<Completion>;
}

/// Builds the client for a role from its config, reading credentials from the
/// environment the daemon populated from the Keychain.
pub async fn client_for(model: &crate::config::ModelRef) -> Result<Box<dyn ModelClient>> {
    use crate::config::Endpoint;
    match model.endpoint {
        Endpoint::Fireworks => Ok(Box::new(openai_compat::OpenAiCompatClient::fireworks(
            model.id.clone(),
        )?)),
        #[cfg(feature = "bedrock")]
        Endpoint::BedrockRuntime => Ok(Box::new(
            bedrock::BedrockClient::new(model.id.clone()).await?,
        )),
        #[cfg(not(feature = "bedrock"))]
        Endpoint::BedrockRuntime => anyhow::bail!(
            "model `{}` is pinned to bedrock-runtime, but this build lacks the \
             `bedrock` feature; rebuild with --features bedrock",
            model.id
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_accumulates_across_turns() {
        let mut total = Usage::default();
        total.add(&Usage {
            input_tokens: 10,
            output_tokens: 5,
            cached_input_tokens: 8,
        });
        total.add(&Usage {
            input_tokens: 3,
            output_tokens: 1,
            cached_input_tokens: 0,
        });
        assert_eq!(total.input_tokens, 13);
        assert_eq!(total.output_tokens, 6);
        assert_eq!(total.cached_input_tokens, 8);
    }

    #[tokio::test]
    async fn bedrock_roles_fail_loudly_without_the_feature() {
        let model = crate::config::ModelRef {
            id: "claude-sonnet-5".into(),
            endpoint: crate::config::Endpoint::BedrockRuntime,
            max_tokens: 1024,
            temperature: None,
        };
        let result = client_for(&model).await;
        #[cfg(feature = "bedrock")]
        {
            // Without AWS credentials this may still fail, but never with the
            // "lacks the feature" message.
            if let Err(e) = result {
                assert!(!e.to_string().contains("lacks the"), "{e}");
            }
        }
        #[cfg(not(feature = "bedrock"))]
        match result {
            Ok(_) => panic!("a bedrock-runtime role must not build without the feature"),
            Err(e) => assert!(e.to_string().contains("bedrock"), "{e}"),
        }
    }
}
