//! Local model backend.
//!
//! Talks to an OpenAI-compatible chat-completions server on localhost
//! (`POST {base}/v1/chat/completions`). When `local_server_command` is
//! configured, the command is spawned through `sh -c` and the backend waits
//! for the server to become ready by polling `GET {base}/v1/models` (with
//! `GET {base}/health` as a fallback) every 250ms for up to 60 seconds.
//! The spawned server is killed when the backend is dropped.
//!
//! No Authorization header is sent. Pricing is zero unless explicitly
//! configured, so the meter reports token counts but no dollar cost.

use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Map, Value};
use silo_core::config::LlmConfig;
use silo_core::conversation::{
    CompletionRequest, CompletionResponse, ContentBlock, Message, Role, StopReason, TokenDelta,
};
use silo_core::cost::{QuotaConfig, UsageMeter, UsageSnapshot};
use silo_core::error::LlmError;
use silo_core::tool::ToolDef;
use silo_core::traits::LlmBackend;
use tokio::io::AsyncReadExt;
use tokio::process::Child;

use crate::common;

const DEFAULT_BASE_URL: &str = "http://127.0.0.1:8080";
const RETRY_ATTEMPTS: u32 = 3;
const RETRY_BASE_DELAY: Duration = Duration::from_millis(250);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(600);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(60);
const STARTUP_POLL_INTERVAL: Duration = Duration::from_millis(250);
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const STDERR_TAIL_BYTES: usize = 4096;
const ERROR_BODY_SNIPPET_BYTES: usize = 600;

pub async fn create(config: &LlmConfig) -> Result<Arc<dyn LlmBackend>, LlmError> {
    let base_url = config
        .base_url
        .clone()
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
        .trim_end_matches('/')
        .to_string();
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|e| LlmError::Config(format!("failed to build http client: {e}")))?;
    let child = match &config.local_server_command {
        Some(command) => Some(start_server(command, &base_url, &client).await?),
        None => None,
    };
    Ok(Arc::new(LocalBackend {
        model: config.model.clone(),
        base_url,
        client,
        meter: UsageMeter::new(config.pricing.unwrap_or_default(), config.quota),
        child: Mutex::new(child),
    }))
}

struct LocalBackend {
    model: String,
    base_url: String,
    client: reqwest::Client,
    meter: UsageMeter,
    /// Managed inference server process, when the backend spawned one.
    /// Killed on drop.
    child: Mutex<Option<Child>>,
}

impl Drop for LocalBackend {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.child.lock() {
            if let Some(child) = guard.as_mut() {
                let _ = child.start_kill();
            }
        }
    }
}

impl LocalBackend {
    async fn send_once(&self, url: &str, body: &Value) -> Result<Value, LlmError> {
        let http_response = self
            .client
            .post(url)
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
impl LlmBackend for LocalBackend {
    fn id(&self) -> String {
        format!("local:{}", self.model)
    }

    async fn complete(&self, request: &CompletionRequest) -> Result<CompletionResponse, LlmError> {
        self.meter.check_quota()?;
        let url = format!("{}/v1/chat/completions", self.base_url);
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

/// Spawns the server command and waits until the server answers a health
/// probe. The child is configured to be killed when its handle is dropped.
async fn start_server(
    command: &str,
    base_url: &str,
    client: &reqwest::Client,
) -> Result<Child, LlmError> {
    let mut child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| LlmError::Config(format!("failed to spawn local server command: {e}")))?;

    let stderr_tail: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let reader = child.stderr.take().map(|mut stderr| {
        let tail = stderr_tail.clone();
        tokio::spawn(async move {
            let mut chunk = [0u8; 1024];
            loop {
                match stderr.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut tail = tail.lock().expect("stderr tail poisoned");
                        tail.extend_from_slice(&chunk[..n]);
                        let len = tail.len();
                        if len > STDERR_TAIL_BYTES {
                            tail.drain(..len - STDERR_TAIL_BYTES);
                        }
                    }
                }
            }
        })
    });

    let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
    let failure = loop {
        if probe(client, base_url).await {
            return Ok(child);
        }
        // A nonzero early exit means the command failed; a zero exit may be
        // a daemonizing launcher, so polling continues.
        if let Ok(Some(status)) = child.try_wait() {
            if !status.success() {
                break format!("local server command exited with {status} before becoming ready");
            }
        }
        if tokio::time::Instant::now() >= deadline {
            let _ = child.start_kill();
            break format!(
                "local server at {base_url} did not become ready within {}s",
                STARTUP_TIMEOUT.as_secs()
            );
        }
        tokio::time::sleep(STARTUP_POLL_INTERVAL).await;
    };

    if let Some(handle) = reader {
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
    }
    let tail = stderr_tail.lock().expect("stderr tail poisoned");
    let tail = String::from_utf8_lossy(&tail).trim().to_string();
    Err(LlmError::Config(format!("{failure}; stderr: {tail}")))
}

/// True when the server answers `/v1/models` or `/health` with a 2xx.
async fn probe(client: &reqwest::Client, base_url: &str) -> bool {
    for path in ["/v1/models", "/health"] {
        let url = format!("{base_url}{path}");
        if let Ok(response) = client.get(&url).timeout(PROBE_TIMEOUT).send().await {
            if response.status().is_success() {
                return true;
            }
        }
    }
    false
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
    let mut messages = Vec::new();
    if !request.system.is_empty() {
        messages.push(json!({"role": "system", "content": request.system}));
    }
    for message in &request.messages {
        append_message(&mut messages, message);
    }
    let mut body = Map::new();
    body.insert("model".into(), json!(model));
    body.insert("max_tokens".into(), json!(request.max_tokens));
    body.insert("messages".into(), Value::Array(messages));
    if !request.tools.is_empty() {
        body.insert(
            "tools".into(),
            Value::Array(request.tools.iter().map(tool_to_wire).collect()),
        );
        body.insert("tool_choice".into(), json!("auto"));
    }
    Value::Object(body)
}

fn tool_to_wire(tool: &ToolDef) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": tool.name,
            "description": tool.description,
            "parameters": tool.input_schema,
        },
    })
}

