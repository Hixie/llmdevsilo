//! Integration tests for the mock frontend: a scripted session driven
//! against a hand-pumped event bus.

use std::sync::Arc;

use serde_json::json;
use tokio::sync::mpsc;

use silo_core::clock::{FakeClock, SharedClock};
use silo_core::config::{FrontendConfig, FrontendKind};
use silo_core::error::FrontendError;
use silo_core::event::{EventBus, EventPayload};
use silo_core::journal::JournalHandle;
use silo_core::replay::{FrontendStep, SharedScript, TestScript};
use silo_core::sandbox::AccessReport;
use silo_core::tool::ToolCall;
use silo_core::traits::{Frontend, FrontendCommand, FrontendContext};

fn new_bus() -> EventBus {
    let clock: SharedClock = Arc::new(FakeClock::default());
    EventBus::new(clock.clone(), JournalHandle::disabled(clock))
}

fn mock_config() -> FrontendConfig {
    FrontendConfig {
        kind: FrontendKind::Mock,
        listen_addr: None,
        headless_prompt: None,
        issue_pairing_code: false,
    }
}

async fn start_mock(
    script: SharedScript,
    bus: &EventBus,
) -> (Box<dyn Frontend>, mpsc::Receiver<FrontendCommand>) {
    let (commands_tx, commands_rx) = mpsc::channel(4);
    let mut frontend = silo_frontend::create_frontend(&mock_config(), Some(script)).unwrap();
    let ctx = FrontendContext {
        harness_id: "mock-harness".into(),
        bus: bus.clone(),
        commands: commands_tx,
        access: AccessReport::default(),
        state_dir: std::env::temp_dir(),
        workspace: "/tmp/ws".into(),
    };
    frontend.start(ctx).await.unwrap();
    (frontend, commands_rx)
}

#[tokio::test]
async fn scripted_session_runs_to_completion() {
    let bus = new_bus();
    let script = SharedScript::new(TestScript {
        name: "happy path".into(),
        frontend: vec![
            FrontendStep::SendPrompt {
                text: "write hello world".into(),
            },
            FrontendStep::ExpectEvent {
                kind: "assistant_text".into(),
                contains: Some("hello".into()),
            },
            FrontendStep::SendPrompt {
                text: "now add tests".into(),
            },
            FrontendStep::AnswerQuestion {
                contains: Some("color".into()),
                answer: "blue".into(),
            },
            FrontendStep::ExpectShutdown {
                message_contains: Some("done".into()),
            },
        ],
        ..TestScript::default()
    });
    let (mut frontend, mut commands_rx) = start_mock(script.clone(), &bus).await;

    assert_eq!(
        frontend
            .tool_defs()
            .iter()
            .map(|d| d.name.as_str())
            .collect::<Vec<_>>(),
        vec!["AskUserQuestion", "SendUserFile", "Exit"]
    );

    // First prompt comes straight from the script.
    assert_eq!(
        frontend.next_user_input().await.unwrap(),
        "write hello world"
    );

    // The next step expects an assistant_text event before prompting again.
    bus.emit(EventPayload::AssistantText {
        agent: "agent-0".into(),
        text: "hello world it is".into(),
    });
    assert_eq!(frontend.next_user_input().await.unwrap(), "now add tests");

    // AskUserQuestion is answered by the scripted step, and the question
    // and answer both appear as events.
    let ask = ToolCall {
        id: "t1".into(),
        name: "AskUserQuestion".into(),
        input: json!({"question": "Which color theme?"}),
    };
    let output = frontend
        .run_tool(&"agent-0".to_string(), &ask)
        .await
        .unwrap();
    assert!(!output.is_error);
    assert_eq!(output.content, "blue");
    let kinds: Vec<&str> = bus.since(0).iter().map(|e| e.payload.kind()).collect();
    assert!(kinds.contains(&"question_asked"));
    assert!(kinds.contains(&"question_answered"));

    // SendUserFile emits a FileShared event with LLM origin.
    let send = ToolCall {
        id: "t2".into(),
        name: "SendUserFile".into(),
        input: json!({
            "path": "src/main.rs",
            "content_b64": silo_core::helper::b64(b"fn main() {}")
        }),
    };
    let output = frontend
        .run_tool(&"agent-0".to_string(), &send)
        .await
        .unwrap();
    assert_eq!(output.content, "sent main.rs to the user");
    assert!(bus
        .since(0)
        .iter()
        .any(|e| e.payload.kind() == "file_shared"));

    // Exit forwards a shutdown command to the harness.
    let exit = ToolCall {
        id: "t3".into(),
        name: "Exit".into(),
        input: json!({"message": "all done"}),
    };
    let output = frontend
        .run_tool(&"agent-0".to_string(), &exit)
        .await
        .unwrap();
    assert_eq!(output.content, "exiting");
    assert_eq!(
        commands_rx.recv().await.unwrap(),
        FrontendCommand::Shutdown {
            message: Some("all done".into())
        }
    );

    // Shutdown consumes the ExpectShutdown step; the script is finished.
    frontend.shutdown(Some("all done".into())).await.unwrap();
    assert!(
        script.finished(),
        "remaining: {}",
        script.remaining_summary()
    );
}

