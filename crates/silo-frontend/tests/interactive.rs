//! Integration tests for the interactive frontend: real TLS over loopback,
//! authentication, event fan-out, catch-up, pairing, and question flow.

use std::sync::Arc;

use serde_json::json;
use tokio::sync::mpsc;

use silo_core::clock::{FakeClock, SharedClock};
use silo_core::config::{FrontendConfig, FrontendKind};
use silo_core::event::{Event, EventBus, EventPayload};
use silo_core::helper::b64;
use silo_core::journal::JournalHandle;
use silo_core::protocol::{AuthRequest, ClientMessage, ServerMessage};
use silo_core::sandbox::AccessReport;
use silo_core::tool::ToolCall;
use silo_core::traits::{Frontend, FrontendCommand, FrontendContext};
use silo_frontend::client;

const HARNESS_ID: &str = "testharness";

struct TestServer {
    frontend: Box<dyn Frontend>,
    bus: EventBus,
    commands_rx: mpsc::Receiver<FrontendCommand>,
    state_dir: tempfile::TempDir,
    run: silo_core::protocol::RunInfo,
    token: String,
}

fn new_bus() -> EventBus {
    let clock: SharedClock = Arc::new(FakeClock::default());
    EventBus::new(clock.clone(), JournalHandle::disabled(clock))
}

async fn boot() -> TestServer {
    boot_with_bus(new_bus()).await
}

async fn boot_with_bus(bus: EventBus) -> TestServer {
    let state_dir = tempfile::tempdir().unwrap();
    let (commands_tx, commands_rx) = mpsc::channel(8);
    let config = FrontendConfig {
        kind: FrontendKind::Interactive,
        listen_addr: None,
        headless_prompt: None,
        issue_pairing_code: false,
    };
    let mut frontend = silo_frontend::create_frontend(&config, None).unwrap();
    let ctx = FrontendContext {
        harness_id: HARNESS_ID.into(),
        bus: bus.clone(),
        commands: commands_tx,
        access: AccessReport {
            sandbox_kind: "mock".into(),
            workspace_mount: "/workspace".into(),
            ..AccessReport::default()
        },
        state_dir: state_dir.path().to_path_buf(),
        workspace: "/tmp/ws".into(),
    };
    frontend.start(ctx).await.unwrap();

    let harnesses = client::list_local_harnesses(state_dir.path());
    assert_eq!(harnesses.len(), 1);
    let run = harnesses[0].clone();
    assert_eq!(run.harness_id, HARNESS_ID);
    let token = std::fs::read_to_string(&run.local_token_path)
        .unwrap()
        .trim()
        .to_string();
    TestServer {
        frontend,
        bus,
        commands_rx,
        state_dir,
        run,
        token,
    }
}

async fn connect_and_auth(server: &TestServer) -> (client::ClientStream, client::AuthOk) {
    let (mut stream, hello) =
        client::connect(&server.run.addr, &server.run.cert_fingerprint_sha256)
            .await
            .unwrap();
    match hello {
        ServerMessage::Hello {
            harness_id,
            protocol_version,
        } => {
            assert_eq!(harness_id, HARNESS_ID);
            assert_eq!(protocol_version, silo_core::PROTOCOL_VERSION);
        }
        other => panic!("expected hello, got {other:?}"),
    }
    let auth = client::authenticate_local(&mut stream, &server.token)
        .await
        .unwrap();
    (stream, auth)
}

async fn await_event_kind(stream: &mut client::ClientStream, kind: &str) -> Event {
    loop {
        if let ServerMessage::Event { event } = client::recv_server(stream).await.unwrap() {
            if event.payload.kind() == kind {
                return event;
            }
        }
    }
}

