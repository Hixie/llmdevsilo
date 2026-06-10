//! Integration tests for the Anthropic Messages API backend, against a
//! loopback HTTP server that records requests and replays canned responses.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use silo_core::config::{LlmBackendKind, LlmConfig};
use silo_core::conversation::{
    CompletionRequest, ContentBlock, Message, Role, StopReason, TokenDelta,
};
use silo_core::cost::{Pricing, QuotaConfig};
use silo_core::error::LlmError;
use silo_core::tool::{ToolAvailability, ToolDef};
use silo_core::traits::LlmBackend;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[derive(Clone)]
struct RecordedRequest {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl RecordedRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(header_name, _)| header_name == name)
            .map(|(_, value)| value.as_str())
    }

    fn body_json(&self) -> Value {
        serde_json::from_slice(&self.body).expect("request body is JSON")
    }
}

/// Loopback HTTP server. Every connection carries one request; the server
/// records it and answers with the next canned response (or a fallback 500
/// when the canned responses run out).
struct FakeServer {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
}

impl FakeServer {
    async fn start(responses: Vec<String>) -> FakeServer {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        let requests: Arc<Mutex<Vec<RecordedRequest>>> = Arc::new(Mutex::new(Vec::new()));
        let queue = Arc::new(Mutex::new(VecDeque::from(responses)));
        let recorded = requests.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                if let Some(request) = read_http_request(&mut stream).await {
                    recorded.lock().unwrap().push(request);
                    let response = queue.lock().unwrap().pop_front().unwrap_or_else(|| {
                        http_response(500, "Internal Server Error", "no canned response left")
                    });
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.flush().await;
                }
            }
        });
        FakeServer { addr, requests }
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }

    fn request(&self, index: usize) -> RecordedRequest {
        self.requests.lock().unwrap()[index].clone()
    }
}

async fn read_http_request(stream: &mut TcpStream) -> Option<RecordedRequest> {
    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let parsed = {
            let mut headers = [httparse::EMPTY_HEADER; 64];
            let mut request = httparse::Request::new(&mut headers);
            match request.parse(&buffer) {
                Ok(httparse::Status::Complete(header_len)) => {
                    let method = request.method?.to_string();
                    let path = request.path?.to_string();
                    let header_list: Vec<(String, String)> = request
                        .headers
                        .iter()
                        .map(|h| {
                            (
                                h.name.to_ascii_lowercase(),
                                String::from_utf8_lossy(h.value).into_owned(),
                            )
                        })
                        .collect();
                    Some((header_len, method, path, header_list))
                }
                Ok(httparse::Status::Partial) => None,
                Err(_) => return None,
            }
        };
        if let Some((header_len, method, path, headers)) = parsed {
            let content_length = headers
                .iter()
                .find(|(name, _)| name == "content-length")
                .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            while buffer.len() < header_len + content_length {
                let n = stream.read(&mut chunk).await.ok()?;
                if n == 0 {
                    return None;
                }
                buffer.extend_from_slice(&chunk[..n]);
            }
            let body = buffer[header_len..header_len + content_length].to_vec();
            return Some(RecordedRequest {
                method,
                path,
                headers,
                body,
            });
        }
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..n]);
    }
}

