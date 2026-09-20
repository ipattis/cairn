//! OpenAI-compatible client for Fireworks.
//!
//! [`OpenAiCompatClient::new`] takes the base URL and key directly, so another
//! OpenAI-compatible provider is a constructor and nothing else.

use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::{
    Completion, CompletionRequest, Message, ModelClient, Role, StopReason, ToolCall, Usage,
};
use crate::Result;

pub const FIREWORKS_BASE_URL: &str = "https://api.fireworks.ai/inference/v1";

pub struct OpenAiCompatClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
    /// Provider name, for error messages.
    provider: &'static str,
}

impl OpenAiCompatClient {
    pub fn new(
        provider: &'static str,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(600))
                .build()?,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            model: model.into(),
            provider,
        })
    }

    /// Fireworks, reading `FIREWORKS_API_KEY` (populated by the daemon from the
    /// Keychain — keys never reach the webview).
    pub fn fireworks(model: String) -> Result<Self> {
        let key = std::env::var("FIREWORKS_API_KEY")
            .map_err(|_| anyhow::anyhow!("FIREWORKS_API_KEY is not set"))?;
        // Strip the provider prefix OpenCode config uses; the API wants the bare ID.
        let model = model
            .strip_prefix("fireworks-ai/")
            .unwrap_or(&model)
            .to_string();
        Self::new("fireworks", FIREWORKS_BASE_URL, key, model)
    }

    /// Builds the wire request. Separated from the HTTP call so the mapping is testable.
    pub fn build_body(&self, request: &CompletionRequest) -> serde_json::Value {
        let mut messages: Vec<WireMessage> = Vec::new();
        if let Some(system) = &request.system {
            messages.push(WireMessage {
                role: "system".into(),
                content: Some(system.clone()),
                tool_calls: None,
                tool_call_id: None,
            });
        }
        for msg in &request.messages {
            messages.push(to_wire(msg));
        }

        let mut body = serde_json::json!({
            "model": self.model,
            "messages": messages,
            "max_tokens": request.max_tokens,
        });
        if let Some(temp) = request.temperature {
            body["temperature"] = serde_json::json!(temp);
        }
        if !request.tools.is_empty() {
            body["tools"] = serde_json::json!(request
                .tools
                .iter()
                .map(|t| serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.input_schema,
                    }
                }))
                .collect::<Vec<_>>());
            body["tool_choice"] = serde_json::json!("auto");
        }
        body
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct WireMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<WireToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct WireToolCall {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    function: WireFunction,
}

#[derive(Debug, Serialize, Deserialize)]
struct WireFunction {
    name: String,
    /// The API carries arguments as a JSON *string*.
    arguments: String,
}

fn to_wire(msg: &Message) -> WireMessage {
    match msg.role {
        Role::System => WireMessage {
            role: "system".into(),
            content: Some(msg.content.clone()),
            tool_calls: None,
            tool_call_id: None,
        },
        Role::User => WireMessage {
            role: "user".into(),
            content: Some(msg.content.clone()),
            tool_calls: None,
            tool_call_id: None,
        },
        Role::Assistant => WireMessage {
            role: "assistant".into(),
            content: if msg.content.is_empty() {
                None
            } else {
                Some(msg.content.clone())
            },
            tool_calls: if msg.tool_calls.is_empty() {
                None
            } else {
                Some(
                    msg.tool_calls
                        .iter()
                        .map(|c| WireToolCall {
                            id: c.id.clone(),
                            kind: "function".into(),
                            function: WireFunction {
                                name: c.name.clone(),
                                arguments: c.arguments.to_string(),
                            },
                        })
                        .collect(),
                )
            },
            tool_call_id: None,
        },
        Role::Tool => WireMessage {
            role: "tool".into(),
            content: Some(msg.content.clone()),
            tool_calls: None,
            tool_call_id: msg.tool_call_id.clone(),
        },
    }
}

#[derive(Debug, Deserialize)]
struct WireResponse {
    #[serde(default)]
    choices: Vec<WireChoice>,
    #[serde(default)]
    usage: Option<WireUsage>,
    #[serde(default)]
    error: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct WireChoice {
    message: WireResponseMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WireResponseMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<WireToolCall>>,
}

#[derive(Debug, Deserialize)]
struct WireUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    #[serde(default)]
    prompt_tokens_details: Option<WirePromptDetails>,
}

#[derive(Debug, Deserialize)]
struct WirePromptDetails {
    #[serde(default)]
    cached_tokens: u64,
}

/// Parses a response body into a [`Completion`]. Public for tests and for replaying
/// captured bodies when a provider changes shape.
pub fn parse_response(provider: &str, body: &str) -> Result<Completion> {
    let parsed: WireResponse = serde_json::from_str(body)
        .map_err(|e| anyhow::anyhow!("{provider}: unparseable response: {e}"))?;
    if let Some(error) = parsed.error {
        anyhow::bail!("{provider} returned an error: {error}");
    }
    let choice = parsed
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("{provider}: response had no choices"))?;

    let mut tool_calls = Vec::new();
    for call in choice.message.tool_calls.unwrap_or_default() {
        let arguments = if call.function.arguments.trim().is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str(&call.function.arguments).map_err(|e| {
                anyhow::anyhow!(
                    "{provider}: tool call `{}` had unparseable arguments: {e}",
                    call.function.name
                )
            })?
        };
        tool_calls.push(ToolCall {
            id: call.id,
            name: call.function.name,
            arguments,
        });
    }

    let stop_reason = match choice.finish_reason.as_deref() {
        Some("tool_calls") => StopReason::ToolUse,
        Some("stop") => StopReason::EndTurn,
        Some("length") => StopReason::MaxTokens,
        _ if !tool_calls.is_empty() => StopReason::ToolUse,
        Some(_) | None => StopReason::EndTurn,
    };

    let usage = parsed
        .usage
        .map(|u| Usage {
            input_tokens: u.prompt_tokens,
            output_tokens: u.completion_tokens,
            cached_input_tokens: u
                .prompt_tokens_details
                .map(|d| d.cached_tokens)
                .unwrap_or(0),
        })
        .unwrap_or_default();

    Ok(Completion {
        text: choice.message.content.unwrap_or_default(),
        tool_calls,
        stop_reason,
        usage,
    })
}

