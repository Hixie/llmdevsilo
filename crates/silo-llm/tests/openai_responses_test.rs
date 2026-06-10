//! Integration tests for the OpenAI Responses backend against a fake
//! HTTP server on loopback.

use std::sync::{Arc, Mutex};

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
}

/// Serves the scripted (status, body) responses in order, one connection
/// per request, recording each request.
async fn fake_server(responses: Vec<(u16, Value)>) -> FakeServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let requests: Arc<Mutex<Vec<RecordedRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let recorded = requests.clone();
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

fn config(base_url: &str, key_env: &str, key: &str) -> LlmConfig {
    std::env::set_var(key_env, key);
    LlmConfig {
        backend: LlmBackendKind::OpenaiResponses,
        model: "gpt-4.1".into(),
        api_key_env: Some(key_env.into()),
        base_url: Some(base_url.into()),
        ..LlmConfig::default()
    }
}

fn tool_request() -> CompletionRequest {
    CompletionRequest {
        system: "you are a test".into(),
        messages: vec![
            Message::user_text("list the files"),
            Message::assistant(vec![
                ContentBlock::Text {
                    text: "running ls".into(),
                },
                ContentBlock::ToolUse {
                    id: "call_1".into(),
                    name: "Bash".into(),
                    input: json!({"command": "ls", "timeout_ms": 500}),
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
            description: "run a shell command".into(),
            input_schema: json!({"type": "object", "properties": {"command": {"type": "string"}}}),
            availability: ToolAvailability::Both,
        }],
        max_tokens: 333,
    }
}

fn completed_response() -> Value {
    json!({
        "id": "resp_1",
        "status": "completed",
        "output": [
            {"type": "message", "role": "assistant", "content": [
                {"type": "output_text", "text": "checking the directory"},
            ]},
            {"type": "function_call", "call_id": "call_2", "name": "Bash",
             "arguments": "{\"command\":\"ls -la\"}"},
        ],
        "usage": {"input_tokens": 40, "output_tokens": 12},
    })
}

#[tokio::test]
async fn sends_responses_request_and_maps_tool_calls() {
    let server = fake_server(vec![(200, completed_response())]).await;
    let backend = silo_llm::openai_responses::create(&config(
        &server.base_url,
        "SILO_TEST_RESP_FLOW_KEY",
        "sk-resp-flow",
    ))
    .await
    .unwrap();
    assert_eq!(backend.id(), "openai-responses:gpt-4.1");

    let response = backend.complete(&tool_request()).await.unwrap();

    // Response mapping.
    assert_eq!(response.stop_reason, StopReason::ToolUse);
    assert_eq!(response.usage.input_tokens, 40);
    assert_eq!(response.usage.output_tokens, 12);
    assert_eq!(
        response.content,
        vec![
            ContentBlock::Text {
                text: "checking the directory".into()
            },
            ContentBlock::ToolUse {
                id: "call_2".into(),
                name: "Bash".into(),
                input: json!({"command": "ls -la"}),
            },
        ]
    );

    // Usage metering with the built-in gpt-4.1 pricing (2.0 / 8.0 per
    // million tokens).
    let usage = backend.usage();
    assert_eq!(usage.input_tokens, 40);
    assert_eq!(usage.output_tokens, 12);
    let expected_usd = 40.0 * 2.0 / 1e6 + 12.0 * 8.0 / 1e6;
    assert!((usage.usd - expected_usd).abs() < 1e-12);

    // Request shape.
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/v1/responses");
    assert_eq!(request.header("authorization"), Some("Bearer sk-resp-flow"));
    let body = &request.body;
    assert_eq!(body["model"], "gpt-4.1");
    assert_eq!(body["instructions"], "you are a test");
    assert_eq!(body["store"], false);
    assert_eq!(body["max_output_tokens"], 333);
    assert_eq!(body["tools"][0]["type"], "function");
    assert_eq!(body["tools"][0]["name"], "Bash");
    assert_eq!(body["tools"][0]["strict"], false);

    let input = body["input"].as_array().unwrap();
    assert_eq!(input.len(), 4);
    assert_eq!(input[0]["type"], "message");
    assert_eq!(input[0]["role"], "user");
    assert_eq!(input[0]["content"][0]["type"], "input_text");
    assert_eq!(input[1]["role"], "assistant");
    assert_eq!(input[1]["content"][0]["type"], "output_text");
    assert_eq!(input[2]["type"], "function_call");
    assert_eq!(input[2]["call_id"], "call_1");
    // The tool input survives the round trip through the arguments string.
    let arguments: Value = serde_json::from_str(input[2]["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(arguments, json!({"command": "ls", "timeout_ms": 500}));
    assert_eq!(input[3]["type"], "function_call_output");
    assert_eq!(input[3]["call_id"], "call_1");
    assert_eq!(input[3]["output"], "file.txt");
}

#[tokio::test]
async fn default_api_key_env_is_openai_api_key() {
    let server = fake_server(vec![(
        200,
        json!({"status": "completed", "output": [], "usage": {"input_tokens": 1, "output_tokens": 1}}),
    )])
    .await;
    std::env::set_var("OPENAI_API_KEY", "sk-default-env");
    let config = LlmConfig {
        backend: LlmBackendKind::OpenaiResponses,
        model: "gpt-4.1".into(),
        api_key_env: None,
        base_url: Some(server.base_url.clone()),
        ..LlmConfig::default()
    };
    let backend = silo_llm::openai_responses::create(&config).await.unwrap();
    backend
        .complete(&CompletionRequest {
            system: String::new(),
            messages: vec![Message::user_text("hi")],
            tools: vec![],
            max_tokens: 16,
        })
        .await
        .unwrap();
    let requests = server.requests.lock().unwrap();
    assert_eq!(
        requests[0].header("authorization"),
        Some("Bearer sk-default-env")
    );
}

#[tokio::test]
async fn maps_incomplete_max_output_tokens_to_max_tokens() {
    let server = fake_server(vec![(
        200,
        json!({
            "status": "incomplete",
            "incomplete_details": {"reason": "max_output_tokens"},
            "output": [{"type": "message", "content": [
                {"type": "output_text", "text": "truncated answer"},
            ]}],
            "usage": {"input_tokens": 5, "output_tokens": 333},
        }),
    )])
    .await;
    let backend = silo_llm::openai_responses::create(&config(
        &server.base_url,
        "SILO_TEST_RESP_MAXTOK_KEY",
        "sk-x",
    ))
    .await
    .unwrap();
    let response = backend.complete(&tool_request()).await.unwrap();
    assert_eq!(response.stop_reason, StopReason::MaxTokens);
    assert_eq!(response.text(), "truncated answer");
}

#[tokio::test]
async fn retries_on_500_then_succeeds() {
    let server = fake_server(vec![
        (500, json!({"error": {"message": "server hiccup"}})),
        (200, completed_response()),
    ])
    .await;
    let backend = silo_llm::openai_responses::create(&config(
        &server.base_url,
        "SILO_TEST_RESP_RETRY_KEY",
        "sk-x",
    ))
    .await
    .unwrap();
    let response = backend.complete(&tool_request()).await.unwrap();
    assert_eq!(response.stop_reason, StopReason::ToolUse);
    assert_eq!(server.request_count(), 2);
}

#[tokio::test]
async fn client_errors_are_not_retried_and_carry_status() {
    let server = fake_server(vec![
        (400, json!({"error": {"message": "bad request"}})),
        (200, completed_response()),
    ])
    .await;
    let backend = silo_llm::openai_responses::create(&config(
        &server.base_url,
        "SILO_TEST_RESP_400_KEY",
        "sk-should-not-leak",
    ))
    .await
    .unwrap();
    let error = backend.complete(&tool_request()).await.err().unwrap();
    let text = error.to_string();
    assert!(matches!(error, LlmError::Api(_)));
    assert!(text.contains("status 400"), "got: {text}");
    assert!(text.contains("bad request"));
    assert!(!text.contains("sk-should-not-leak"));
    assert_eq!(server.request_count(), 1);
}

#[tokio::test]
async fn quota_blocks_requests_once_exhausted() {
    let server = fake_server(vec![(200, completed_response())]).await;
    let mut config = config(&server.base_url, "SILO_TEST_RESP_QUOTA_KEY", "sk-x");
    config.quota = QuotaConfig {
        max_total_tokens: Some(10),
        max_usd: None,
    };
    let backend = silo_llm::openai_responses::create(&config).await.unwrap();

    // First call records 52 tokens, exceeding the quota for later calls.
    backend.complete(&tool_request()).await.unwrap();
    let error = backend.complete(&tool_request()).await.err().unwrap();
    assert!(matches!(error, LlmError::QuotaExceeded(_)));
    // The second request never reached the server.
    assert_eq!(server.request_count(), 1);
}
