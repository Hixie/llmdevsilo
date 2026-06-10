//! Journal-to-script replay: a session recorded in a journal replays
//! through `script_from_journal` and produces the same events and the same
//! LLM/tool journal entries.

mod common;

use serde_json::json;

use silo_core::conversation::{StopReason, TokenDelta};
use silo_core::journal::JournalEntry;
use silo_core::replay::{script_from_journal, FrontendStep, ScriptedToolExec, TestScript};
use silo_core::tool::ToolOutput;

fn session_a_script() -> TestScript {
    TestScript {
        name: "replay_roundtrip".into(),
        llm: vec![
            common::llm_turn(
                Some("build it"),
                Some("Listing the sources."),
                &[("t1", "Bash", json!({"command": "ls"}))],
                StopReason::ToolUse,
                TokenDelta {
                    input_tokens: 10,
                    output_tokens: 5,
                },
            ),
            common::llm_turn(
                Some("src"),
                Some("Build complete."),
                &[("t2", "Exit", json!({"message": "done: built"}))],
                StopReason::ToolUse,
                TokenDelta {
                    input_tokens: 20,
                    output_tokens: 7,
                },
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
    }
}

/// Journal entries that must match between the recorded and the replayed
/// session, as comparable JSON values with time fields removed.
fn comparable_entries(records: &[silo_core::journal::JournalRecord]) -> Vec<serde_json::Value> {
    records
        .iter()
        .filter(|record| {
            matches!(
                record.entry,
                JournalEntry::LlmRequest { .. }
                    | JournalEntry::LlmResponse { .. }
                    | JournalEntry::ToolExec { .. }
            )
        })
        .map(|record| serde_json::to_value(&record.entry).expect("entry serializes"))
        .collect()
}

fn payloads(events: &[silo_core::event::Event]) -> Vec<serde_json::Value> {
    events
        .iter()
        .map(|event| serde_json::to_value(&event.payload).expect("payload serializes"))
        .collect()
}

#[tokio::test]
async fn replayed_session_matches_the_recording() {
    let mut fixture = common::Fixture::new();
    let config = fixture.config();

    // Session A: the original recording.
    let script_a = common::shared(session_a_script());
    let outcome_a = silo_harness::run(config.clone(), fixture.options(script_a.clone()))
        .await
        .expect("session A completes");
    assert!(script_a.finished());
    let records_a = fixture.records();
    let events_a = fixture.events();

    // Generate the replay script from A's journal.
    let generated = script_from_journal(&records_a, "generated");
    assert_eq!(generated.llm.len(), 2);
    assert_eq!(generated.tools.len(), 1);
    assert_eq!(generated.frontend.len(), 2);

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
    let records_b = fixture.records();
    let events_b = fixture.events();

    assert_eq!(outcome_a.message, outcome_b.message);
    assert_eq!(payloads(&events_a), payloads(&events_b));
    assert_eq!(
        comparable_entries(&records_a),
        comparable_entries(&records_b)
    );
}
