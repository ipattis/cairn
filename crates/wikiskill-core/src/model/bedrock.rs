//! Bedrock runtime via the Converse API.
//!
//! The curator roles run on Claude through Bedrock: Sonnet 5 maintains the wiki, Opus 5
//! proposes skills. Converse is the provider-agnostic shape, so swapping the model ID is
//! a config edit rather than a rewrite.
//!
//! This lives behind the non-default `bedrock` feature: the AWS SDK is a large dependency
//! and the Fireworks/Mantle path needs none of it, so the default build stays light and
//! entirely offline-testable. [`super::client_for`] fails with a rebuild instruction when a
//! role is pinned to `bedrock-runtime` in a build without the feature.

use async_trait::async_trait;
use aws_sdk_bedrockruntime::types as bt;
use aws_smithy_types::Document;

use super::{Completion, CompletionRequest, Message, ModelClient, Role, StopReason, ToolCall, Usage};
use crate::Result;

pub struct BedrockClient {
    client: aws_sdk_bedrockruntime::Client,
    model_id: String,
}

impl BedrockClient {
    /// Credentials come from the standard AWS chain, which the daemon seeds from the
    /// Keychain into its own environment only — never the webview's.
    pub async fn new(model_id: impl Into<String>) -> Result<Self> {
        let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
        Ok(Self {
            client: aws_sdk_bedrockruntime::Client::new(&config),
            model_id: model_id.into(),
        })
    }
}

#[async_trait]
impl ModelClient for BedrockClient {
    fn model_id(&self) -> &str {
        &self.model_id
    }

    async fn complete(&self, request: &CompletionRequest) -> Result<Completion> {
        let mut call = self
            .client
            .converse()
            .model_id(&self.model_id)
            .set_messages(Some(to_bedrock_messages(&request.messages)?));

        if let Some(system) = &request.system {
            call = call.system(bt::SystemContentBlock::Text(system.clone()));
        }

        let mut inference = bt::InferenceConfiguration::builder().max_tokens(
            i32::try_from(request.max_tokens).unwrap_or(i32::MAX),
        );
        if let Some(temperature) = request.temperature {
            inference = inference.temperature(temperature);
        }
        call = call.inference_config(inference.build());

        if !request.tools.is_empty() {
            let mut tools = Vec::new();
            for tool in &request.tools {
                let spec = bt::ToolSpecification::builder()
                    .name(&tool.name)
                    .description(&tool.description)
                    .input_schema(bt::ToolInputSchema::Json(to_document(&tool.input_schema)))
                    .build()
                    .map_err(|e| anyhow::anyhow!("bedrock: bad tool spec: {e}"))?;
                tools.push(bt::Tool::ToolSpec(spec));
            }
            call = call.tool_config(
                bt::ToolConfiguration::builder()
                    .set_tools(Some(tools))
                    .build()
                    .map_err(|e| anyhow::anyhow!("bedrock: bad tool config: {e}"))?,
            );
        }

        let response = call
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("bedrock converse failed: {}", describe(e)))?;

        let mut text = String::new();
        let mut tool_calls = Vec::new();
        if let Some(bt::ConverseOutput::Message(message)) = response.output() {
            for block in message.content() {
                match block {
                    bt::ContentBlock::Text(chunk) => text.push_str(chunk),
                    bt::ContentBlock::ToolUse(use_block) => tool_calls.push(ToolCall {
                        id: use_block.tool_use_id().to_string(),
                        name: use_block.name().to_string(),
                        arguments: from_document(use_block.input()),
                    }),
                    // Reasoning and other block kinds carry no instruction for the loop.
                    _ => {}
                }
            }
        }

        let usage = response
            .usage()
            .map(|u| Usage {
                input_tokens: u.input_tokens().max(0) as u64,
                output_tokens: u.output_tokens().max(0) as u64,
                cached_input_tokens: u.cache_read_input_tokens().unwrap_or(0).max(0) as u64,
            })
            .unwrap_or_default();

        Ok(Completion {
            text,
            tool_calls,
            stop_reason: match response.stop_reason() {
                bt::StopReason::EndTurn | bt::StopReason::StopSequence => StopReason::EndTurn,
                bt::StopReason::ToolUse => StopReason::ToolUse,
                bt::StopReason::MaxTokens => StopReason::MaxTokens,
                _ => StopReason::Other,
            },
            usage,
        })
    }
}

