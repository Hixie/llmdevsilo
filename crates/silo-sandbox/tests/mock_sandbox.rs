//! The mock sandbox through the public `create_sandbox` entry point.

use std::sync::Arc;

use serde_json::json;
use silo_core::clock::FakeClock;
use silo_core::config::{SandboxConfig, SandboxKind};
use silo_core::error::SandboxError;
use silo_core::journal::JournalHandle;
use silo_core::replay::{ScriptedToolExec, SharedScript, TestScript};
use silo_core::tool::{ToolCall, ToolOutput};

fn journal() -> JournalHandle {
    JournalHandle::disabled(Arc::new(FakeClock::default()))
}

#[tokio::test]
async fn create_sandbox_builds_a_scripted_mock() {
    let script = SharedScript::new(TestScript {
        name: "session".into(),
        tools: vec![
            ScriptedToolExec {
                expect_name: "Bash".into(),
                expect_input: Some(json!({"command": "ls"})),
                output: ToolOutput::ok("Cargo.toml\nsrc"),
            },
            ScriptedToolExec {
                expect_name: "Read".into(),
                expect_input: Some(json!({"path": "Cargo.toml"})),
                output: ToolOutput::ok("[package]"),
            },
        ],
        ..TestScript::default()
    });
    let config = SandboxConfig {
        kind: SandboxKind::Mock,
        ..SandboxConfig::default()
    };
    let mut sandbox = silo_sandbox::create_sandbox(&config, None, Some(script.clone()), journal())
        .await
        .unwrap();
    sandbox.start().await.unwrap();
    assert_eq!(sandbox.kind(), "mock");

    let agent = "agent-0".to_string();
    let output = sandbox
        .run_tool(
            &agent,
            &ToolCall {
                id: "1".into(),
                name: "Bash".into(),
                input: json!({"command": "ls", "timeout_ms": 1000}),
            },
        )
        .await
        .unwrap();
    assert_eq!(output.content, "Cargo.toml\nsrc");

    let output = sandbox
        .run_tool(
            &agent,
            &ToolCall {
                id: "2".into(),
                name: "Read".into(),
                input: json!({"path": "Cargo.toml"}),
            },
        )
        .await
        .unwrap();
    assert_eq!(output.content, "[package]");
    assert!(
        script.finished(),
        "remaining: {}",
        script.remaining_summary()
    );

    // The script is exhausted: any further call is a mismatch.
    let err = sandbox
        .run_tool(
            &agent,
            &ToolCall {
                id: "3".into(),
                name: "Bash".into(),
                input: json!({"command": "true"}),
            },
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, SandboxError::ScriptMismatch(_)),
        "got {err:?}"
    );

    sandbox.shutdown().await.unwrap();
}

#[tokio::test]
async fn create_sandbox_requires_a_script_for_the_mock() {
    let config = SandboxConfig {
        kind: SandboxKind::Mock,
        ..SandboxConfig::default()
    };
    let result = silo_sandbox::create_sandbox(&config, None, None, journal()).await;
    match result {
        Err(err) => assert!(matches!(err, SandboxError::Setup(_)), "got {err:?}"),
        Ok(_) => panic!("expected a Setup error"),
    }
}