#[async_trait]
impl ModelClient for OpenAiCompatClient {
    fn model_id(&self) -> &str {
        &self.model
    }

    async fn complete(&self, request: &CompletionRequest) -> Result<Completion> {
        let url = format!("{}/chat/completions", self.base_url);
        let response = self
            .http
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&self.build_body(request))
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("{}: request failed: {e}", self.provider))?;
        let status = response.status();
        let body = response.text().await?;
        if !status.is_success() {
            anyhow::bail!(
                "{} returned {}: {}",
                self.provider,
                status,
                body.chars().take(2000).collect::<String>()
            );
        }
        parse_response(self.provider, &body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ToolSchema;

    fn client() -> OpenAiCompatClient {
        OpenAiCompatClient::new("fireworks", FIREWORKS_BASE_URL, "k", "deepseek-v4-pro").unwrap()
    }

    #[test]
    fn tool_arguments_go_out_as_a_json_string() {
        let request = CompletionRequest {
            system: Some("sys".into()),
            messages: vec![
                Message::user("hi"),
                Message {
                    role: Role::Assistant,
                    content: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "c1".into(),
                        name: "read".into(),
                        arguments: serde_json::json!({"path": "wiki/index.md"}),
                    }],
                    tool_call_id: None,
                    is_error: false,
                },
                Message::tool_result("c1", "# index", false),
            ],
            tools: vec![ToolSchema {
                name: "read".into(),
                description: "read a file".into(),
                input_schema: serde_json::json!({"type": "object"}),
            }],
            max_tokens: 100,
            temperature: Some(0.2),
        };
        let body = client().build_body(&request);
        assert_eq!(body["model"], "deepseek-v4-pro");
        assert_eq!(body["messages"][0]["role"], "system");
        let args = body["messages"][2]["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .expect("arguments must be a string, not an object");
        assert_eq!(args, "{\"path\":\"wiki/index.md\"}");
        assert_eq!(body["messages"][3]["tool_call_id"], "c1");
        assert_eq!(body["tools"][0]["function"]["name"], "read");
    }

    #[test]
    fn parses_a_tool_call_response_with_cache_stats() {
        let body = r#"{
          "choices": [{
            "finish_reason": "tool_calls",
            "message": {
              "content": null,
              "tool_calls": [{
                "id": "call_1",
                "type": "function",
                "function": {"name": "str_replace", "arguments": "{\"path\":\"a.md\"}"}
              }]
            }
          }],
          "usage": {
            "prompt_tokens": 120,
            "completion_tokens": 8,
            "prompt_tokens_details": {"cached_tokens": 100}
          }
        }"#;
        let completion = parse_response("fireworks", body).unwrap();
        assert_eq!(completion.stop_reason, StopReason::ToolUse);
        assert_eq!(completion.tool_calls[0].name, "str_replace");
        assert_eq!(completion.tool_calls[0].arguments["path"], "a.md");
        assert_eq!(completion.usage.cached_input_tokens, 100);
        assert_eq!(completion.text, "");
    }

    #[test]
    fn parses_a_final_answer() {
        let body = r#"{"choices":[{"finish_reason":"stop","message":{"content":"done"}}]}"#;
        let completion = parse_response("fireworks", body).unwrap();
        assert_eq!(completion.stop_reason, StopReason::EndTurn);
        assert_eq!(completion.text, "done");
        assert!(completion.tool_calls.is_empty());
    }

    #[test]
    fn surfaces_provider_errors_and_bad_tool_arguments() {
        let err = parse_response("fireworks", r#"{"error":{"message":"no such model"}}"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no such model"), "{err}");

        let body = r#"{"choices":[{"finish_reason":"tool_calls","message":{"tool_calls":[
            {"id":"c","type":"function","function":{"name":"read","arguments":"{oops"}}]}}]}"#;
        let err = parse_response("fireworks", body).unwrap_err().to_string();
        assert!(err.contains("unparseable arguments"), "{err}");
    }

    #[test]
    fn empty_tool_arguments_become_an_empty_object() {
        let body = r#"{"choices":[{"finish_reason":"tool_calls","message":{"tool_calls":[
            {"id":"c","type":"function","function":{"name":"list","arguments":""}}]}}]}"#;
        let completion = parse_response("fireworks", body).unwrap();
        assert_eq!(completion.tool_calls[0].arguments, serde_json::json!({}));
    }

    #[test]
    fn provider_prefixes_are_stripped_from_model_ids() {
        // SAFETY-free: only this test touches the var, and it is set before use.
        std::env::set_var("FIREWORKS_API_KEY", "fw_test");
        let client =
            OpenAiCompatClient::fireworks("fireworks-ai/deepseek-v4-pro".into()).unwrap();
        assert_eq!(client.model_id(), "deepseek-v4-pro");
        std::env::remove_var("FIREWORKS_API_KEY");
    }
}