/// Maps one conversation message to chat-completions messages. Tool results
/// become `role:"tool"` messages; text blocks are joined into a single
/// user or assistant message; assistant tool-use blocks become `tool_calls`
/// on the assistant message.
fn append_message(messages: &mut Vec<Value>, message: &Message) {
    match message.role {
        Role::User => {
            let mut texts = Vec::new();
            for block in &message.content {
                match block {
                    ContentBlock::Text { text } => texts.push(text.as_str()),
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    } => messages.push(json!({
                        "role": "tool",
                        "tool_call_id": tool_use_id,
                        "content": content,
                    })),
                    ContentBlock::ToolUse { .. } => {}
                }
            }
            if !texts.is_empty() {
                messages.push(json!({"role": "user", "content": texts.join("\n")}));
            }
        }
        Role::Assistant => {
            let mut texts = Vec::new();
            let mut tool_calls = Vec::new();
            for block in &message.content {
                match block {
                    ContentBlock::Text { text } => texts.push(text.as_str()),
                    ContentBlock::ToolUse { id, name, input } => tool_calls.push(json!({
                        "id": id,
                        "type": "function",
                        "function": {"name": name, "arguments": input.to_string()},
                    })),
                    ContentBlock::ToolResult { .. } => {}
                }
            }
            let mut assistant = Map::new();
            assistant.insert("role".into(), json!("assistant"));
            if texts.is_empty() {
                assistant.insert("content".into(), Value::Null);
            } else {
                assistant.insert("content".into(), json!(texts.join("\n")));
            }
            if !tool_calls.is_empty() {
                assistant.insert("tool_calls".into(), Value::Array(tool_calls));
            }
            messages.push(Value::Object(assistant));
        }
    }
}

