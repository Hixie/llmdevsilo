//! Anthropic Messages API backend.
//!
//! Converts the provider-agnostic [`CompletionRequest`] to a
//! `POST {base}/v1/messages` call, parses the response back into
//! conversation types, and meters token and dollar usage. Quota is checked
//! before every request; HTTP 429/5xx and transport failures are retried
//! with exponential backoff. The API key lives in a
//! [`SecretString`] and is sent only as the `x-api-key` header.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use silo_core::config::LlmConfig;
use silo_core::conversation::{
    CompletionRequest, CompletionResponse, ContentBlock, Message, Role, StopReason, TokenDelta,
};
use silo_core::cost::{QuotaConfig, UsageMeter, UsageSnapshot};
use silo_core::error::LlmError;
use silo_core::secrets::SecretString;
use silo_core::traits::LlmBackend;

use crate::common;

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const DEFAULT_API_KEY_ENV: &str = "ANTHROPIC_API_KEY";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const MAX_ATTEMPTS: u32 = 3;
const RETRY_BASE_DELAY: Duration = Duration::from_millis(250);
const ERROR_BODY_EXCERPT_CHARS: usize = 500;

/// Creates an Anthropic backend from the configuration. The API key is read
/// from the environment variable named by `api_key_env`
/// (`ANTHROPIC_API_KEY` by default). Pricing comes from the configuration,
/// falling back to the built-in price table for known models and to zero
/// pricing otherwise.
pub async fn create(config: &LlmConfig) -> Result<Arc<dyn LlmBackend>, LlmError> {
    let key_env = config
        .api_key_env
        .clone()
        .unwrap_or_else(|| DEFAULT_API_KEY_ENV.to_string());
    let api_key = match std::env::var(&key_env) {
        Ok(value) if !value.is_empty() => SecretString::new(value),
        _ => {
            return Err(LlmError::Config(format!(
                "environment variable {key_env} is not set"
            )));
        }
    };
    let base_url = config
        .base_url
        .clone()
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
    let base_url = base_url.trim_end_matches('/').to_string();
    let pricing = config
        .pricing
        .or_else(|| common::default_pricing_for_model(&config.model))
        .unwrap_or_default();
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .map_err(|e| LlmError::Config(format!("failed to build http client: {e}")))?;
    Ok(Arc::new(AnthropicBackend {
        client,
        base_url,
        api_key,
        model: config.model.clone(),
        max_tokens: config.max_tokens,
        meter: UsageMeter::new(pricing, config.quota),
    }))
}

/// The key field is a [`SecretString`], so `Debug` output shows it as
/// `[redacted]`.
#[derive(Debug)]
struct AnthropicBackend {
    client: reqwest::Client,
    base_url: String,
    api_key: SecretString,
    model: String,
    max_tokens: u32,
    meter: UsageMeter,
}

#[async_trait]
impl LlmBackend for AnthropicBackend {
    fn id(&self) -> String {
        format!("anthropic:{}", self.model)
    }

    async fn complete(&self, request: &CompletionRequest) -> Result<CompletionResponse, LlmError> {
        self.meter.check_quota()?;
        let url = format!("{}/v1/messages", self.base_url);
        let body = build_body(&self.model, self.max_tokens, request);
        let response = common::retry_with_backoff(MAX_ATTEMPTS, RETRY_BASE_DELAY, || {
            let client = &self.client;
            let url = &url;
            let body = &body;
            let api_key = &self.api_key;
            async move {
                let http_response = client
                    .post(url)
                    .header("x-api-key", api_key.expose())
                    .header("anthropic-version", ANTHROPIC_VERSION)
                    .header("content-type", "application/json")
                    .json(body)
                    .send()
                    .await
                    .map_err(|e| LlmError::Transport(e.to_string()))?;
                let status = http_response.status();
                let text = http_response
                    .text()
                    .await
                    .map_err(|e| LlmError::Transport(e.to_string()))?;
                if !status.is_success() {
                    return Err(LlmError::Api(format!(
                        "status {}: {}",
                        status.as_u16(),
                        excerpt(&text)
                    )));
                }
                parse_response(&text)
            }
        })
        .await?;
        self.meter.record(response.usage);
        Ok(response)
    }

    fn usage(&self) -> UsageSnapshot {
        self.meter.snapshot()
    }

    fn quota(&self) -> QuotaConfig {
        self.meter.quota()
    }
}

