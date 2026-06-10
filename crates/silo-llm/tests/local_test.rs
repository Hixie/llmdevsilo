//! Integration tests for the local model backend against a fake
//! OpenAI-compatible chat-completions server on loopback.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use silo_core::config::{LlmBackendKind, LlmConfig};
use silo_core::conversation::{CompletionRequest, ContentBlock, Message, Role, StopReason};
use silo_core::cost::QuotaConfig;
use silo_core::error::LlmError;
use silo_core::tool::{ToolAvailability, ToolDef};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[derive(Debug)]
struct RecordedRequest {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Value,
}

impl RecordedRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

struct FakeServer {
    base_url: String,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
}

impl FakeServer {
    fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }

    fn paths(&self) -> Vec<String> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(|r| r.path.clone())
            .collect()
    }
}

/// Serves the scripted (status, body) responses in order on the given
/// listener, one connection per request, recording each request.
fn serve_script(
    listener: TcpListener,
    responses: Vec<(u16, Value)>,
    recorded: Arc<Mutex<Vec<RecordedRequest>>>,
) {
    tokio::spawn(async move {
        for (status, body) in responses {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            if let Some(request) = read_request(&mut stream).await {
                recorded.lock().unwrap().push(request);
            }
            let body = body.to_string();
            let response = format!(
                "HTTP/1.1 {status} Scripted\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.shutdown().await;
        }
    });
}

async fn fake_server(responses: Vec<(u16, Value)>) -> FakeServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let requests: Arc<Mutex<Vec<RecordedRequest>>> = Arc::new(Mutex::new(Vec::new()));
    serve_script(listener, responses, requests.clone());
    FakeServer {
        base_url: format!("http://{addr}"),
        requests,
    }
}

async fn read_request(stream: &mut TcpStream) -> Option<RecordedRequest> {
    let mut buf = Vec::new();
    loop {
        let mut headers = [httparse::EMPTY_HEADER; 64];
        let mut request = httparse::Request::new(&mut headers);
        match request.parse(&buf) {
            Ok(httparse::Status::Complete(offset)) => {
                let method = request.method?.to_string();
                let path = request.path?.to_string();
                let header_vec: Vec<(String, String)> = request
                    .headers
                    .iter()
                    .map(|h| {
                        (
                            h.name.to_lowercase(),
                            String::from_utf8_lossy(h.value).to_string(),
                        )
                    })
                    .collect();
                let content_length = header_vec
                    .iter()
                    .find(|(n, _)| n == "content-length")
                    .and_then(|(_, v)| v.parse::<usize>().ok())
                    .unwrap_or(0);
                while buf.len() < offset + content_length {
                    let mut chunk = [0u8; 4096];
                    let n = stream.read(&mut chunk).await.ok()?;
                    if n == 0 {
                        return None;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                let body = serde_json::from_slice(&buf[offset..offset + content_length])
                    .unwrap_or(Value::Null);
                return Some(RecordedRequest {
                    method,
                    path,
                    headers: header_vec,
                    body,
                });
            }
            Ok(httparse::Status::Partial) => {
                let mut chunk = [0u8; 4096];
                let n = stream.read(&mut chunk).await.ok()?;
                if n == 0 {
                    return None;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            Err(_) => return None,
        }
    }
}

/// A loopback port with nothing listening on it.
async fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

fn local_config(base_url: &str) -> LlmConfig {
    LlmConfig {
        backend: LlmBackendKind::Local,
        model: "gpt-4o".into(),
        base_url: Some(base_url.into()),
        ..LlmConfig::default()
    }
}

fn tool_request() -> CompletionRequest {
    CompletionRequest {
        system: "local instructions".into(),
        messages: vec![
            Message::user_text("build the project"),
            Message::assistant(vec![
                ContentBlock::Text {
                    text: "starting".into(),
                },
                ContentBlock::ToolUse {
                    id: "lc_1".into(),
                    name: "Bash".into(),
                    input: json!({"command": "make", "jobs": 4}),
                },
            ]),
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "lc_1".into(),
                    content: "build ok".into(),
                    is_error: false,
                }],
            },
        ],
        tools: vec![ToolDef {
            name: "Bash".into(),
            description: "run a shell command".into(),
            input_schema: json!({"type": "object"}),
            availability: ToolAvailability::Both,
        }],
        max_tokens: 222,
    }
}

fn chat_completion_with_tool_call() -> Value {
    json!({
        "id": "chatcmpl-1",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "running the tests now",
                "tool_calls": [{
                    "id": "lc_2",
                    "type": "function",
                    "function": {"name": "Bash", "arguments": "{\"command\":\"make test\"}"},
                }],
            },
            "finish_reason": "tool_calls",
        }],
        "usage": {"prompt_tokens": 30, "completion_tokens": 14},
    })
}