#[tokio::test]
async fn prompt_is_fanned_out_queued_and_emitted_once() {
    let mut server = boot().await;
    let (mut a, _) = connect_and_auth(&server).await;
    let (mut b, _) = connect_and_auth(&server).await;

    client::send_client(
        &mut a,
        &ClientMessage::Prompt {
            text: "build the thing".into(),
        },
    )
    .await
    .unwrap();

    let event_a = await_event_kind(&mut a, "user_prompt").await;
    let event_b = await_event_kind(&mut b, "user_prompt").await;
    for event in [&event_a, &event_b] {
        match &event.payload {
            EventPayload::UserPrompt { client_id, text } => {
                assert_eq!(text, "build the thing");
                assert!(client_id.is_some());
            }
            other => panic!("expected user_prompt, got {other:?}"),
        }
    }
    assert_eq!(event_a.seq, event_b.seq);

    let input = server.frontend.next_user_input().await.unwrap();
    assert_eq!(input, "build the thing");

    let prompt_events: Vec<_> = server
        .bus
        .since(0)
        .into_iter()
        .filter(|e| e.payload.kind() == "user_prompt")
        .collect();
    assert_eq!(prompt_events.len(), 1);

    // Prompts sent while the model is busy stay queued in order.
    client::send_client(
        &mut b,
        &ClientMessage::Prompt {
            text: "first".into(),
        },
    )
    .await
    .unwrap();
    await_event_kind(&mut a, "user_prompt").await;
    client::send_client(
        &mut b,
        &ClientMessage::Prompt {
            text: "second".into(),
        },
    )
    .await
    .unwrap();
    await_event_kind(&mut a, "user_prompt").await;
    assert_eq!(server.frontend.next_user_input().await.unwrap(), "first");
    assert_eq!(server.frontend.next_user_input().await.unwrap(), "second");

    // Shutdown announces ShuttingDown to clients and removes the run file.
    server.frontend.shutdown(Some("bye".into())).await.unwrap();
    loop {
        match client::recv_server(&mut a).await {
            Ok(ServerMessage::ShuttingDown { message }) => {
                assert_eq!(message.as_deref(), Some("bye"));
                break;
            }
            Ok(_) => continue,
            Err(e) => panic!("connection closed before shutting_down: {e}"),
        }
    }
    assert!(client::list_local_harnesses(server.state_dir.path()).is_empty());
}

#[tokio::test]
async fn late_client_catches_up_with_request_events() {
    let bus = new_bus();
    bus.emit(EventPayload::AwaitingInput);
    bus.emit(EventPayload::AssistantText {
        agent: "agent-0".into(),
        text: "hello there".into(),
    });
    let mut server = boot_with_bus(bus).await;

    let (mut stream, auth) = connect_and_auth(&server).await;
    assert_eq!(auth.next_seq, 2);

    client::send_client(&mut stream, &ClientMessage::RequestEvents { from_seq: 0 })
        .await
        .unwrap();
    let backlog = loop {
        match client::recv_server(&mut stream).await.unwrap() {
            ServerMessage::Events { events } => break events,
            _ => continue,
        }
    };
    assert_eq!(backlog.len(), 2);
    assert_eq!(backlog[0].seq, 0);
    assert_eq!(backlog[0].payload.kind(), "awaiting_input");
    assert_eq!(backlog[1].seq, 1);
    assert_eq!(backlog[1].payload.kind(), "assistant_text");

    server.frontend.shutdown(None).await.unwrap();
}

#[tokio::test]
async fn pairing_then_challenge_login_works_and_misuse_fails() {
    let mut server = boot().await;
    let (mut local, _) = connect_and_auth(&server).await;

    // An authenticated client mints a pairing code.
    client::send_client(&mut local, &ClientMessage::RequestPairingCode)
        .await
        .unwrap();
    let code = loop {
        match client::recv_server(&mut local).await.unwrap() {
            ServerMessage::PairingCode {
                code,
                expires_in_secs,
            } => {
                assert_eq!(expires_in_secs, 120);
                break code;
            }
            _ => continue,
        }
    };
    assert_eq!(code.len(), 8);

    // Pair a new client with a fresh key.
    let key = client::generate_signing_key();
    let (mut remote, _) = client::connect(&server.run.addr, &server.run.cert_fingerprint_sha256)
        .await
        .unwrap();
    let paired = client::pair(&mut remote, &code, "test laptop", &key.verifying_key())
        .await
        .unwrap();
    let key_id = paired.key_id.clone().expect("pairing assigns a key id");
    drop(remote);

    // The key registry is persisted.
    let keys_path = silo_core::paths::harness_dir(server.state_dir.path(), HARNESS_ID)
        .join("authorized-keys.json");
    let registry = std::fs::read_to_string(&keys_path).unwrap();
    assert!(registry.contains(&key_id));
    assert!(registry.contains("test laptop"));

    // Reconnect and log in by signing a challenge.
    let (mut returning, _) = client::connect(&server.run.addr, &server.run.cert_fingerprint_sha256)
        .await
        .unwrap();
    let login = client::login_with_key(&mut returning, &key_id, &key)
        .await
        .unwrap();
    assert_eq!(login.key_id.as_deref(), Some(key_id.as_str()));

    // The pairing code is single use.
    let (mut reuse, _) = client::connect(&server.run.addr, &server.run.cert_fingerprint_sha256)
        .await
        .unwrap();
    let other_key = client::generate_signing_key();
    let reuse_result =
        client::pair(&mut reuse, &code, "imposter", &other_key.verifying_key()).await;
    assert!(matches!(
        reuse_result,
        Err(silo_core::error::FrontendError::Auth(_))
    ));

    // A garbage signature is rejected.
    let (mut forger, _) = client::connect(&server.run.addr, &server.run.cert_fingerprint_sha256)
        .await
        .unwrap();
    client::send_client(
        &mut forger,
        &ClientMessage::Authenticate {
            auth: AuthRequest::Challenge {
                key_id: key_id.clone(),
            },
        },
    )
    .await
    .unwrap();
    match client::recv_server(&mut forger).await.unwrap() {
        ServerMessage::AuthChallenge { .. } => {}
        other => panic!("expected a challenge, got {other:?}"),
    }
    client::send_client(
        &mut forger,
        &ClientMessage::Authenticate {
            auth: AuthRequest::Signature {
                key_id: key_id.clone(),
                signature_b64: b64(&[0u8; 64]),
            },
        },
    )
    .await
    .unwrap();
    match client::recv_server(&mut forger).await.unwrap() {
        ServerMessage::AuthError { message } => assert!(message.contains("signature")),
        other => panic!("expected an auth error, got {other:?}"),
    }

    server.frontend.shutdown(None).await.unwrap();
}

