//! OpenAI Realtime WebSocket backend, text only.
//!
//! Connects to `{base}/v1/realtime?model=<model>` and drives one
//! text-modality session per `complete` call: wait for `session.created`,
//! send `session.update` (text only, no audio anywhere), replay the
//! conversation as `conversation.item.create` events, send
//! `response.create`, and collect events until `response.done`.
//!
//! One connection is opened per `complete` call and closed afterwards.
//! This keeps the backend stateless, which is acceptable here: the harness
//! resends the full conversation on every call, so no provider-side session
//! state is needed, and a fresh connection per call avoids reconnection and
//! session-expiry handling.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
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
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::common;

const DEFAULT_BASE_URL: &str = "wss://api.openai.com";
const DEFAULT_API_KEY_ENV: &str = "OPENAI_API_KEY";
const RETRY_ATTEMPTS: u32 = 3;
const RETRY_BASE_DELAY: Duration = Duration::from_millis(250);
const CALL_TIMEOUT: Duration = Duration::from_secs(300);

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub async fn create(config: &LlmConfig) -> Result<Arc<dyn LlmBackend>, LlmError> {
    Ok(Arc::new(OpenAiWsBackend::from_config(config)?))
}

struct OpenAiWsBackend {
    model: String,
    base_url: String,
    api_key: SecretString,
    meter: UsageMeter,
}

impl OpenAiWsBackend {
    fn from_config(config: &LlmConfig) -> Result<Self, LlmError> {
        let pricing = config
            .pricing
            .or_else(|| common::default_pricing_for_model(&config.model))
            .unwrap_or_default();
        Ok(OpenAiWsBackend {
            model: config.model.clone(),
            base_url: websocket_base_url(config)?,
            api_key: resolve_api_key(config)?,
            meter: UsageMeter::new(pricing, config.quota),
        })
    }

    async fn connect(&self) -> Result<WsStream, LlmError> {
        let url = format!("{}/v1/realtime?model={}", self.base_url, self.model);
        let mut ws_request = url
            .as_str()
            .into_client_request()
            .map_err(|e| LlmError::Config(format!("invalid realtime url {url:?}: {e}")))?;
        let auth = format!("Bearer {}", self.api_key.expose());
        let auth = HeaderValue::from_str(&auth)
            .map_err(|_| LlmError::Config("api key is not a valid header value".into()))?;
        ws_request.headers_mut().insert(
            tokio_tungstenite::tungstenite::http::header::AUTHORIZATION,
            auth,
        );
        ws_request
            .headers_mut()
            .insert("OpenAI-Beta", HeaderValue::from_static("realtime=v1"));
        let (ws, _response) = tokio_tungstenite::connect_async(ws_request)
            .await
            .map_err(|e| LlmError::Transport(e.to_string()))?;
        Ok(ws)
    }

    /// One full session: connect, configure, replay the conversation,
    /// request one response, and collect it.
    async fn run_session(
        &self,
        request: &CompletionRequest,
    ) -> Result<CompletionResponse, LlmError> {
        let mut ws = self.connect().await?;

        loop {
            let event = next_event(&mut ws).await?;
            match event.get("type").and_then(Value::as_str) {
                Some("session.created") => break,
                Some("error") => return Err(api_error(&event)),
                _ => {}
            }
        }

        send_event(&mut ws, &session_update(&request.system, &request.tools)).await?;
        for item in conversation_items(&request.messages) {
            send_event(
                &mut ws,
                &json!({"type": "conversation.item.create", "item": item}),
            )
            .await?;
        }
        send_event(
            &mut ws,
            &json!({"type": "response.create", "response": {"modalities": ["text"]}}),
        )
        .await?;

        let response = loop {
            let event = next_event(&mut ws).await?;
            match event.get("type").and_then(Value::as_str) {
                Some("response.done") => break parse_response_done(&event)?,
                Some("error") => return Err(api_error(&event)),
                _ => {}
            }
        };
        let _ = ws.close(None).await;
        Ok(response)
    }
}

