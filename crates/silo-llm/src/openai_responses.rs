//! OpenAI Responses REST backend.
//!
//! Sends each completion as one `POST {base}/v1/responses` call with
//! `store: false`, so the provider keeps no conversation state between
//! calls. Conversation history is replayed in full as ordered `input`
//! items. Usage is metered through `silo_core::cost::UsageMeter`, with the
//! quota checked before every request, and transient failures (transport
//! errors, HTTP 429/5xx) are retried with exponential backoff.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use silo_core::config::LlmConfig;
use silo_core::conversation::{
    CompletionRequest, CompletionResponse, ContentBlock, Message, Role, StopReason, TokenDelta,
};
use silo_core::cost::{QuotaConfig, UsageMeter, UsageSnapshot};
use silo_core::error::LlmError;
use silo_core::secrets::SecretString;
use silo_core::tool::ToolDef;
use silo_core::traits::LlmBackend;

use crate::common;

const DEFAULT_BASE_URL: &str = "https://api.openai.com";
const DEFAULT_API_KEY_ENV: &str = "OPENAI_API_KEY";
const RETRY_ATTEMPTS: u32 = 3;
const RETRY_BASE_DELAY: Duration = Duration::from_millis(250);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(600);
const ERROR_BODY_SNIPPET_BYTES: usize = 600;

pub async fn create(config: &LlmConfig) -> Result<Arc<dyn LlmBackend>, LlmError> {
    Ok(Arc::new(OpenAiResponsesBackend::from_config(config)?))
}

struct OpenAiResponsesBackend {
    model: String,
    base_url: String,
    api_key: SecretString,
    client: reqwest::Client,
    meter: UsageMeter,
}

impl OpenAiResponsesBackend {
    fn from_config(config: &LlmConfig) -> Result<Self, LlmError> {
        let base_url = config
            .base_url
            .clone()
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
            .trim_end_matches('/')
            .to_string();
        let pricing = config
            .pricing
            .or_else(|| common::default_pricing_for_model(&config.model))
            .unwrap_or_default();
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|e| LlmError::Config(format!("failed to build http client: {e}")))?;
        Ok(OpenAiResponsesBackend {
            model: config.model.clone(),
            base_url,
            api_key: resolve_api_key(config)?,
            client,
            meter: UsageMeter::new(pricing, config.quota),
        })
    }

    async fn send_once(&self, url: &str, body: &Value) -> Result<Value, LlmError> {
        let http_response = self
            .client
            .post(url)
            .header(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {}", self.api_key.expose()),
            )
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
                snippet(&text)
            )));
        }
        serde_json::from_str(&text)
            .map_err(|e| LlmError::Malformed(format!("response body is not valid JSON: {e}")))
    }
}

#[async_trait]
impl LlmBackend for OpenAiResponsesBackend {
    fn id(&self) -> String {
        format!("openai-responses:{}", self.model)
    }