#[tokio::test]
async fn sends_chat_completion_and_maps_tool_calls() {
    let server = fake_server(vec![(200, chat_completion_with_tool_call())]).await;
    let backend = silo_llm::local::create(&local_config(&server.base_url))
        .await
        .unwrap();
    assert_eq!(backend.id(), "local:gpt-4o");

    let response = backend.complete(&tool_request()).await.unwrap();

    // Response mapping.
    assert_eq!(response.stop_reason, StopReason::ToolUse);
    assert_eq!(response.usage.input_tokens, 30);
    assert_eq!(response.usage.output_tokens, 14);
    assert_eq!(
        response.content,
        vec![
            ContentBlock::Text {
                text: "running the tests now".into()
            },
            ContentBlock::ToolUse {
                id: "lc_2".into(),
                name: "Bash".into(),
                input: json!({"command": "make test"}),
            },
        ]
    );

    // Tokens are metered but pricing stays zero for the local backend,
    // even though the model name matches a cloud price-table entry.
    let usage = backend.usage();
    assert_eq!(usage.input_tokens, 30);
    assert_eq!(usage.output_tokens, 14);
    assert_eq!(usage.usd, 0.0);

    // Request shape.
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/v1/chat/completions");
    assert_eq!(request.header("authorization"), None);

    let body = &request.body;
    assert_eq!(body["model"], "gpt-4o");
    assert_eq!(body["max_tokens"], 222);
    assert_eq!(body["tool_choice"], "auto");
    assert_eq!(body["tools"][0]["type"], "function");
    assert_eq!(body["tools"][0]["function"]["name"], "Bash");

    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 4);
    assert_eq!(
        messages[0],
        json!({"role": "system", "content": "local instructions"})
    );
    assert_eq!(
        messages[1],
        json!({"role": "user", "content": "build the project"})
    );
    assert_eq!(messages[2]["role"], "assistant");
    assert_eq!(messages[2]["content"], "starting");
    assert_eq!(messages[2]["tool_calls"][0]["id"], "lc_1");
    // The tool input survives the round trip through the arguments string.
    let arguments: Value = serde_json::from_str(
        messages[2]["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(arguments, json!({"command": "make", "jobs": 4}));
    assert_eq!(
        messages[3],
        json!({"role": "tool", "tool_call_id": "lc_1", "content": "build ok"})
    );
}

#[tokio::test]
async fn finish_reason_length_maps_to_max_tokens() {
    let server = fake_server(vec![(
        200,
        json!({
            "choices": [{
                "message": {"role": "assistant", "content": "cut off"},
                "finish_reason": "length",
            }],
            "usage": {"prompt_tokens": 3, "completion_tokens": 222},
        }),
    )])
    .await;
    let backend = silo_llm::local::create(&local_config(&server.base_url))
        .await
        .unwrap();
    let response = backend.complete(&tool_request()).await.unwrap();
    assert_eq!(response.stop_reason, StopReason::MaxTokens);
    assert_eq!(response.text(), "cut off");
}

#[tokio::test]
async fn retries_on_500_then_succeeds() {
    let server = fake_server(vec![
        (500, json!({"error": "loading model"})),
        (200, chat_completion_with_tool_call()),
    ])
    .await;
    let backend = silo_llm::local::create(&local_config(&server.base_url))
        .await
        .unwrap();
    let response = backend.complete(&tool_request()).await.unwrap();
    assert_eq!(response.stop_reason, StopReason::ToolUse);
    assert_eq!(server.request_count(), 2);
}

#[tokio::test]
async fn api_error_carries_status() {
    let server = fake_server(vec![(404, json!({"error": "no such model"}))]).await;
    let backend = silo_llm::local::create(&local_config(&server.base_url))
        .await
        .unwrap();
    let error = backend.complete(&tool_request()).await.err().unwrap();
    assert!(matches!(error, LlmError::Api(_)));
    assert!(error.to_string().contains("status 404"));
    assert_eq!(server.request_count(), 1);
}

#[tokio::test]
async fn quota_blocks_requests_once_exhausted() {
    let server = fake_server(vec![(200, chat_completion_with_tool_call())]).await;
    let mut config = local_config(&server.base_url);
    config.quota = QuotaConfig {
        max_total_tokens: Some(20),
        max_usd: None,
    };
    let backend = silo_llm::local::create(&config).await.unwrap();

    // First call records 44 tokens, exceeding the quota for later calls.
    backend.complete(&tool_request()).await.unwrap();
    let error = backend.complete(&tool_request()).await.err().unwrap();
    assert!(matches!(error, LlmError::QuotaExceeded(_)));
    assert_eq!(server.request_count(), 1);
}

#[tokio::test]
async fn spawned_server_is_polled_until_ready() {
    // Reserve a port, then start listening on it only after a delay, so
    // the backend's first health probes fail with connection refused.
    let port = free_port().await;
    let base_url = format!("http://127.0.0.1:{port}");
    let requests: Arc<Mutex<Vec<RecordedRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let recorded = requests.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(600)).await;
        let listener = TcpListener::bind(("127.0.0.1", port)).await.unwrap();
        serve_script(
            listener,
            vec![
                (200, json!({"object": "list", "data": []})),
                (200, chat_completion_with_tool_call()),
            ],
            recorded,
        );
    });

    let mut config = local_config(&base_url);
    config.local_server_command = Some("sleep 30".into());
    let backend = silo_llm::local::create(&config).await.unwrap();

    let response = backend.complete(&tool_request()).await.unwrap();
    assert_eq!(response.stop_reason, StopReason::ToolUse);

    let paths: Vec<String> = requests
        .lock()
        .unwrap()
        .iter()
        .map(|r| r.path.clone())
        .collect();
    assert_eq!(paths, vec!["/v1/models", "/v1/chat/completions"]);
}

#[tokio::test]
async fn health_endpoint_is_used_as_fallback() {
    let server = fake_server(vec![
        (404, json!({"error": "not found"})),
        (200, json!({"status": "ok"})),
        (200, chat_completion_with_tool_call()),
    ])
    .await;
    let mut config = local_config(&server.base_url);
    config.local_server_command = Some("sleep 30".into());
    let backend = silo_llm::local::create(&config).await.unwrap();

    let response = backend.complete(&tool_request()).await.unwrap();
    assert_eq!(response.stop_reason, StopReason::ToolUse);
    assert_eq!(
        server.paths(),
        vec!["/v1/models", "/health", "/v1/chat/completions"]
    );
}

#[tokio::test]
async fn failing_server_command_reports_stderr_tail() {
    let port = free_port().await;
    let mut config = local_config(&format!("http://127.0.0.1:{port}"));
    config.local_server_command = Some("echo boom-diagnostic >&2; exit 3".into());
    let error = silo_llm::local::create(&config).await.err().unwrap();
    assert!(matches!(error, LlmError::Config(_)));
    let text = error.to_string();
    assert!(text.contains("boom-diagnostic"), "got: {text}");
    assert!(text.contains("exited"), "got: {text}");
}