#[async_trait]
impl LlmBackend for OpenAiWsBackend {
    fn id(&self) -> String {
        format!("openai-ws:{}", self.model)
    }

    async fn complete(&self, request: &CompletionRequest) -> Result<CompletionResponse, LlmError> {
        self.meter.check_quota()?;
        let attempt_all = common::retry_with_backoff(RETRY_ATTEMPTS, RETRY_BASE_DELAY, || {
            self.run_session(request)
        });
        let response = match tokio::time::timeout(CALL_TIMEOUT, attempt_all).await {
            Ok(result) => result?,
            Err(_) => return Err(LlmError::Transport("timed out".into())),
        };
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

/// Normalizes the configured base URL to a ws:// or wss:// origin.
/// http:// and https:// are accepted and mapped to the WebSocket scheme.
fn websocket_base_url(config: &LlmConfig) -> Result<String, LlmError> {
    let raw = config
        .base_url
        .clone()
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
    let trimmed = raw.trim_end_matches('/');
    if trimmed.starts_with("ws://") || trimmed.starts_with("wss://") {
        Ok(trimmed.to_string())
    } else if let Some(rest) = trimmed.strip_prefix("https://") {
        Ok(format!("wss://{rest}"))
    } else if let Some(rest) = trimmed.strip_prefix("http://") {
        Ok(format!("ws://{rest}"))
    } else {
        Err(LlmError::Config(format!(
            "base_url {trimmed:?} must use the ws:// or wss:// scheme"
        )))
    }
}

async fn send_event(ws: &mut WsStream, event: &Value) -> Result<(), LlmError> {
    let text = serde_json::to_string(event)
        .map_err(|e| LlmError::Malformed(format!("failed to serialize event: {e}")))?;
    ws.send(WsMessage::Text(text.into()))
        .await
        .map_err(|e| LlmError::Transport(e.to_string()))
}

/// Reads the next JSON event, skipping non-text frames. A closed stream is
/// a transport error: the caller only reads while awaiting more events.
async fn next_event(ws: &mut WsStream) -> Result<Value, LlmError> {
    while let Some(frame) = ws.next().await {
        let frame = frame.map_err(|e| LlmError::Transport(e.to_string()))?;
        match frame {
            WsMessage::Text(text) => {
                return serde_json::from_str(text.as_str()).map_err(|e| {
                    LlmError::Malformed(format!("server event is not valid JSON: {e}"))
                });
            }
            WsMessage::Close(_) => {
                return Err(LlmError::Transport(
                    "connection closed before response.done".into(),
                ));
            }
            WsMessage::Binary(_)
            | WsMessage::Ping(_)
            | WsMessage::Pong(_)
            | WsMessage::Frame(_) => {}
        }
    }
    Err(LlmError::Transport(
        "connection closed before response.done".into(),
    ))
}

fn api_error(event: &Value) -> LlmError {
    let message = event
        .pointer("/error/message")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| event.to_string());
    LlmError::Api(format!("realtime error: {message}"))
}

fn session_update(system: &str, tools: &[ToolDef]) -> Value {
    json!({
        "type": "session.update",
        "session": {
            "modalities": ["text"],
            "instructions": system,
            "tools": tools.iter().map(|tool| json!({
                "type": "function",
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.input_schema,
            })).collect::<Vec<_>>(),
            "tool_choice": "auto",
            "turn_detection": null,
        },
    })
}

/// Maps the conversation to Realtime conversation items, one item per
/// content block.
fn conversation_items(messages: &[Message]) -> Vec<Value> {
    let mut items = Vec::new();
    for message in messages {
        let (role, text_type) = match message.role {
            Role::User => ("user", "input_text"),
            Role::Assistant => ("assistant", "text"),
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

fn parse_response_done(event: &Value) -> Result<CompletionResponse, LlmError> {
    let response = event
        .get("response")
        .ok_or_else(|| LlmError::Malformed("response.done event has no response object".into()))?;
    let output = response
        .get("output")
        .and_then(Value::as_array)
        .ok_or_else(|| LlmError::Malformed("response.done has no output array".into()))?;
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
                    let part_type = part.get("type").and_then(Value::as_str);
                    if part_type == Some("text") || part_type == Some("output_text") {
                        let text = part.get("text").and_then(Value::as_str).ok_or_else(|| {
                            LlmError::Malformed("text part has no text string".into())
                        })?;
                        content.push(ContentBlock::Text {
                            text: text.to_string(),
                        });
                    }
                }
            }
            Some("function_call") => {
                saw_function_call = true;
                let id = item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| LlmError::Malformed("function_call has no call_id".into()))?;
                let name = item
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| LlmError::Malformed("function_call has no name".into()))?;
                let arguments = item
                    .get("arguments")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        LlmError::Malformed("function_call has no arguments string".into())
                    })?;
                let input: Value = serde_json::from_str(arguments).map_err(|e| {
                    LlmError::Malformed(format!("function_call arguments are not valid JSON: {e}"))
                })?;
                content.push(ContentBlock::ToolUse {
                    id: id.to_string(),
                    name: name.to_string(),
                    input,
                });
            }
            _ => {}
        }
    }
    let usage = TokenDelta {
        input_tokens: response
            .pointer("/usage/input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        output_tokens: response
            .pointer("/usage/output_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    };
    let stop_reason = if saw_function_call {
        StopReason::ToolUse
    } else if response.get("status").and_then(Value::as_str) == Some("incomplete")
        && response
            .pointer("/status_details/reason")
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

#[cfg(test)]
mod tests {
    use super::*;
    use silo_core::tool::ToolAvailability;

    #[test]
    fn base_url_accepts_ws_schemes_and_maps_http() {
        let with = |base: Option<&str>| LlmConfig {
            base_url: base.map(str::to_string),
            ..LlmConfig::default()
        };
        assert_eq!(
            websocket_base_url(&with(None)).unwrap(),
            "wss://api.openai.com"
        );
        assert_eq!(
            websocket_base_url(&with(Some("ws://127.0.0.1:9000/"))).unwrap(),
            "ws://127.0.0.1:9000"
        );
        assert_eq!(
            websocket_base_url(&with(Some("wss://example.com"))).unwrap(),
            "wss://example.com"
        );
        assert_eq!(
            websocket_base_url(&with(Some("https://example.com"))).unwrap(),
            "wss://example.com"
        );
        assert_eq!(
            websocket_base_url(&with(Some("http://example.com"))).unwrap(),
            "ws://example.com"
        );
        assert!(matches!(
            websocket_base_url(&with(Some("ftp://example.com"))),
            Err(LlmError::Config(_))
        ));
    }

    #[test]
    fn session_update_is_text_only_with_tools() {
        let tools = vec![ToolDef {
            name: "Bash".into(),
            description: "run a command".into(),
            input_schema: json!({"type": "object"}),
            availability: ToolAvailability::Both,
        }];
        let update = session_update("system prompt", &tools);
        assert_eq!(update["type"], "session.update");
        let session = &update["session"];
        assert_eq!(session["modalities"], json!(["text"]));
        assert_eq!(session["instructions"], "system prompt");
        assert_eq!(session["tool_choice"], "auto");
        assert_eq!(session["turn_detection"], Value::Null);
        assert_eq!(session["tools"][0]["type"], "function");
        assert_eq!(session["tools"][0]["name"], "Bash");
        assert_eq!(session["tools"][0]["parameters"], json!({"type": "object"}));
        assert!(!update.to_string().contains("audio"));
    }

    #[test]
    fn conversation_items_map_all_block_kinds() {
        let messages = vec![
            Message::user_text("hello"),
            Message::assistant(vec![
                ContentBlock::Text {
                    text: "calling".into(),
                },
                ContentBlock::ToolUse {
                    id: "call_7".into(),
                    name: "Read".into(),
                    input: json!({"path": "x"}),
                },
            ]),
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "call_7".into(),
                    content: "contents".into(),
                    is_error: false,
                }],
            },
        ];
        let items = conversation_items(&messages);
        assert_eq!(items.len(), 4);
        assert_eq!(items[0]["type"], "message");
        assert_eq!(items[0]["role"], "user");
        assert_eq!(items[0]["content"][0]["type"], "input_text");
        assert_eq!(items[1]["role"], "assistant");
        assert_eq!(items[1]["content"][0]["type"], "text");
        assert_eq!(items[2]["type"], "function_call");
        assert_eq!(items[2]["call_id"], "call_7");
        let arguments: Value =
            serde_json::from_str(items[2]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(arguments, json!({"path": "x"}));
        assert_eq!(items[3]["type"], "function_call_output");
        assert_eq!(items[3]["output"], "contents");
    }

    #[test]
    fn response_done_maps_text_function_calls_and_usage() {
        let event = json!({
            "type": "response.done",
            "response": {
                "status": "completed",
                "output": [
                    {"type": "message", "role": "assistant", "content": [
                        {"type": "text", "text": "thinking"},
                    ]},
                    {"type": "function_call", "call_id": "c1", "name": "Bash",
                     "arguments": "{\"command\":\"ls\"}"},
                ],
                "usage": {"input_tokens": 21, "output_tokens": 9},
            },
        });
        let response = parse_response_done(&event).unwrap();
        assert_eq!(response.stop_reason, StopReason::ToolUse);
        assert_eq!(
            response.usage,
            TokenDelta {
                input_tokens: 21,
                output_tokens: 9
            }
        );
        assert_eq!(response.content.len(), 2);
        assert_eq!(
            response.content[1],
            ContentBlock::ToolUse {
                id: "c1".into(),
                name: "Bash".into(),
                input: json!({"command": "ls"}),
            }
        );
    }

    #[test]
    fn response_done_stop_reasons() {
        let text_only = json!({
            "response": {
                "status": "completed",
                "output": [{"type": "message", "content": [
                    {"type": "text", "text": "done"},
                ]}],
            },
        });
        assert_eq!(
            parse_response_done(&text_only).unwrap().stop_reason,
            StopReason::EndTurn
        );

        let truncated = json!({
            "response": {
                "status": "incomplete",
                "status_details": {"reason": "max_output_tokens"},
                "output": [],
            },
        });
        assert_eq!(
            parse_response_done(&truncated).unwrap().stop_reason,
            StopReason::MaxTokens
        );
    }

    #[test]
    fn response_done_rejects_malformed_events() {
        assert!(matches!(
            parse_response_done(&json!({"type": "response.done"})),
            Err(LlmError::Malformed(_))
        ));
        let bad_arguments = json!({
            "response": {"output": [
                {"type": "function_call", "call_id": "c", "name": "n", "arguments": "{"},
            ]},
        });
        assert!(matches!(
            parse_response_done(&bad_arguments),
            Err(LlmError::Malformed(_))
        ));
    }

    #[test]
    fn api_error_prefers_error_message() {
        let event = json!({"type": "error", "error": {"message": "no such model"}});
        assert_eq!(
            api_error(&event).to_string(),
            "llm api error: realtime error: no such model"
        );
    }

    #[test]
    fn create_requires_api_key() {
        let config = LlmConfig {
            api_key_env: Some("SILO_TEST_WS_UNSET_KEY".into()),
            ..LlmConfig::default()
        };
        let error = futures::executor::block_on(create(&config)).err().unwrap();
        assert!(matches!(error, LlmError::Config(_)));
    }

    #[test]
    fn backend_id_uses_ws_prefix() {
        std::env::set_var("SILO_TEST_WS_ID_KEY", "sk-test");
        let config = LlmConfig {
            model: "gpt-4o-realtime-preview".into(),
            api_key_env: Some("SILO_TEST_WS_ID_KEY".into()),
            ..LlmConfig::default()
        };
        let backend = OpenAiWsBackend::from_config(&config).unwrap();
        assert_eq!(backend.id(), "openai-ws:gpt-4o-realtime-preview");
    }
}