fn http_response(status: u16, reason: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn message_response(
    content: Value,
    stop_reason: &str,
    input_tokens: u64,
    output_tokens: u64,
) -> String {
    let body = json!({
        "id": "msg_test",
        "type": "message",
        "role": "assistant",
        "model": "claude-test",
        "content": content,
        "stop_reason": stop_reason,
        "usage": {"input_tokens": input_tokens, "output_tokens": output_tokens},
    });
    http_response(200, "OK", &body.to_string())
}

fn text_response(text: &str, stop_reason: &str, input_tokens: u64, output_tokens: u64) -> String {
    message_response(
        json!([{"type": "text", "text": text}]),
        stop_reason,
        input_tokens,
        output_tokens,
    )
}

/// Builds a backend pointed at the fake server. Each test uses its own
/// environment variable name so tests do not interfere when run in
/// parallel.
async fn anthropic_backend(
    server: &FakeServer,
    env_var: &str,
    key: &str,
    configure: impl FnOnce(&mut LlmConfig),
) -> Arc<dyn LlmBackend> {
    std::env::set_var(env_var, key);
    let mut config = LlmConfig {
        backend: LlmBackendKind::Anthropic,
        model: "claude-test".to_string(),
        api_key_env: Some(env_var.to_string()),
        base_url: Some(server.base_url()),
        ..LlmConfig::default()
    };
    configure(&mut config);
    silo_llm::anthropic::create(&config)
        .await
        .expect("backend creation succeeds")
}

fn simple_request(text: &str) -> CompletionRequest {
    CompletionRequest {
        system: String::new(),
        messages: vec![Message::user_text(text)],
        tools: vec![],
        max_tokens: 256,
    }
}

#[tokio::test]
async fn sends_full_messages_api_request() {
    let server = FakeServer::start(vec![text_response("done", "end_turn", 5, 7)]).await;
    let backend = anthropic_backend(
        &server,
        "SILO_TEST_ANTHROPIC_KEY_FULL_REQUEST",
        "sk-test-full",
        |_| {},
    )
    .await;
    assert_eq!(backend.id(), "anthropic:claude-test");

    let request = CompletionRequest {
        system: "be helpful".into(),
        messages: vec![
            Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "list the files".into(),
                }],
            },
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Text {
                        text: "running ls".into(),
                    },
                    ContentBlock::ToolUse {
                        id: "tu_1".into(),
                        name: "Bash".into(),
                        input: json!({"command": "ls"}),
                    },
                ],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "tu_1".into(),
                    content: "file.txt".into(),
                    is_error: false,
                }],
            },
        ],
        tools: vec![ToolDef {
            name: "Bash".into(),
            description: "Runs a shell command.".into(),
            input_schema: json!({"type": "object", "properties": {"command": {"type": "string"}}}),
            availability: ToolAvailability::Both,
        }],
        max_tokens: 1024,
    };
    let response = backend
        .complete(&request)
        .await
        .expect("completion succeeds");
    assert_eq!(response.text(), "done");

    assert_eq!(server.request_count(), 1);
    let recorded = server.request(0);
    assert_eq!(recorded.method, "POST");
    assert_eq!(recorded.path, "/v1/messages");
    assert_eq!(recorded.header("x-api-key"), Some("sk-test-full"));
    assert_eq!(recorded.header("anthropic-version"), Some("2023-06-01"));
    assert!(recorded
        .header("content-type")
        .expect("content-type present")
        .starts_with("application/json"));

    assert_eq!(
        recorded.body_json(),
        json!({
            "model": "claude-test",
            "max_tokens": 1024,
            "system": "be helpful",
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "list the files"}]},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "running ls"},
                    {"type": "tool_use", "id": "tu_1", "name": "Bash", "input": {"command": "ls"}},
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "tu_1", "content": "file.txt", "is_error": false},
                ]},
            ],
            "tools": [{
                "name": "Bash",
                "description": "Runs a shell command.",
                "input_schema": {"type": "object", "properties": {"command": {"type": "string"}}},
            }],
        })
    );
}

#[tokio::test]
async fn parses_mixed_content_and_meters_usage_with_explicit_pricing() {
    let server = FakeServer::start(vec![message_response(
        json!([
            {"type": "text", "text": "Let me check."},
            {"type": "tool_use", "id": "tu_9", "name": "Read", "input": {"path": "/tmp/x"}},
        ]),
        "tool_use",
        100,
        200,
    )])
    .await;
    let backend = anthropic_backend(
        &server,
        "SILO_TEST_ANTHROPIC_KEY_PARSE",
        "sk-test-parse",
        |config| {
            config.pricing = Some(Pricing {
                usd_per_million_input_tokens: 5.0,
                usd_per_million_output_tokens: 10.0,
            });
        },
    )
    .await;

    let response = backend
        .complete(&simple_request("hi"))
        .await
        .expect("completion succeeds");
    assert_eq!(
        response.content,
        vec![
            ContentBlock::Text {
                text: "Let me check.".into()
            },
            ContentBlock::ToolUse {
                id: "tu_9".into(),
                name: "Read".into(),
                input: json!({"path": "/tmp/x"}),
            },
        ]
    );
    assert_eq!(response.stop_reason, StopReason::ToolUse);
    assert_eq!(
        response.usage,
        TokenDelta {
            input_tokens: 100,
            output_tokens: 200
        }
    );

    let usage = backend.usage();
    assert_eq!(usage.input_tokens, 100);
    assert_eq!(usage.output_tokens, 200);
    let expected_usd = (100.0 * 5.0 + 200.0 * 10.0) / 1_000_000.0;
    assert!((usage.usd - expected_usd).abs() < 1e-12);
}