#[tokio::test]
async fn first_answer_wins_the_question_and_late_answers_are_ignored() {
    let mut server = boot().await;
    let (mut a, _) = connect_and_auth(&server).await;
    let (mut b, _) = connect_and_auth(&server).await;

    let call = ToolCall {
        id: "tool-use-1".into(),
        name: "AskUserQuestion".into(),
        input: json!({
            "question": "Which color?",
            "options": [{"label": "red"}, {"label": "blue"}]
        }),
    };
    let agent = "agent-0".to_string();
    let ask = server.frontend.run_tool(&agent, &call);

    let drive_clients = async {
        let asked_a = await_event_kind(&mut a, "question_asked").await;
        let asked_b = await_event_kind(&mut b, "question_asked").await;
        assert_eq!(asked_a.seq, asked_b.seq);
        let question_id = match &asked_a.payload {
            EventPayload::QuestionAsked { id, question, .. } => {
                assert_eq!(question.question, "Which color?");
                assert_eq!(question.options.len(), 2);
                id.clone()
            }
            other => panic!("expected question_asked, got {other:?}"),
        };

        // Client A answers first.
        client::send_client(
            &mut a,
            &ClientMessage::AnswerQuestion {
                question_id: question_id.clone(),
                answer: "red".into(),
            },
        )
        .await
        .unwrap();

        // Both clients see the answer event.
        for stream in [&mut a, &mut b] {
            let answered = await_event_kind(stream, "question_answered").await;
            match &answered.payload {
                EventPayload::QuestionAnswered { id, answer, .. } => {
                    assert_eq!(id, &question_id);
                    assert_eq!(answer, "red");
                }
                other => panic!("expected question_answered, got {other:?}"),
            }
        }

        // Client B answers late; the answer is ignored silently. The Pong
        // confirms the server processed the late answer before we assert.
        client::send_client(
            &mut b,
            &ClientMessage::AnswerQuestion {
                question_id: question_id.clone(),
                answer: "blue".into(),
            },
        )
        .await
        .unwrap();
        client::send_client(&mut b, &ClientMessage::Ping { nonce: 99 })
            .await
            .unwrap();
        loop {
            match client::recv_server(&mut b).await.unwrap() {
                ServerMessage::Pong { nonce } => {
                    assert_eq!(nonce, 99);
                    break;
                }
                _ => continue,
            }
        }
    };

    let (tool_result, ()) = tokio::join!(ask, drive_clients);
    let output = tool_result.unwrap();
    assert!(!output.is_error);
    assert_eq!(output.content, "red");

    let answered_events: Vec<_> = server
        .bus
        .since(0)
        .into_iter()
        .filter(|e| e.payload.kind() == "question_answered")
        .collect();
    assert_eq!(answered_events.len(), 1);

    server.frontend.shutdown(None).await.unwrap();
}

#[tokio::test]
async fn wrong_token_is_rejected_and_cannot_prompt() {
    let mut server = boot().await;
    let (mut stream, _) = client::connect(&server.run.addr, &server.run.cert_fingerprint_sha256)
        .await
        .unwrap();
    let denied = client::authenticate_local(&mut stream, "0123456789abcdef").await;
    assert!(matches!(
        denied,
        Err(silo_core::error::FrontendError::Auth(_))
    ));

    // The server has closed the connection; a Prompt goes nowhere.
    let _ = client::send_client(
        &mut stream,
        &ClientMessage::Prompt {
            text: "sneaky".into(),
        },
    )
    .await;
    assert!(client::recv_server(&mut stream).await.is_err());
    assert!(server
        .bus
        .since(0)
        .iter()
        .all(|e| e.payload.kind() != "user_prompt"));

    server.frontend.shutdown(None).await.unwrap();
}