/// Builds the Messages API request body. `system` and `tools` are included
/// only when non-empty. The request's `max_tokens` is used when set;
/// a zero value falls back to the configured default.
fn build_body(
    model: &str,
    default_max_tokens: u32,
    request: &CompletionRequest,
) -> serde_json::Value {
    let max_tokens = if request.max_tokens > 0 {
        request.max_tokens
    } else {
        default_max_tokens
    };
    let messages: Vec<serde_json::Value> = request.messages.iter().map(message_to_wire).collect();
    let mut body = json!({
        "model": model,
        "max_tokens": max_tokens,
        "messages": messages,
    });
    if !request.system.is_empty() {
        body["system"] = json!(request.system);
    }
    if !request.tools.is_empty() {
        let tools: Vec<serde_json::Value> = request
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "name": tool.name,
                    "description": tool.description,
                    "input_schema": tool.input_schema,
                })
            })
            .collect();
        body["tools"] = json!(tools);
    }
    body
}

fn message_to_wire(message: &Message) -> serde_json::Value {
    let role = match message.role {
        Role::User => "user",
        Role::Assistant => "assistant",
    };
    let content: Vec<serde_json::Value> = message
        .content
        .iter()
        .map(|block| match block {
            ContentBlock::Text { text } => json!({"type": "text", "text": text}),
            ContentBlock::ToolUse { id, name, input } => {
                json!({"type": "tool_use", "id": id, "name": name, "input": input})
            }
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                json!({
                    "type": "tool_result",
                    "tool_use_id": tool_use_id,
                    "content": content,
                    "is_error": is_error,
                })
            }
        })
        .collect();
    json!({"role": role, "content": content})
}

#[derive(Deserialize)]
struct WireResponse {
    #[serde(default)]
    content: Vec<WireContentBlock>,
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default)]
    usage: WireUsage,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    /// Content block types this backend does not consume (for example
    /// `thinking`); skipped during conversion.
    #[serde(other)]
    Unknown,
}

#[derive(Default, Deserialize)]
struct WireUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
}

fn parse_response(text: &str) -> Result<CompletionResponse, LlmError> {
    let wire: WireResponse = serde_json::from_str(text)
        .map_err(|e| LlmError::Malformed(format!("invalid response JSON: {e}")))?;
    let content = wire
        .content
        .into_iter()
        .filter_map(|block| match block {
            WireContentBlock::Text { text } => Some(ContentBlock::Text { text }),
            WireContentBlock::ToolUse { id, name, input } => {
                Some(ContentBlock::ToolUse { id, name, input })
            }
            WireContentBlock::Unknown => None,
        })
        .collect();
    Ok(CompletionResponse {
        content,
        stop_reason: map_stop_reason(wire.stop_reason.as_deref()),
        usage: TokenDelta {
            input_tokens: wire.usage.input_tokens,
            output_tokens: wire.usage.output_tokens,
        },
    })
}

fn map_stop_reason(value: Option<&str>) -> StopReason {
    match value {
        Some("end_turn") => StopReason::EndTurn,
        Some("tool_use") => StopReason::ToolUse,
        Some("max_tokens") => StopReason::MaxTokens,
        Some(other) => StopReason::Other(other.to_string()),
        None => StopReason::Other("(none)".to_string()),
    }
}

