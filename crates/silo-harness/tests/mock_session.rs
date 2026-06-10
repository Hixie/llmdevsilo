//! End-to-end mock session: prompt, one sandbox tool call, then Exit.

mod common;

use serde_json::json;

use silo_core::conversation::{StopReason, TokenDelta};
use silo_core::replay::{FrontendStep, ScriptedToolExec, TestScript};
use silo_core::tool::ToolOutput;

fn usage(input: u64, output: u64) -> TokenDelta {
    TokenDelta {
        input_tokens: input,
        output_tokens: output,
    }
}

#[tokio::test]
async fn mock_session_runs_to_exit() {
    let fixture = common::Fixture::new();
    let script = common::shared(TestScript {
        name: "mock_session".into(),
        llm: vec![
            common::llm_turn(
                Some("build it"),
                Some("Listing the sources."),
                &[("t1", "Bash", json!({"command": "ls"}))],
                StopReason::ToolUse,
                usage(10, 5),
            ),
            common::llm_turn(
                Some("src"),
                Some("Build complete."),
                &[("t2", "Exit", json!({"message": "done: built"}))],
                StopReason::ToolUse,
                usage(20, 7),
            ),
        ],
        tools: vec![ScriptedToolExec {
            expect_name: "Bash".into(),
            expect_input: Some(json!({"command": "ls"})),
            output: ToolOutput::ok("src"),
        }],
        frontend: vec![
            FrontendStep::SendPrompt {
                text: "build it".into(),
            },
            FrontendStep::ExpectShutdown {
                message_contains: Some("done".into()),
            },
        ],
        network: vec![],
    });

    let outcome = silo_harness::run(fixture.config(), fixture.options(script.clone()))
        .await
        .expect("session completes");

    let message = outcome.message.expect("outcome message");
    assert!(message.contains("done"), "message: {message}");
    assert!(outcome.journal_path.is_none());
    assert!(
        script.finished(),
        "remaining: {}",
        script.remaining_summary()
    );

    let events = fixture.events();
    assert_eq!(
        common::event_kinds(&events),
        vec![
            "harness_started",
            "access_report_updated",
            "awaiting_input",
            "user_prompt",
            "assistant_text",
            "cost_report",
            "tool_use",
            "tool_result",
            "assistant_text",
            "cost_report",
            "tool_use",
            "tool_result",
            "shutdown",
        ]
    );
    common::assert_strictly_increasing_from_zero(&events);

    // The journaled payloads carry the scripted content.
    let serialized = serde_json::to_string(&events).expect("events serialize");
    assert!(serialized.contains("build it"));
    assert!(serialized.contains("Listing the sources."));
    assert!(serialized.contains("done: built"));
}