/// Converse takes alternating user/assistant messages, and tool results are user-role
/// content blocks rather than a role of their own.
fn to_bedrock_messages(messages: &[Message]) -> Result<Vec<bt::Message>> {
    let mut out: Vec<bt::Message> = Vec::new();
    for message in messages {
        let (role, blocks) = match message.role {
            Role::System => {
                // A system message inside the list is a caller mistake: Converse takes
                // system prompts in their own field.
                anyhow::bail!(
                    "bedrock: pass the system prompt in CompletionRequest::system, not \
                     as a message"
                );
            }
            Role::User => (bt::ConversationRole::User, vec![text_block(&message.content)]),
            Role::Assistant => {
                let mut blocks = Vec::new();
                if !message.content.trim().is_empty() {
                    blocks.push(text_block(&message.content));
                }
                for call in &message.tool_calls {
                    blocks.push(bt::ContentBlock::ToolUse(
                        bt::ToolUseBlock::builder()
                            .tool_use_id(&call.id)
                            .name(&call.name)
                            .input(to_document(&call.arguments))
                            .build()
                            .map_err(|e| anyhow::anyhow!("bedrock: bad tool use: {e}"))?,
                    ));
                }
                (bt::ConversationRole::Assistant, blocks)
            }
            Role::Tool => {
                let id = message
                    .tool_call_id
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("bedrock: tool result without a call id"))?;
                let block = bt::ToolResultBlock::builder()
                    .tool_use_id(id)
                    .content(bt::ToolResultContentBlock::Text(message.content.clone()))
                    .status(if message.is_error {
                        bt::ToolResultStatus::Error
                    } else {
                        bt::ToolResultStatus::Success
                    })
                    .build()
                    .map_err(|e| anyhow::anyhow!("bedrock: bad tool result: {e}"))?;
                (
                    bt::ConversationRole::User,
                    vec![bt::ContentBlock::ToolResult(block)],
                )
            }
        };

        // Consecutive same-role messages must be merged; Converse rejects them otherwise,
        // and the agent loop naturally produces several tool results in a row.
        match out.last_mut() {
            Some(last) if last.role() == &role => {
                let mut merged = last.content().to_vec();
                merged.extend(blocks);
                *last = bt::Message::builder()
                    .role(role)
                    .set_content(Some(merged))
                    .build()
                    .map_err(|e| anyhow::anyhow!("bedrock: bad message: {e}"))?;
            }
            _ => out.push(
                bt::Message::builder()
                    .role(role)
                    .set_content(Some(blocks))
                    .build()
                    .map_err(|e| anyhow::anyhow!("bedrock: bad message: {e}"))?,
            ),
        }
    }
    Ok(out)
}

fn text_block(text: &str) -> bt::ContentBlock {
    // Converse rejects empty text blocks.
    bt::ContentBlock::Text(if text.is_empty() {
        " ".to_string()
    } else {
        text.to_string()
    })
}

/// Smithy's `Document` is the Converse API's JSON, so tool schemas and arguments need a
/// conversion in each direction.
pub fn to_document(value: &serde_json::Value) -> Document {
    match value {
        serde_json::Value::Null => Document::Null,
        serde_json::Value::Bool(b) => Document::Bool(*b),
        serde_json::Value::Number(n) => {
            use aws_smithy_types::Number;
            if let Some(i) = n.as_i64() {
                Document::Number(Number::NegInt(i))
            } else if let Some(u) = n.as_u64() {
                Document::Number(Number::PosInt(u))
            } else {
                Document::Number(Number::Float(n.as_f64().unwrap_or_default()))
            }
        }
        serde_json::Value::String(s) => Document::String(s.clone()),
        serde_json::Value::Array(items) => {
            Document::Array(items.iter().map(to_document).collect())
        }
        serde_json::Value::Object(map) => Document::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), to_document(v)))
                .collect(),
        ),
    }
}

pub fn from_document(document: &Document) -> serde_json::Value {
    use aws_smithy_types::Number;
    match document {
        Document::Null => serde_json::Value::Null,
        Document::Bool(b) => serde_json::Value::Bool(*b),
        Document::Number(n) => match n {
            Number::PosInt(u) => serde_json::json!(u),
            Number::NegInt(i) => serde_json::json!(i),
            Number::Float(f) => serde_json::json!(f),
        },
        Document::String(s) => serde_json::Value::String(s.clone()),
        Document::Array(items) => {
            serde_json::Value::Array(items.iter().map(from_document).collect())
        }
        Document::Object(map) => serde_json::Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), from_document(v)))
                .collect(),
        ),
    }
}

/// SDK errors print as `ServiceError` without their cause by default; the cause is the
/// part worth logging.
fn describe<E: std::error::Error>(error: E) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(&format!(": {cause}"));
        source = cause.source();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Message, ToolCall};

    #[test]
    fn json_round_trips_through_smithy_documents() {
        let value = serde_json::json!({
            "path": "wiki/patterns/loop.md",
            "count": 3,
            "ratio": 0.5,
            "ok": true,
            "tags": ["a", "b"],
            "nested": {"empty": null}
        });
        assert_eq!(from_document(&to_document(&value)), value);
    }

    #[test]
    fn consecutive_tool_results_are_merged_into_one_user_turn() {
        let messages = vec![
            Message::user("start"),
            Message {
                role: crate::model::Role::Assistant,
                content: String::new(),
                tool_calls: vec![
                    ToolCall {
                        id: "1".into(),
                        name: "read".into(),
                        arguments: serde_json::json!({"path": "a"}),
                    },
                    ToolCall {
                        id: "2".into(),
                        name: "read".into(),
                        arguments: serde_json::json!({"path": "b"}),
                    },
                ],
                tool_call_id: None,
                is_error: false,
            },
            Message::tool_result("1", "contents of a", false),
            Message::tool_result("2", "no such file", true),
        ];
        let converted = to_bedrock_messages(&messages).unwrap();
        assert_eq!(converted.len(), 3);
        assert_eq!(converted[2].role(), &bt::ConversationRole::User);
        assert_eq!(converted[2].content().len(), 2);
    }

    #[test]
    fn a_system_message_in_the_list_is_refused() {
        let err = to_bedrock_messages(&[Message {
            role: crate::model::Role::System,
            content: "you are".into(),
            tool_calls: vec![],
            tool_call_id: None,
            is_error: false,
        }])
        .unwrap_err()
        .to_string();
        assert!(err.contains("CompletionRequest::system"), "{err}");
    }
}