    async fn complete(&self, request: &CompletionRequest) -> Result<CompletionResponse, LlmError> {
        self.meter.check_quota()?;
        let url = format!("{}/v1/responses", self.base_url);
        let body = build_body(&self.model, request);
        let value = common::retry_with_backoff(RETRY_ATTEMPTS, RETRY_BASE_DELAY, || {
            self.send_once(&url, &body)
        })
        .await?;
        let response = parse_response(&value)?;
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

fn resolve_api_key(config: &LlmConfig) -> Result<SecretString, LlmError> {
    let env_name = config
        .api_key_env
        .clone()
        .unwrap_or_else(|| DEFAULT_API_KEY_ENV.to_string());
    match std::env::var(&env_name) {
        Ok(value) if !value.is_empty() => Ok(SecretString::new(value)),
        _ => Err(LlmError::Config(format!(
            "environment variable {env_name} is not set"
        ))),
    }
}

/// First bytes of an error body, trimmed onto a character boundary.
fn snippet(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.len() <= ERROR_BODY_SNIPPET_BYTES {
        return trimmed.to_string();
    }
    let mut end = ERROR_BODY_SNIPPET_BYTES;
    while !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    format!("{} [truncated]", &trimmed[..end])
}

fn build_body(model: &str, request: &CompletionRequest) -> Value {
    json!({
        "model": model,
        "instructions": request.system,
        "store": false,
        "max_output_tokens": request.max_tokens,
        "tools": request.tools.iter().map(tool_to_wire).collect::<Vec<_>>(),
        "input": build_input(&request.messages),
    })
}

fn tool_to_wire(tool: &ToolDef) -> Value {
    json!({
        "type": "function",
        "name": tool.name,
        "description": tool.description,
        "parameters": tool.input_schema,
        "strict": false,
    })
}

/// Maps the conversation to ordered Responses input items, one item per
/// content block.
fn build_input(messages: &[Message]) -> Vec<Value> {
    let mut items = Vec::new();
    for message in messages {
        let (role, text_type) = match message.role {
            Role::User => ("user", "input_text"),
            Role::Assistant => ("assistant", "output_text"),
        };
        for block in &message.content {
            match block {
                ContentBlock::Text { text } => items.push(json!({
                    "type": "message",
                    "role": role,
                    "content": [{"type": text_type, "text": text}],
                })),
                ContentBlock::ToolUse { id, name, input } => items.push(json!({
                    "type": "function_call",
                    "call_id": id,
                    "name": name,
                    "arguments": input.to_string(),
                })),
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } => items.push(json!({
                    "type": "function_call_output",
                    "call_id": tool_use_id,
                    "output": content,
                })),
            }
        }
    }
    items
}

fn parse_response(value: &Value) -> Result<CompletionResponse, LlmError> {
    let output = value
        .get("output")
        .and_then(Value::as_array)
        .ok_or_else(|| LlmError::Malformed("response has no output array".into()))?;
    let mut content = Vec::new();
    let mut saw_function_call = false;
    for item in output {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                for part in item
                    .get("content")
                    .and_then(Value::as_array)
                    .map(|a| a.as_slice())
                    .unwrap_or_default()
                {
                    if part.get("type").and_then(Value::as_str) == Some("output_text") {
                        let text = part.get("text").and_then(Value::as_str).ok_or_else(|| {
                            LlmError::Malformed("output_text part has no text string".into())
                        })?;
                        content.push(ContentBlock::Text {
                            text: text.to_string(),
                        });
                    }
                }
            }
            Some("function_call") => {
                saw_function_call = true;
                content.push(parse_function_call(item)?);
            }
            _ => {}
        }
    }
    let usage = TokenDelta {
        input_tokens: value
            .pointer("/usage/input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        output_tokens: value
            .pointer("/usage/output_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    };
    let stop_reason = if saw_function_call {
        StopReason::ToolUse
    } else if value.get("status").and_then(Value::as_str) == Some("incomplete")
        && value
            .pointer("/incomplete_details/reason")
            .and_then(Value::as_str)
            == Some("max_output_tokens")
    {
        StopReason::MaxTokens
    } else {
        StopReason::EndTurn
    };
    Ok(CompletionResponse {
        content,
        stop_reason,
        usage,
    })
}

fn parse_function_call(item: &Value) -> Result<ContentBlock, LlmError> {
    let id = item
        .get("call_id")
        .and_then(Value::as_str)
        .ok_or_else(|| LlmError::Malformed("function_call item has no call_id".into()))?;
    let name = item
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| LlmError::Malformed("function_call item has no name".into()))?;
    let arguments = item
        .get("arguments")
        .and_then(Value::as_str)
        .ok_or_else(|| LlmError::Malformed("function_call item has no arguments string".into()))?;
    let input: Value = serde_json::from_str(arguments).map_err(|e| {
        LlmError::Malformed(format!("function_call arguments are not valid JSON: {e}"))
    })?;
    Ok(ContentBlock::ToolUse {
        id: id.to_string(),
        name: name.to_string(),
        input,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use silo_core::tool::ToolAvailability;

    fn request_with_history() -> CompletionRequest {
        CompletionRequest {
            system: "be terse".into(),
            messages: vec![
                Message::user_text("list files"),
                Message::assistant(vec![
                    ContentBlock::Text {
                        text: "running ls".into(),
                    },
                    ContentBlock::ToolUse {
                        id: "call_1".into(),
                        name: "Bash".into(),
                        input: json!({"command": "ls"}),
                    },
                ]),
                Message {
                    role: Role::User,
                    content: vec![ContentBlock::ToolResult {
                        tool_use_id: "call_1".into(),
                        content: "file.txt".into(),
                        is_error: false,
                    }],
                },
            ],
            tools: vec![ToolDef {
                name: "Bash".into(),
                description: "run a command".into(),
                input_schema: json!({"type": "object"}),
                availability: ToolAvailability::Both,
            }],
            max_tokens: 512,
        }
    }

    #[test]
    fn body_carries_model_instructions_store_and_tools() {
        let body = build_body("gpt-4.1", &request_with_history());
        assert_eq!(body["model"], "gpt-4.1");
        assert_eq!(body["instructions"], "be terse");
        assert_eq!(body["store"], false);
        assert_eq!(body["max_output_tokens"], 512);
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["name"], "Bash");
        assert_eq!(body["tools"][0]["strict"], false);
        assert_eq!(body["tools"][0]["parameters"], json!({"type": "object"}));
    }

    #[test]
    fn input_items_map_blocks_in_order() {
        let body = build_body("gpt-4.1", &request_with_history());
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 4);

        assert_eq!(input[0]["type"], "message");
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["content"][0]["type"], "input_text");
        assert_eq!(input[0]["content"][0]["text"], "list files");

        assert_eq!(input[1]["type"], "message");
        assert_eq!(input[1]["role"], "assistant");
        assert_eq!(input[1]["content"][0]["type"], "output_text");

        assert_eq!(input[2]["type"], "function_call");
        assert_eq!(input[2]["call_id"], "call_1");
        assert_eq!(input[2]["name"], "Bash");
        let arguments: Value =
            serde_json::from_str(input[2]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(arguments, json!({"command": "ls"}));

        assert_eq!(input[3]["type"], "function_call_output");
        assert_eq!(input[3]["call_id"], "call_1");
        assert_eq!(input[3]["output"], "file.txt");
    }

    #[test]
    fn parses_text_and_function_calls() {
        let value = json!({
            "status": "completed",
            "output": [
                {"type": "reasoning", "summary": []},
                {"type": "message", "role": "assistant", "content": [
                    {"type": "output_text", "text": "checking"},
                ]},
                {"type": "function_call", "call_id": "c9", "name": "Read",
                 "arguments": "{\"path\":\"a.rs\"}"},
            ],
            "usage": {"input_tokens": 11, "output_tokens": 7},
        });
        let response = parse_response(&value).unwrap();
        assert_eq!(response.stop_reason, StopReason::ToolUse);
        assert_eq!(
            response.usage,
            TokenDelta {
                input_tokens: 11,
                output_tokens: 7
            }
        );
        assert_eq!(response.content.len(), 2);
        assert_eq!(
            response.content[0],
            ContentBlock::Text {
                text: "checking".into()
            }
        );
        assert_eq!(
            response.content[1],
            ContentBlock::ToolUse {
                id: "c9".into(),
                name: "Read".into(),
                input: json!({"path": "a.rs"}),
            }
        );
    }

    #[test]
    fn stop_reason_end_turn_and_max_tokens() {
        let completed = json!({
            "status": "completed",
            "output": [{"type": "message", "content": [
                {"type": "output_text", "text": "done"},
            ]}],
        });
        assert_eq!(
            parse_response(&completed).unwrap().stop_reason,
            StopReason::EndTurn
        );

        let truncated = json!({
            "status": "incomplete",
            "incomplete_details": {"reason": "max_output_tokens"},
            "output": [{"type": "message", "content": [
                {"type": "output_text", "text": "partial"},
            ]}],
        });
        assert_eq!(
            parse_response(&truncated).unwrap().stop_reason,
            StopReason::MaxTokens
        );

        let other_incomplete = json!({
            "status": "incomplete",
            "incomplete_details": {"reason": "content_filter"},
            "output": [],
        });
        assert_eq!(
            parse_response(&other_incomplete).unwrap().stop_reason,
            StopReason::EndTurn
        );
    }

    #[test]
    fn malformed_output_is_rejected() {
        assert!(matches!(
            parse_response(&json!({"usage": {}})),
            Err(LlmError::Malformed(_))
        ));
        let bad_arguments = json!({
            "output": [{"type": "function_call", "call_id": "c", "name": "Read",
                        "arguments": "not json"}],
        });
        assert!(matches!(
            parse_response(&bad_arguments),
            Err(LlmError::Malformed(_))
        ));
        let missing_call_id = json!({
            "output": [{"type": "function_call", "name": "Read", "arguments": "{}"}],
        });
        assert!(matches!(
            parse_response(&missing_call_id),
            Err(LlmError::Malformed(_))
        ));
    }

    #[test]
    fn missing_usage_defaults_to_zero() {
        let value = json!({"output": []});
        let response = parse_response(&value).unwrap();
        assert_eq!(response.usage, TokenDelta::default());
    }

    #[test]
    fn create_requires_api_key() {
        let config = LlmConfig {
            api_key_env: Some("SILO_TEST_RESPONSES_UNSET_KEY".into()),
            ..LlmConfig::default()
        };
        let error = futures::executor::block_on(create(&config)).err().unwrap();
        assert!(matches!(error, LlmError::Config(_)));
        assert!(error.to_string().contains("SILO_TEST_RESPONSES_UNSET_KEY"));
    }

    #[test]
    fn backend_id_and_default_pricing() {
        std::env::set_var("SILO_TEST_RESPONSES_ID_KEY", "sk-test");
        let config = LlmConfig {
            model: "gpt-4.1".into(),
            api_key_env: Some("SILO_TEST_RESPONSES_ID_KEY".into()),
            ..LlmConfig::default()
        };
        let backend = OpenAiResponsesBackend::from_config(&config).unwrap();
        assert_eq!(backend.id(), "openai-responses:gpt-4.1");
        backend.meter.record(TokenDelta {
            input_tokens: 1_000_000,
            output_tokens: 0,
        });
        assert!((backend.usage().usd - 2.0).abs() < 1e-9);
    }

    #[test]
    fn snippet_truncates_on_char_boundary() {
        let long = "é".repeat(2000);
        let cut = snippet(&long);
        assert!(cut.ends_with("[truncated]"));
        assert!(cut.len() < long.len());
        assert_eq!(snippet("short"), "short");
    }
}