#[tokio::test]
async fn client_rejects_a_server_with_the_wrong_fingerprint() {
    let mut server = boot().await;
    let wrong = "ab".repeat(32);
    assert_ne!(wrong, server.run.cert_fingerprint_sha256);
    let result = client::connect(&server.run.addr, &wrong).await;
    assert!(result.is_err());
    server.frontend.shutdown(None).await.unwrap();
}

#[tokio::test]
async fn upload_access_report_cost_and_send_user_file_flow() {
    let mut server = boot().await;
    let (mut a, _) = connect_and_auth(&server).await;
    let (mut b, _) = connect_and_auth(&server).await;

    // Uploads are announced to every client with the uploader's id.
    client::send_client(
        &mut a,
        &ClientMessage::UploadFile {
            name: "notes.txt".into(),
            content_b64: b64(b"hello"),
        },
    )
    .await
    .unwrap();
    let shared_event = await_event_kind(&mut b, "file_shared").await;
    match &shared_event.payload {
        EventPayload::FileShared { name, origin, .. } => {
            assert_eq!(name, "notes.txt");
            assert!(matches!(
                origin,
                silo_core::event::FileOrigin::Client { .. }
            ));
        }
        other => panic!("expected file_shared, got {other:?}"),
    }

    // The access report is served from the context.
    client::send_client(&mut a, &ClientMessage::RequestAccessReport)
        .await
        .unwrap();
    loop {
        match client::recv_server(&mut a).await.unwrap() {
            ServerMessage::AccessReport { report } => {
                assert_eq!(report.sandbox_kind, "mock");
                break;
            }
            _ => continue,
        }
    }

    // Cost reports are cached per backend, latest wins.
    server.bus.emit(EventPayload::CostReport {
        backend: "mock".into(),
        usage: silo_core::cost::UsageSnapshot {
            input_tokens: 10,
            output_tokens: 5,
            usd: 0.0,
        },
        quota: silo_core::cost::QuotaConfig::default(),
    });
    server.bus.emit(EventPayload::CostReport {
        backend: "mock".into(),
        usage: silo_core::cost::UsageSnapshot {
            input_tokens: 20,
            output_tokens: 9,
            usd: 0.0,
        },
        quota: silo_core::cost::QuotaConfig::default(),
    });
    // Both cost events reach the clients; the cache converges on the
    // latest report, so the request is repeated until it reflects it.
    await_event_kind(&mut a, "cost_report").await;
    await_event_kind(&mut a, "cost_report").await;
    loop {
        client::send_client(&mut a, &ClientMessage::RequestCost)
            .await
            .unwrap();
        let entries = loop {
            match client::recv_server(&mut a).await.unwrap() {
                ServerMessage::Cost { entries } => break entries,
                _ => continue,
            }
        };
        if entries.len() == 1 && entries[0].usage.input_tokens == 20 {
            assert_eq!(entries[0].backend, "mock");
            break;
        }
    }

    // SendUserFile emits a FileShared event with LLM origin.
    let call = ToolCall {
        id: "tool-use-2".into(),
        name: "SendUserFile".into(),
        input: json!({
            "path": "out/report.md",
            "caption": "the report",
            "content_b64": b64(b"# report")
        }),
    };
    let output = server
        .frontend
        .run_tool(&"agent-0".to_string(), &call)
        .await
        .unwrap();
    assert!(!output.is_error);
    assert_eq!(output.content, "sent report.md to the user");
    let sent = await_event_kind(&mut b, "file_shared").await;
    match &sent.payload {
        EventPayload::FileShared { name, origin, .. } => {
            assert_eq!(name, "report.md");
            assert!(matches!(origin, silo_core::event::FileOrigin::Llm { .. }));
        }
        other => panic!("expected file_shared, got {other:?}"),
    }

    server.frontend.shutdown(None).await.unwrap();
}

#[tokio::test]
async fn client_shutdown_message_forwards_a_frontend_command() {
    let mut server = boot().await;
    let (mut a, _) = connect_and_auth(&server).await;
    client::send_client(&mut a, &ClientMessage::Shutdown)
        .await
        .unwrap();
    assert_eq!(
        server.commands_rx.recv().await.unwrap(),
        FrontendCommand::Shutdown { message: None }
    );
    server.frontend.shutdown(None).await.unwrap();
}

#[tokio::test]
async fn interactive_tool_defs_are_question_and_file() {
    let server = boot().await;
    let names: Vec<_> = server
        .frontend
        .tool_defs()
        .into_iter()
        .map(|d| d.name)
        .collect();
    assert_eq!(names, vec!["AskUserQuestion", "SendUserFile"]);
}
