//! Integration tests for the OpenAI Realtime WebSocket backend against a
//! scripted WebSocket server on loopback.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use silo_core::config::{LlmBackendKind, LlmConfig};
use silo_core::conversation::{CompletionRequest, ContentBlock, Message, Role, StopReason};
use silo_core::cost::{Pricing, QuotaConfig};
use silo_core::error::LlmError;
use silo_core::tool::{ToolAvailability, ToolDef};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::WebSocketStream;

type ServerWs = WebSocketStream<TcpStream>;

#[derive(Clone, Debug)]
struct Handshake {
    uri: String,
    authorization: Option<String>,
    beta: Option<String>,
}

#[derive(Clone, Default)]
struct ServerState {
    handshakes: Arc<Mutex<Vec<Handshake>>>,
    received: Arc<Mutex<Vec<Value>>>,
    connections: Arc<AtomicU32>,
}

impl ServerState {
    fn received(&self) -> Vec<Value> {
        self.received.lock().unwrap().clone()
    }

    fn handshake(&self) -> Handshake {
        self.handshakes.lock().unwrap()[0].clone()
    }
}

// The callback's error type is fixed by tungstenite's Callback trait.
#[allow(clippy::result_large_err)]
async fn accept_ws(listener: &TcpListener, state: &ServerState) -> ServerWs {
    let (stream, _) = listener.accept().await.unwrap();
    state.connections.fetch_add(1, Ordering::SeqCst);
    let handshakes = state.handshakes.clone();
    let callback =
        move |request: &Request, response: Response| -> Result<Response, ErrorResponse> {
            let header = |name: &str| {
                request
                    .headers()
                    .get(name)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string)
            };
            handshakes.lock().unwrap().push(Handshake {
                uri: request.uri().to_string(),
                authorization: header("authorization"),
                beta: header("openai-beta"),
            });
            Ok(response)
        };
    tokio_tungstenite::accept_hdr_async(stream, callback)
        .await
        .unwrap()
}

async fn send_json(ws: &mut ServerWs, value: &Value) {
    ws.send(WsMessage::Text(value.to_string().into()))
        .await
        .unwrap();
}

async fn next_json(ws: &mut ServerWs) -> Option<Value> {
    while let Some(Ok(message)) = ws.next().await {
        if let WsMessage::Text(text) = message {
            return serde_json::from_str(text.as_str()).ok();
        }
    }
    None
}

/// Sends session.created, records client events until response.create,
/// then sends a delta event (which the client must skip) and the finale.
async fn drive_session(ws: &mut ServerWs, state: &ServerState, finale: Value) {
    send_json(
        ws,
        &json!({"type": "session.created", "session": {"id": "sess_1"}}),
    )
    .await;
    loop {
        let event = next_json(ws)
            .await
            .expect("client closed before response.create");
        let event_type = event
            .get("type")
            .and_then(Value::as_str)
            .map(str::to_string);
        state.received.lock().unwrap().push(event);
        if event_type.as_deref() == Some("response.create") {
            break;
        }
    }
    send_json(
        ws,
        &json!({"type": "response.output_text.delta", "delta": "partial"}),
    )
    .await;
    send_json(ws, &finale).await;
}

fn response_done() -> Value {
    json!({
        "type": "response.done",
        "response": {
            "status": "completed",
            "output": [
                {"type": "message", "role": "assistant", "content": [
                    {"type": "text", "text": "let me check"},
                ]},
                {"type": "function_call", "call_id": "rt_call_1", "name": "Bash",
                 "arguments": "{\"command\":\"cargo test\"}"},
            ],
            "usage": {"input_tokens": 100, "output_tokens": 25},
        },
    })
}

fn ws_config(base_url: &str) -> LlmConfig {
    LlmConfig {
        backend: LlmBackendKind::OpenaiWebsocket,
        model: "gpt-test-realtime".into(),
        api_key_env: None,
        base_url: Some(base_url.into()),
        pricing: Some(Pricing {
            usd_per_million_input_tokens: 1.0,
            usd_per_million_output_tokens: 2.0,
        }),
        ..LlmConfig::default()
    }
}

