//! Parallel subagents: the top-level agent spawns two subagents at once,
//! then collects both with AwaitAgent. Because the two run concurrently and
//! the mock matches scripted turns by content, the completion order is not
//! fixed; the test asserts on the set of collected results, each attributed
//! to the right agent id and name.

mod common;

use std::collections::BTreeSet;

use serde_json::json;

use silo_core::conversation::{StopReason, TokenDelta};
use silo_core::event::EventPayload;
use silo_core::journal::JournalEntry;
use silo_core::replay::{FrontendStep, TestScript};

/// The shared part of both variants: spawn alpha and beta, each scripted to
/// return a distinct result keyed by its prompt.
fn base_llm() -> Vec<silo_core::replay::ScriptedLlmTurn> {
    vec![
        // Top level: spawn both subagents in one response.
        common::llm_turn(
            Some("do both"),
            Some("Delegating two tasks."),
            &[
                (
                    "t1",
                    "Agent",
                    json!({"prompt": "alpha task", "name": "alpha"}),
                ),
                (
                    "t2",
                    "Agent",
                    json!({"prompt": "beta task", "name": "beta"}),
                ),
            ],
            StopReason::ToolUse,
            TokenDelta::default(),
        ),
        // The two subagents' own turns, matched to each by its prompt.
        common::llm_turn(
            Some("alpha task"),
            Some("alpha done"),
            &[],
            StopReason::EndTurn,
            TokenDelta::default(),
        ),
        common::llm_turn(
            Some("beta task"),
            Some("beta done"),
            &[],
            StopReason::EndTurn,
            TokenDelta::default(),
        ),
    ]
}

/// Collects the AwaitAgent harness execs from the journal, in order.
fn await_results(fixture: &common::Fixture) -> Vec<String> {
    fixture
        .records()
        .into_iter()
        .filter_map(|record| match record.entry {
            JournalEntry::ToolExec {
                owner,
                call,
                output,
                ..
            } if owner == "harness" && call.name == "AwaitAgent" => Some(output.content),
            _ => None,
        })
        .collect()
}

fn completed_set(fixture: &common::Fixture) -> BTreeSet<(String, String, bool)> {
    fixture
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
        .collect()
}

#[tokio::test]
async fn await_any_collects_both_subagents() {
    let fixture = common::Fixture::new();
    let mut llm = base_llm();
    // Two await-any collections, then exit. The first collection is keyed
    // by the spawn results ("runs in the background"); the next two are
    // keyed by the previous AwaitAgent result ("finished"). Both
    // "finished"-keyed turns match the same text, and the mock consumes the
    // earlier one first, so order is preserved without depending on which
    // subagent finished first.
    llm.push(common::llm_turn(
        Some("runs in the background"),
        Some("Collecting the first."),
        &[("a1", "AwaitAgent", json!({}))],
        StopReason::ToolUse,
        TokenDelta::default(),
    ));
    llm.push(common::llm_turn(
        Some("finished"),
        Some("Collecting the second."),
        &[("a2", "AwaitAgent", json!({}))],
        StopReason::ToolUse,
        TokenDelta::default(),
    ));
    llm.push(common::llm_turn(
        Some("finished"),
        Some("Both done."),
        &[("x", "Exit", json!({"message": "both collected"}))],
        StopReason::ToolUse,
        TokenDelta::default(),
    ));

    let script = common::shared(TestScript {
        name: "parallel_await_any".into(),
        llm,
        tools: vec![],
        frontend: vec![
            FrontendStep::SendPrompt {
                text: "do both".into(),
            },
            FrontendStep::ExpectShutdown {
                message_contains: Some("both collected".into()),
            },
        ],
        network: vec![],
    });

    let outcome = silo_harness::run(fixture.config(), fixture.options(script.clone()))
        .await
        .expect("session completes");
    assert_eq!(outcome.message.as_deref(), Some("both collected"));
    assert!(
        script.finished(),
        "remaining: {}",
        script.remaining_summary()
    );

    // Both subagents completed, each with its own result.
    assert_eq!(
        completed_set(&fixture),
        BTreeSet::from([
            ("agent-1".to_string(), "alpha done".to_string(), false),
            ("agent-2".to_string(), "beta done".to_string(), false),
        ])
    );

    // Both were delivered through AwaitAgent, each attributed to the right
    // agent id and name. Completion order is not fixed, so assert on the
    // set of (id, name, text) the two collections carried.
    let results = await_results(&fixture);
    assert_eq!(results.len(), 2, "results: {results:?}");
    let attributed: BTreeSet<bool> = results
        .iter()
        .map(|content| {
            (content.contains("agent-1")
                && content.contains("alpha")
                && content.contains("alpha done"))
                || (content.contains("agent-2")
                    && content.contains("beta")
                    && content.contains("beta done"))
        })
        .collect();
    assert_eq!(
        attributed,
        BTreeSet::from([true]),
        "each AwaitAgent result must name one subagent and its text: {results:?}"
    );
    // The two results are for different subagents.
    let collected_alpha = results.iter().any(|c| c.contains("alpha done"));
    let collected_beta = results.iter().any(|c| c.contains("beta done"));
    assert!(collected_alpha && collected_beta, "results: {results:?}");
}

#[tokio::test]
async fn await_specific_id_collects_that_subagent() {
    let fixture = common::Fixture::new();
    let mut llm = base_llm();
    // Collect beta first by its id (agent-2), then alpha (agent-1), then
    // exit.
    llm.push(common::llm_turn(
        Some("runs in the background"),
        Some("Collecting beta by id."),
        &[("a1", "AwaitAgent", json!({"agent": "agent-2"}))],
        StopReason::ToolUse,
        TokenDelta::default(),
    ));
    llm.push(common::llm_turn(
        Some("beta done"),
        Some("Collecting alpha by id."),
        &[("a2", "AwaitAgent", json!({"agent": "agent-1"}))],
        StopReason::ToolUse,
        TokenDelta::default(),
    ));
    llm.push(common::llm_turn(
        Some("alpha done"),
        Some("Both done."),
        &[("x", "Exit", json!({"message": "ids collected"}))],
        StopReason::ToolUse,
        TokenDelta::default(),
    ));

    let script = common::shared(TestScript {
        name: "parallel_await_specific".into(),
        llm,
        tools: vec![],
        frontend: vec![
            FrontendStep::SendPrompt {
                text: "do both".into(),
            },
            FrontendStep::ExpectShutdown {
                message_contains: Some("ids collected".into()),
            },
        ],
        network: vec![],
    });

    let outcome = silo_harness::run(fixture.config(), fixture.options(script.clone()))
        .await
        .expect("session completes");
    assert_eq!(outcome.message.as_deref(), Some("ids collected"));
    assert!(
        script.finished(),
        "remaining: {}",
        script.remaining_summary()
    );

    // The first AwaitAgent collected agent-2 (beta); the second collected
    // agent-1 (alpha).
    let results = await_results(&fixture);
    assert_eq!(results.len(), 2, "results: {results:?}");
    assert!(
        results[0].contains("agent-2") && results[0].contains("beta done"),
        "first result: {}",
        results[0]
    );
    assert!(
        results[1].contains("agent-1") && results[1].contains("alpha done"),
        "second result: {}",
        results[1]
    );
}
