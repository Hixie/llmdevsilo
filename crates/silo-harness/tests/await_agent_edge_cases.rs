//! AwaitAgent edge cases: awaiting when nothing is running returns an error
//! tool result and the session continues; spawning a subagent and ending the
//! turn without collecting it cancels the orphan at turn end.

mod common;

use serde_json::json;

use silo_core::conversation::{StopReason, TokenDelta};
use silo_core::event::EventPayload;
use silo_core::journal::JournalEntry;
use silo_core::replay::{FrontendStep, TestScript};

#[tokio::test]
async fn await_with_no_running_subagents_is_an_error_result() {
    let fixture = common::Fixture::new();
    let script = common::shared(TestScript {
        name: "await_nothing".into(),
        llm: vec![
            // Call AwaitAgent without ever spawning a subagent.
            common::llm_turn(
                Some("go"),
                Some("Trying to collect."),
                &[("a1", "AwaitAgent", json!({}))],
                StopReason::ToolUse,
                TokenDelta::default(),
            ),
            // The error result feeds back; the model recovers and exits.
            common::llm_turn(
                Some("no subagents are running"),
                Some("Nothing to wait for."),
                &[("x", "Exit", json!({"message": "recovered"}))],
                StopReason::ToolUse,
                TokenDelta::default(),
            ),
        ],
        tools: vec![],
        frontend: vec![
            FrontendStep::SendPrompt { text: "go".into() },
            FrontendStep::ExpectShutdown {
                message_contains: Some("recovered".into()),
            },
        ],
        network: vec![],
    });

    let outcome = silo_harness::run(fixture.config(), fixture.options(script.clone()))
        .await
        .expect("session completes");
    assert_eq!(outcome.message.as_deref(), Some("recovered"));
    assert!(
        script.finished(),
        "remaining: {}",
        script.remaining_summary()
    );

    // The AwaitAgent exec was journaled as an error result the model saw.
    let await_exec = fixture
        .records()
        .into_iter()
        .find_map(|record| match record.entry {
            JournalEntry::ToolExec {
                owner,
                call,
                output,
                ..
            } if owner == "harness" && call.name == "AwaitAgent" => Some(output),
            _ => None,
        })
        .expect("an AwaitAgent exec");
    assert!(await_exec.is_error);
    assert!(
        await_exec.content.contains("no subagents are running"),
        "content: {}",
        await_exec.content
    );
}

#[tokio::test]
async fn uncollected_subagent_is_cancelled_at_turn_end() {
    let fixture = common::Fixture::new();
    let script = common::shared(TestScript {
        name: "orphan_cancel".into(),
        llm: vec![
            // Spawn a subagent, then exit in the same response without ever
            // awaiting it.
            common::llm_turn(
                Some("spawn and go"),
                Some("Spawning then leaving."),
                &[
                    (
                        "t1",
                        "Agent",
                        json!({"prompt": "orphan work", "name": "orphan"}),
                    ),
                    ("x", "Exit", json!({"message": "left early"})),
                ],
                StopReason::ToolUse,
                TokenDelta::default(),
            ),
        ],
        tools: vec![],
        frontend: vec![
            FrontendStep::SendPrompt {
                text: "spawn and go".into(),
            },
            FrontendStep::ExpectShutdown {
                message_contains: Some("left early".into()),
            },
        ],
        network: vec![],
    });

    let outcome = silo_harness::run(fixture.config(), fixture.options(script.clone()))
        .await
        .expect("session completes");
    assert_eq!(outcome.message.as_deref(), Some("left early"));

    // The orphan was cancelled at turn end, with an error agent_completed
    // event and a journal note.
    let completed: Vec<_> = fixture
        .events()
        .into_iter()
        .filter_map(|event| match event.payload {
            EventPayload::AgentCompleted {
                agent,
                result,
                is_error,
            } => Some((agent, result, is_error)),
            _ => None,
        })
        .collect();
    assert_eq!(completed.len(), 1, "completed: {completed:?}");
    let (agent, result, is_error) = &completed[0];
    assert_eq!(agent, "agent-1");
    assert!(is_error, "the cancelled orphan is an error completion");
    assert!(result.contains("cancelled"), "cancel result: {result}");

    assert!(
        fixture.records().iter().any(|record| matches!(
            &record.entry,
            JournalEntry::Lifecycle { message }
                if message.contains("cancelled 1 uncollected subagent")
        )),
        "expected a cancellation lifecycle note"
    );
}