fn tool_request() -> CompletionRequest {
    CompletionRequest {
        system: "be careful".into(),
        messages: vec![
            Message::user_text("run the tests"),
            Message::assistant(vec![
                ContentBlock::Text {
                    text: "on it".into(),
                },
                ContentBlock::ToolUse {
                    id: "rt_call_0".into(),
                    name: "Bash".into(),
                    input: json!({"command": "ls"}),
                },
            ]),
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "rt_call_0".into(),
                    content: "Cargo.toml src".into(),
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
        max_tokens: 4096,
    }
}

#[tokio::test]
async fn full_session_flow_with_tools_and_usage() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("ws://{}", listener.local_addr().unwrap());
    let state = ServerState::default();
    let server_state = state.clone();
    tokio::spawn(async move {
        let mut ws = accept_ws(&listener, &server_state).await;
        drive_session(&mut ws, &server_state, response_done()).await;
    });

    std::env::set_var("OPENAI_API_KEY", "sk-realtime-test");
    let backend = silo_llm::openai_ws::create(&ws_config(&base_url))
        .await
        .unwrap();
    assert_eq!(backend.id(), "openai-ws:gpt-test-realtime");

    let response = backend.complete(&tool_request()).await.unwrap();

    // Response mapping.
    assert_eq!(response.stop_reason, StopReason::ToolUse);
    assert_eq!(response.usage.input_tokens, 100);
    assert_eq!(response.usage.output_tokens, 25);
    assert_eq!(
        response.content,
        vec![
            ContentBlock::Text {
                text: "let me check".into()
            },
            ContentBlock::ToolUse {
                id: "rt_call_1".into(),
                name: "Bash".into(),
                input: json!({"command": "cargo test"}),
            },
        ]
    );

    // Usage metering with the configured pricing.
    let usage = backend.usage();
    assert_eq!(usage.input_tokens, 100);
    assert_eq!(usage.output_tokens, 25);
    let expected_usd = 100.0 * 1.0 / 1e6 + 25.0 * 2.0 / 1e6;
    assert!((usage.usd - expected_usd).abs() < 1e-12);

    // Handshake: URL, Authorization from the default env var, beta header.
    let handshake = state.handshake();
    assert_eq!(handshake.uri, "/v1/realtime?model=gpt-test-realtime");
    assert_eq!(
        handshake.authorization.as_deref(),
        Some("Bearer sk-realtime-test")
    );
    assert_eq!(handshake.beta.as_deref(), Some("realtime=v1"));

    // Client events: session.update, four items, response.create.
    let received = state.received();
    assert_eq!(received.len(), 6);

    let update = &received[0];
    assert_eq!(update["type"], "session.update");
    let session = &update["session"];
    assert_eq!(session["modalities"], json!(["text"]));
    assert_eq!(session["instructions"], "be careful");
    assert_eq!(session["tool_choice"], "auto");
    assert_eq!(session["turn_detection"], Value::Null);
    assert_eq!(session["tools"][0]["type"], "function");
    assert_eq!(session["tools"][0]["name"], "Bash");
    assert!(
        !update.to_string().contains("audio"),
        "session.update must carry no audio fields: {update}"
    );

    for item_event in &received[1..5] {
        assert_eq!(item_event["type"], "conversation.item.create");
    }
    assert_eq!(received[1]["item"]["type"], "message");
    assert_eq!(received[1]["item"]["role"], "user");
    assert_eq!(received[1]["item"]["content"][0]["type"], "input_text");
    assert_eq!(received[2]["item"]["role"], "assistant");
    assert_eq!(received[2]["item"]["content"][0]["type"], "text");
    assert_eq!(received[3]["item"]["type"], "function_call");
    assert_eq!(received[3]["item"]["call_id"], "rt_call_0");
    let arguments: Value =
        serde_json::from_str(received[3]["item"]["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(arguments, json!({"command": "ls"}));
    assert_eq!(received[4]["item"]["type"], "function_call_output");
    assert_eq!(received[4]["item"]["call_id"], "rt_call_0");
    assert_eq!(received[4]["item"]["output"], "Cargo.toml src");

    let create = &received[5];
    assert_eq!(create["type"], "response.create");
    assert_eq!(create["response"]["modalities"], json!(["text"]));
    assert!(
        !create.to_string().contains("audio"),
        "response.create must carry no audio fields: {create}"
    );
}

#[tokio::test]
async fn error_event_maps_to_api_error_without_retry() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("ws://{}", listener.local_addr().unwrap());
    let state = ServerState::default();
    let server_state = state.clone();
    tokio::spawn(async move {
        let mut ws = accept_ws(&listener, &server_state).await;
        drive_session(
            &mut ws,
            &server_state,
            json!({"type": "error", "error": {"message": "model exploded"}}),
        )
        .await;
    });

    std::env::set_var("SILO_TEST_WS_ERR_KEY", "sk-x");
    let mut config = ws_config(&base_url);
    config.api_key_env = Some("SILO_TEST_WS_ERR_KEY".into());
    let backend = silo_llm::openai_ws::create(&config).await.unwrap();
    let error = backend.complete(&tool_request()).await.err().unwrap();
    assert!(matches!(error, LlmError::Api(_)));
    assert!(error.to_string().contains("model exploded"));
    assert_eq!(state.connections.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn dropped_connection_is_retried() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("ws://{}", listener.local_addr().unwrap());
    let state = ServerState::default();
    let server_state = state.clone();
    tokio::spawn(async move {
        // First connection: dropped before the WebSocket handshake.
        let (stream, _) = listener.accept().await.unwrap();
        server_state.connections.fetch_add(1, Ordering::SeqCst);
        drop(stream);
        // Second connection: full session.
        let mut ws = accept_ws(&listener, &server_state).await;
        drive_session(&mut ws, &server_state, response_done()).await;
    });

    std::env::set_var("SILO_TEST_WS_RETRY_KEY", "sk-x");
    let mut config = ws_config(&base_url);
    config.api_key_env = Some("SILO_TEST_WS_RETRY_KEY".into());
    let backend = silo_llm::openai_ws::create(&config).await.unwrap();
    let response = backend.complete(&tool_request()).await.unwrap();
    assert_eq!(response.stop_reason, StopReason::ToolUse);
    assert_eq!(state.connections.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn quota_blocks_second_completion() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("ws://{}", listener.local_addr().unwrap());
    let state = ServerState::default();
    let server_state = state.clone();
    tokio::spawn(async move {
        let mut ws = accept_ws(&listener, &server_state).await;
        let finale = json!({
            "type": "response.done",
            "response": {
                "status": "completed",
                "output": [{"type": "message", "content": [
                    {"type": "text", "text": "all done"},
                ]}],
                "usage": {"input_tokens": 9, "output_tokens": 3},
            },
        });
        drive_session(&mut ws, &server_state, finale).await;
    });

    std::env::set_var("SILO_TEST_WS_QUOTA_KEY", "sk-x");
    let mut config = ws_config(&base_url);
    config.api_key_env = Some("SILO_TEST_WS_QUOTA_KEY".into());
    config.quota = QuotaConfig {
        max_total_tokens: Some(10),
        max_usd: None,
    };
    let backend = silo_llm::openai_ws::create(&config).await.unwrap();

    let response = backend.complete(&tool_request()).await.unwrap();
    assert_eq!(response.stop_reason, StopReason::EndTurn);
    assert_eq!(response.text(), "all done");

    let error = backend.complete(&tool_request()).await.err().unwrap();
    assert!(matches!(error, LlmError::QuotaExceeded(_)));
    // No second connection was opened.
    assert_eq!(state.connections.load(Ordering::SeqCst), 1);
}
