//! SendUserFile flow: the harness reads the file through the sandbox Read
//! path and injects the base64 content before forwarding to the frontend.

mod common;

use serde_json::json;

use silo_core::conversation::{StopReason, TokenDelta};
use silo_core::event::{EventPayload, FileOrigin};
use silo_core::helper::b64;
use silo_core::replay::{FrontendStep, ScriptedToolExec, TestScript};
use silo_core::tool::ToolOutput;

#[tokio::test]
async fn sent_file_content_comes_from_the_sandbox_read_path() {
    let fixture = common::Fixture::new();
    let script = common::shared(TestScript {
        name: "send_file_session".into(),
        llm: vec![
            common::llm_turn(
                Some("send the report"),
                None,
                &[("t1", "SendUserFile", json!({"path": "out/report.txt"}))],
                StopReason::ToolUse,
                TokenDelta::default(),
            ),
            common::llm_turn(
                Some("sent report.txt"),
                None,
                &[("t2", "Exit", json!({"message": "report delivered"}))],
                StopReason::ToolUse,
                TokenDelta::default(),
            ),
        ],
        // The synthetic Read issued by the harness consumes this entry.
        tools: vec![ScriptedToolExec {
            expect_name: "Read".into(),
            expect_input: Some(json!({"path": "out/report.txt"})),
            output: ToolOutput::ok("hello user"),
        }],
        frontend: vec![
            FrontendStep::SendPrompt {
                text: "send the report".into(),
            },
            FrontendStep::ExpectShutdown {
                message_contains: Some("delivered".into()),
            },
        ],
        network: vec![],
    });

    let outcome = silo_harness::run(fixture.config(), fixture.options(script.clone()))
        .await
        .expect("session completes");
    assert_eq!(outcome.message.as_deref(), Some("report delivered"));
    assert!(
        script.finished(),
        "remaining: {}",
        script.remaining_summary()
    );

    let events = fixture.events();
    let shared_files: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::FileShared {
                name,
                content_b64,
                origin,
            } => Some((name.clone(), content_b64.clone(), origin.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(shared_files.len(), 1);
    assert_eq!(shared_files[0].0, "report.txt");
    assert_eq!(shared_files[0].1, b64(b"hello user"));
    assert!(matches!(shared_files[0].2, FileOrigin::Llm { .. }));
}