#[tokio::test]
async fn expect_event_blocks_until_the_event_is_observed() {
    let bus = new_bus();
    let script = SharedScript::new(TestScript {
        frontend: vec![
            FrontendStep::ExpectEvent {
                kind: "tool_result".into(),
                contains: Some("Bash".into()),
            },
            FrontendStep::SendPrompt {
                text: "next".into(),
            },
        ],
        ..TestScript::default()
    });
    let (frontend, _commands_rx) = start_mock(script, &bus).await;

    let input = frontend.next_user_input();
    let emit = async {
        bus.emit(EventPayload::ToolResult {
            agent: "agent-0".into(),
            tool_use_id: "t1".into(),
            tool_name: "Bash".into(),
            output: silo_core::tool::ToolOutput::ok("ok"),
        });
    };
    let (input, ()) = tokio::join!(input, emit);
    assert_eq!(input.unwrap(), "next");
}

#[tokio::test]
async fn prompt_request_against_wrong_step_is_a_script_mismatch() {
    let bus = new_bus();
    let script = SharedScript::new(TestScript {
        frontend: vec![FrontendStep::AnswerQuestion {
            contains: None,
            answer: "x".into(),
        }],
        ..TestScript::default()
    });
    let (frontend, _commands_rx) = start_mock(script, &bus).await;
    let result = frontend.next_user_input().await;
    assert!(matches!(result, Err(FrontendError::ScriptMismatch(_))));
}

#[tokio::test]
async fn question_against_exhausted_script_is_a_script_mismatch() {
    let bus = new_bus();
    let script = SharedScript::new(TestScript::default());
    let (frontend, _commands_rx) = start_mock(script, &bus).await;
    let call = ToolCall {
        id: "t1".into(),
        name: "AskUserQuestion".into(),
        input: json!({"question": "anyone there?"}),
    };
    let result = frontend.run_tool(&"agent-0".to_string(), &call).await;
    assert!(matches!(result, Err(FrontendError::ScriptMismatch(_))));
}

#[tokio::test]
async fn question_contains_mismatch_is_reported() {
    let bus = new_bus();
    let script = SharedScript::new(TestScript {
        frontend: vec![FrontendStep::AnswerQuestion {
            contains: Some("deploy".into()),
            answer: "yes".into(),
        }],
        ..TestScript::default()
    });
    let (frontend, _commands_rx) = start_mock(script, &bus).await;
    let call = ToolCall {
        id: "t1".into(),
        name: "AskUserQuestion".into(),
        input: json!({"question": "Run the tests?"}),
    };
    let result = frontend.run_tool(&"agent-0".to_string(), &call).await;
    assert!(matches!(result, Err(FrontendError::ScriptMismatch(_))));
}

#[tokio::test]
async fn shutdown_mismatch_is_reported_and_matching_shutdown_consumes() {
    let bus = new_bus();
    let script = SharedScript::new(TestScript {
        frontend: vec![FrontendStep::ExpectShutdown {
            message_contains: Some("success".into()),
        }],
        ..TestScript::default()
    });
    let (mut frontend, _commands_rx) = start_mock(script.clone(), &bus).await;
    assert!(matches!(
        frontend.shutdown(Some("failure".into())).await,
        Err(FrontendError::ScriptMismatch(_))
    ));
    // The mismatching attempt consumed the step; the script reports it.
    assert!(script.finished());
}
