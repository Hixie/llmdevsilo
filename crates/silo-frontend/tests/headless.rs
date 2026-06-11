//! Integration tests for the headless frontend through the public factory.

use std::sync::Arc;

use serde_json::json;
use tokio::sync::mpsc;

use silo_core::clock::{FakeClock, SharedClock};
use silo_core::config::{FrontendConfig, FrontendKind};
use silo_core::error::FrontendError;
use silo_core::event::EventBus;
use silo_core::journal::JournalHandle;
use silo_core::sandbox::AccessReport;
use silo_core::tool::ToolCall;
use silo_core::traits::{FrontendCommand, FrontendContext};

fn headless_config(prompt: Option<&str>) -> FrontendConfig {
    FrontendConfig {
        kind: FrontendKind::Headless,
        headless_prompt: prompt.map(str::to_string),
        ..FrontendConfig::default()
    }
}

#[tokio::test]
async fn headless_session_prompts_then_reminds_then_exits() {
    let mut frontend =
        silo_frontend::create_frontend(&headless_config(Some("refactor the parser")), None)
            .unwrap();
    assert_eq!(frontend.kind(), "headless");

    let (commands_tx, mut commands_rx) = mpsc::channel(4);
    let clock: SharedClock = Arc::new(FakeClock::default());
    let ctx = FrontendContext {
        harness_id: "headless-h".into(),
        bus: EventBus::new(clock.clone(), JournalHandle::disabled(clock)),
        commands: commands_tx,
        access: AccessReport::default(),
        state_dir: std::env::temp_dir(),
        workspace: "/tmp/ws".into(),
        configured_read_allowlist: Vec::new(),
    };
    frontend.start(ctx).await.unwrap();

    let first = frontend.next_user_input().await.unwrap();
    assert_eq!(
        first,
        "refactor the parser\n\nWhen the task is complete, use the Exit tool with a final report message."
    );
    let second = frontend.next_user_input().await.unwrap();
    assert_eq!(
        second,
        "This is a non-interactive session; complete the task then use the Exit tool"
    );
    assert_eq!(frontend.next_user_input().await.unwrap(), second);

    let exit = ToolCall {
        id: "t1".into(),
        name: "Exit".into(),
        input: json!({"message": "parser refactored"}),
    };
    let output = frontend
        .run_tool(&"agent-0".to_string(), &exit)
        .await
        .unwrap();
    assert!(!output.is_error);
    assert_eq!(output.content, "exiting");
    assert_eq!(
        commands_rx.recv().await.unwrap(),
        FrontendCommand::Shutdown {
            message: Some("parser refactored".into())
        }
    );
    frontend
        .shutdown(Some("parser refactored".into()))
        .await
        .unwrap();
}

#[test]
fn headless_factory_requires_a_prompt() {
    let result = silo_frontend::create_frontend(&headless_config(None), None);
    assert!(matches!(result, Err(FrontendError::Setup(_))));
}

#[test]
fn mock_factory_requires_a_script() {
    let config = FrontendConfig {
        kind: FrontendKind::Mock,
        ..FrontendConfig::default()
    };
    let result = silo_frontend::create_frontend(&config, None);
    assert!(matches!(result, Err(FrontendError::Setup(_))));
}