#[tokio::test]
async fn maps_stop_reasons() {
    let server = FakeServer::start(vec![
        text_response("a", "end_turn", 1, 1),
        text_response("b", "max_tokens", 1, 1),
        text_response("c", "pause_turn", 1, 1),
    ])
    .await;
    let backend = anthropic_backend(
        &server,
        "SILO_TEST_ANTHROPIC_KEY_STOP_REASONS",
        "sk-test-stop",
        |_| {},
    )
    .await;

    let request = simple_request("go");
    let first = backend.complete(&request).await.expect("first completion");
    assert_eq!(first.stop_reason, StopReason::EndTurn);
    let second = backend.complete(&request).await.expect("second completion");
    assert_eq!(second.stop_reason, StopReason::MaxTokens);
    let third = backend.complete(&request).await.expect("third completion");
    assert_eq!(third.stop_reason, StopReason::Other("pause_turn".into()));
}

#[tokio::test]
async fn retries_after_500_and_succeeds() {
    let server = FakeServer::start(vec![
        http_response(500, "Internal Server Error", "transient failure"),
        text_response("after retry", "end_turn", 1, 2),
    ])
    .await;
    let backend = anthropic_backend(
        &server,
        "SILO_TEST_ANTHROPIC_KEY_RETRY",
        "sk-test-retry",
        |_| {},
    )
    .await;

    let response = backend
        .complete(&simple_request("flaky"))
        .await
        .expect("completion succeeds after retry");
    assert_eq!(response.text(), "after retry");
    assert_eq!(server.request_count(), 2);
}

#[tokio::test]
async fn client_error_fails_without_retry() {
    let error_body = r#"{"type":"error","error":{"type":"invalid_request_error","message":"bad"}}"#;
    let server = FakeServer::start(vec![http_response(400, "Bad Request", error_body)]).await;
    let backend = anthropic_backend(
        &server,
        "SILO_TEST_ANTHROPIC_KEY_NO_RETRY",
        "sk-test-no-retry",
        |_| {},
    )
    .await;

    let error = backend.complete(&simple_request("oops")).await.unwrap_err();
    match &error {
        LlmError::Api(message) => {
            assert!(message.starts_with("status 400"), "message: {message}");
            assert!(
                message.contains("invalid_request_error"),
                "message: {message}"
            );
        }
        other => panic!("expected Api error, got {other:?}"),
    }
    assert!(!error.to_string().contains("sk-test-no-retry"));
    assert_eq!(server.request_count(), 1);
}

#[tokio::test]
async fn missing_api_key_env_is_a_config_error() {
    let config = LlmConfig {
        backend: LlmBackendKind::Anthropic,
        api_key_env: Some("SILO_TEST_ANTHROPIC_KEY_NEVER_SET".into()),
        ..LlmConfig::default()
    };
    match silo_llm::anthropic::create(&config).await {
        Err(LlmError::Config(message)) => {
            assert!(message.contains("SILO_TEST_ANTHROPIC_KEY_NEVER_SET"));
        }
        Err(other) => panic!("expected Config error, got {other:?}"),
        Ok(_) => panic!("expected create to fail"),
    }
}

#[tokio::test]
async fn quota_blocks_the_second_request_before_any_http() {
    let server = FakeServer::start(vec![
        text_response("one", "end_turn", 6, 6),
        text_response("two", "end_turn", 6, 6),
    ])
    .await;
    let backend = anthropic_backend(
        &server,
        "SILO_TEST_ANTHROPIC_KEY_QUOTA",
        "sk-test-quota",
        |config| {
            config.quota = QuotaConfig {
                max_total_tokens: Some(10),
                max_usd: None,
            };
        },
    )
    .await;

    backend
        .complete(&simple_request("first"))
        .await
        .expect("first completion succeeds");
    assert_eq!(backend.usage().total_tokens(), 12);

    let error = backend
        .complete(&simple_request("second"))
        .await
        .unwrap_err();
    assert!(matches!(error, LlmError::QuotaExceeded(_)));
    assert_eq!(server.request_count(), 1);
}