fn parse_response(value: &Value) -> Result<CompletionResponse, LlmError> {
    let choice = value
        .pointer("/choices/0")
        .ok_or_else(|| LlmError::Malformed("response has no choices".into()))?;
    let message = choice
        .get("message")
        .ok_or_else(|| LlmError::Malformed("choice has no message".into()))?;
    let mut content = Vec::new();
    if let Some(text) = message.get("content").and_then(Value::as_str) {
        if !text.is_empty() {
            content.push(ContentBlock::Text {
                text: text.to_string(),
            });
        }
    }
    for call in message
        .get("tool_calls")
        .and_then(Value::as_array)
        .map(|a| a.as_slice())
        .unwrap_or_default()
    {
        let id = call
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| LlmError::Malformed("tool_call has no id".into()))?;
        let name = call
            .pointer("/function/name")
            .and_then(Value::as_str)
            .ok_or_else(|| LlmError::Malformed("tool_call has no function name".into()))?;
        let arguments = call
            .pointer("/function/arguments")
            .and_then(Value::as_str)
            .ok_or_else(|| LlmError::Malformed("tool_call has no arguments string".into()))?;
        let input: Value = serde_json::from_str(arguments).map_err(|e| {
            LlmError::Malformed(format!("tool_call arguments are not valid JSON: {e}"))
        })?;
        content.push(ContentBlock::ToolUse {
            id: id.to_string(),
            name: name.to_string(),
            input,
        });
    }
    let stop_reason = match choice.get("finish_reason").and_then(Value::as_str) {
        Some("tool_calls") => StopReason::ToolUse,
        Some("length") => StopReason::MaxTokens,
        _ => StopReason::EndTurn,
    };
    let usage = TokenDelta {
        input_tokens: value
            .pointer("/usage/prompt_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        output_tokens: value
            .pointer("/usage/completion_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
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

    fn request_with_history() -> CompletionRequest {
        CompletionRequest {
            system: "stay local".into(),
            messages: vec![
                Message::user_text("compile it"),
                Message::assistant(vec![
                    ContentBlock::Text {
                        text: "building".into(),
                    },
                    ContentBlock::ToolUse {
                        id: "call_3".into(),
                        name: "Bash".into(),
                        input: json!({"command": "make"}),
                    },
                ]),
                Message {
                    role: Role::User,
                    content: vec![ContentBlock::ToolResult {
                        tool_use_id: "call_3".into(),
                        content: "ok".into(),
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
            max_tokens: 256,
        }
    }

    #[test]
    fn body_maps_system_tools_and_messages() {
        let body = build_body("llama-3", &request_with_history());
        assert_eq!(body["model"], "llama-3");
        assert_eq!(body["max_tokens"], 256);
        assert_eq!(body["tool_choice"], "auto");
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["name"], "Bash");
        assert_eq!(
            body["tools"][0]["function"]["parameters"],
            json!({"type": "object"})
        );

        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 4);
        assert_eq!(
            messages[0],
            json!({"role": "system", "content": "stay local"})
        );
        assert_eq!(
            messages[1],
            json!({"role": "user", "content": "compile it"})
        );
        assert_eq!(messages[2]["role"], "assistant");
        assert_eq!(messages[2]["content"], "building");
        assert_eq!(messages[2]["tool_calls"][0]["id"], "call_3");
        assert_eq!(messages[2]["tool_calls"][0]["type"], "function");
        assert_eq!(messages[2]["tool_calls"][0]["function"]["name"], "Bash");
        let arguments: Value = serde_json::from_str(
            messages[2]["tool_calls"][0]["function"]["arguments"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(arguments, json!({"command": "make"}));
        assert_eq!(
            messages[3],
            json!({"role": "tool", "tool_call_id": "call_3", "content": "ok"})
        );
    }

    #[test]
    fn body_omits_empty_system_and_tools() {
        let request = CompletionRequest {
            system: String::new(),
            messages: vec![Message::user_text("hi")],
            tools: vec![],
            max_tokens: 10,
        };
        let body = build_body("m", &request);
        assert!(body.get("tools").is_none());
        assert!(body.get("tool_choice").is_none());
        assert_eq!(body["messages"][0]["role"], "user");
    }

    #[test]
    fn assistant_message_without_text_has_null_content() {
        let mut messages = Vec::new();
        append_message(
            &mut messages,
            &Message::assistant(vec![ContentBlock::ToolUse {
                id: "c".into(),
                name: "Read".into(),
                input: json!({}),
            }]),
        );
        assert_eq!(messages[0]["content"], Value::Null);
        assert_eq!(messages[0]["tool_calls"][0]["function"]["name"], "Read");
    }

    #[test]
    fn parses_text_response() {
        let value = json!({
            "choices": [{
                "message": {"role": "assistant", "content": "hello"},
                "finish_reason": "stop",
            }],
            "usage": {"prompt_tokens": 4, "completion_tokens": 2},
        });
        let response = parse_response(&value).unwrap();
        assert_eq!(response.text(), "hello");
        assert_eq!(response.stop_reason, StopReason::EndTurn);
        assert_eq!(
            response.usage,
            TokenDelta {
                input_tokens: 4,
                output_tokens: 2
            }
        );
    }

    #[test]
    fn parses_tool_calls_and_finish_reasons() {
        let value = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "t1",
                        "type": "function",
                        "function": {"name": "Bash", "arguments": "{\"command\":\"ls\"}"},
                    }],
                },
                "finish_reason": "tool_calls",
            }],
        });
        let response = parse_response(&value).unwrap();
        assert_eq!(response.stop_reason, StopReason::ToolUse);
        assert_eq!(
            response.content,
            vec![ContentBlock::ToolUse {
                id: "t1".into(),
                name: "Bash".into(),
                input: json!({"command": "ls"}),
            }]
        );

        let length = json!({
            "choices": [{
                "message": {"content": "cut off"},
                "finish_reason": "length",
            }],
        });
        assert_eq!(
            parse_response(&length).unwrap().stop_reason,
            StopReason::MaxTokens
        );
    }

    #[test]
    fn rejects_malformed_responses() {
        assert!(matches!(
            parse_response(&json!({"choices": []})),
            Err(LlmError::Malformed(_))
        ));
        let bad_arguments = json!({
            "choices": [{
                "message": {"tool_calls": [{
                    "id": "t",
                    "function": {"name": "Bash", "arguments": "nope"},
                }]},
            }],
        });
        assert!(matches!(
            parse_response(&bad_arguments),
            Err(LlmError::Malformed(_))
        ));
    }
}
