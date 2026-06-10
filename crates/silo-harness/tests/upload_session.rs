//! Client upload flow: an UploadFile script step emits a client-origin
//! FileShared event, the harness upload listener stores the bytes via the
//! scripted sandbox Write with `content_b64` intact, and a recorded upload
//! session replays from its journal.

mod common;

use serde_json::json;

use silo_core::conversation::{StopReason, TokenDelta};
use silo_core::helper::b64;
use silo_core::replay::{script_from_journal, FrontendStep, ScriptedToolExec, TestScript};
use silo_core::tool::ToolOutput;

const UPLOAD_BYTES: &[u8] = &[0xde, 0xad, 0xbe, 0xef, 0x00, 0xff];

fn upload_script(content_b64: &str) -> TestScript {
    TestScript {
        name: "upload_session".into(),
        llm: vec![common::llm_turn(
            Some("describe the upload"),
            Some("Received."),
            &[("t1", "Exit", json!({"message": "upload handled"}))],
            StopReason::ToolUse,
            TokenDelta {
                input_tokens: 5,
                output_tokens: 3,
            },
        )],
        tools: vec![ScriptedToolExec {
            expect_name: "Write".into(),
            expect_input: Some(json!({
                "path": "_uploads/blob.bin",
                "content_b64": content_b64,
            })),
            output: ToolOutput::ok("Wrote 6 bytes to _uploads/blob.bin"),
        }],
        frontend: vec![
            FrontendStep::UploadFile {
                name: "blob.bin".into(),
                content_b64: content_b64.into(),
            },
            FrontendStep::SendPrompt {
                text: "describe the upload".into(),
            },
            FrontendStep::ExpectShutdown {
                message_contains: Some("upload handled".into()),
            },
        ],
        network: vec![],
    }
}

#[tokio::test]
async fn upload_step_stores_the_file_via_the_scripted_write() {
    let fixture = common::Fixture::new();
    let content_b64 = b64(UPLOAD_BYTES);
    let script = common::shared(upload_script(&content_b64));

    let outcome = silo_harness::run(fixture.config(), fixture.options(script.clone()))
        .await
        .expect("session completes");

    assert_eq!(outcome.message.as_deref(), Some("upload handled"));
    assert!(
        script.finished(),
        "remaining: {}",
        script.remaining_summary()
    );
}

#[tokio::test]
async fn recorded_upload_session_replays_from_its_journal() {
    let mut fixture = common::Fixture::new();
    let config = fixture.config();
    let content_b64 = b64(UPLOAD_BYTES);

    // Session A: the original recording.
    let script_a = common::shared(upload_script(&content_b64));
    let outcome_a = silo_harness::run(config.clone(), fixture.options(script_a.clone()))
        .await
        .expect("session A completes");
    assert!(script_a.finished());
    let events_a = fixture.events();

    // The generated script carries the upload step and the stored-upload
    // Write execution.
    let generated = script_from_journal(&fixture.records(), "generated");
    assert!(
        generated
            .frontend
            .iter()
            .any(|step| matches!(step, FrontendStep::UploadFile { .. })),
        "frontend steps: {:?}",
        generated.frontend
    );
    assert_eq!(generated.tools.len(), 1);
    assert_eq!(generated.tools[0].expect_name, "Write");

    // Session B: replay against the same (still locked) workspace.
    fixture.reset_journal();
    let script_b = common::shared(generated);
    let outcome_b = silo_harness::run(config, fixture.options(script_b.clone()))
        .await
        .expect("session B completes");
    assert!(
        script_b.finished(),
        "remaining: {}",
        script_b.remaining_summary()
    );
    let events_b = fixture.events();

    assert_eq!(outcome_a.message, outcome_b.message);
    assert_eq!(
        common::event_kinds(&events_a),
        common::event_kinds(&events_b)
    );
}