/// First [`ERROR_BODY_EXCERPT_CHARS`] characters of an error body, for
/// inclusion in [`LlmError::Api`] messages.
fn excerpt(body: &str) -> String {
    body.trim().chars().take(ERROR_BODY_EXCERPT_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use silo_core::cost::Pricing;
    use silo_core::tool::{ToolAvailability, ToolDef};

    fn test_backend() -> AnthropicBackend {
        AnthropicBackend {
            client: reqwest::Client::builder().no_proxy().build().unwrap(),
            base_url: "http://127.0.0.1:1".into(),
            api_key: SecretString::new("sk-super-secret"),
            model: "claude-test".into(),
            max_tokens: 16,
            meter: UsageMeter::new(Pricing::default(), QuotaConfig::default()),
        }
    }

    #[test]
    fn body_contains_full_conversation_system_and_tools() {
        let request = CompletionRequest {
            system: "sys".into(),
            messages: vec![
                Message::user_text("hello"),
                Message::assistant(vec![
                    ContentBlock::Text { text: "hi".into() },
                    ContentBlock::ToolUse {
                        id: "t1".into(),
                        name: "Bash".into(),
                        input: json!({"command": "ls"}),
                    },
                ]),
                Message {
                    role: Role::User,
                    content: vec![ContentBlock::ToolResult {
                        tool_use_id: "t1".into(),
                        content: "no such directory".into(),
                        is_error: true,
                    }],
                },
            ],
            tools: vec![ToolDef {
                name: "Bash".into(),
                description: "Runs a command.".into(),
                input_schema: json!({"type": "object"}),
                availability: ToolAvailability::Both,
            }],
            max_tokens: 99,
        };
        let body = build_body("claude-test", 8192, &request);
        assert_eq!(
            body,
            json!({
                "model": "claude-test",
                "max_tokens": 99,
                "system": "sys",
                "messages": [
                    {"role": "user", "content": [{"type": "text", "text": "hello"}]},
                    {"role": "assistant", "content": [
                        {"type": "text", "text": "hi"},
                        {"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "ls"}},
                    ]},
                    {"role": "user", "content": [
                        {
                            "type": "tool_result",
                            "tool_use_id": "t1",
                            "content": "no such directory",
                            "is_error": true,
                        },
                    ]},
                ],
                "tools": [{
                    "name": "Bash",
                    "description": "Runs a command.",
                    "input_schema": {"type": "object"},
                }],
            })
        );
    }

    #[test]
    fn body_omits_empty_system_and_tools_and_uses_config_max_tokens() {
        let request = CompletionRequest {
            system: String::new(),
            messages: vec![Message::user_text("hi")],
            tools: vec![],
            max_tokens: 0,
        };
        let body = build_body("claude-test", 4096, &request);
        assert_eq!(body["max_tokens"], 4096);
        assert!(body.get("system").is_none());
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn parse_maps_text_and_tool_use_blocks() {
        let text = json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "content": [
                {"type": "text", "text": "Checking."},
                {"type": "tool_use", "id": "t2", "name": "Read", "input": {"path": "/x"}},
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 11, "output_tokens": 22},
        })
        .to_string();
        let response = parse_response(&text).unwrap();
        assert_eq!(
            response.content,
            vec![
                ContentBlock::Text {
                    text: "Checking.".into()
                },
                ContentBlock::ToolUse {
                    id: "t2".into(),
                    name: "Read".into(),
                    input: json!({"path": "/x"}),
                },
            ]
        );
        assert_eq!(response.stop_reason, StopReason::ToolUse);
        assert_eq!(
            response.usage,
            TokenDelta {
                input_tokens: 11,
                output_tokens: 22
            }
        );
    }

    #[test]
    fn parse_skips_unknown_content_blocks() {
        let text = json!({
            "content": [
                {"type": "thinking", "thinking": "hmm"},
                {"type": "text", "text": "answer"},
            ],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 1, "output_tokens": 2},
        })
        .to_string();
        let response = parse_response(&text).unwrap();
        assert_eq!(
            response.content,
            vec![ContentBlock::Text {
                text: "answer".into()
            }]
        );
    }

    #[test]
    fn parse_rejects_invalid_json() {
        assert!(matches!(
            parse_response("not json"),
            Err(LlmError::Malformed(_))
        ));
    }

    #[test]
    fn stop_reasons_map_to_conversation_variants() {
        assert_eq!(map_stop_reason(Some("end_turn")), StopReason::EndTurn);
        assert_eq!(map_stop_reason(Some("tool_use")), StopReason::ToolUse);
        assert_eq!(map_stop_reason(Some("max_tokens")), StopReason::MaxTokens);
        assert_eq!(
            map_stop_reason(Some("pause_turn")),
            StopReason::Other("pause_turn".into())
        );
        assert_eq!(map_stop_reason(None), StopReason::Other("(none)".into()));
    }

    #[test]
    fn excerpt_truncates_to_the_character_limit() {
        let long = "é".repeat(ERROR_BODY_EXCERPT_CHARS + 100);
        let cut = excerpt(&long);
        assert_eq!(cut.chars().count(), ERROR_BODY_EXCERPT_CHARS);
        assert_eq!(excerpt("  short  "), "short");
    }

    #[test]
    fn debug_output_redacts_the_api_key() {
        let backend = test_backend();
        let debug = format!("{backend:?}");
        assert!(!debug.contains("sk-super-secret"));
        assert!(debug.contains("[redacted]"));
    }

    #[test]
    fn id_includes_the_model() {
        assert_eq!(test_backend().id(), "anthropic:claude-test");
    }

    #[tokio::test]
    async fn create_fails_without_the_api_key() {
        let config = LlmConfig {
            api_key_env: Some("SILO_LLM_UNIT_TEST_KEY_THAT_IS_NEVER_SET".into()),
            ..LlmConfig::default()
        };
        match create(&config).await {
            Err(LlmError::Config(message)) => {
                assert!(message.contains("SILO_LLM_UNIT_TEST_KEY_THAT_IS_NEVER_SET"));
            }
            Err(other) => panic!("expected Config error, got {other:?}"),
            Ok(_) => panic!("expected create to fail"),
        }
    }
}
