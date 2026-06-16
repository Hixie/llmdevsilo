//! Subagent flow: the top-level agent spawns a subagent through the Agent
//! tool (which returns at once with the subagent's id), then collects the
//! subagent's report with AwaitAgent, and the report feeds the next turn.

mod common;

use serde_json::json;

use silo_core::conversation::{StopReason, TokenDelta};
use silo_core::event::EventPayload;
use silo_core::journal::JournalEntry;
use silo_core::replay::{FrontendStep, TestScript};

#[tokio::test]
async fn spawn_then_await_feeds_the_parent_turn() {
    let fixture = common::Fixture::new();
    let script = common::shared(TestScript {
        name: "subagent_session".into(),
        llm: vec![
            // Top level: spawn the subagent. The Agent call returns at once.
            common::llm_turn(
                Some("do it"),
                Some("Delegating."),
                &[(
                    "t1",
                    "Agent",
                    json!({"prompt": "sub work", "name": "sub task"}),
                )],
                StopReason::ToolUse,
                TokenDelta::default(),
            ),
            // Top level again: the Agent result reports the subagent
            // started; now collect it with AwaitAgent.
            common::llm_turn(
                Some("runs in the background"),
                Some("Collecting."),
                &[("t2", "AwaitAgent", json!({}))],
                StopReason::ToolUse,
                TokenDelta::default(),
            ),
            // The subagent's own turn, seeded with the Agent prompt.
            common::llm_turn(
                Some("sub work"),
                Some("sub result"),
                &[],
                StopReason::EndTurn,
                TokenDelta::default(),
            ),
            // Back on the top level: the AwaitAgent result carries the
            // subagent's final text and its id/name.
            common::llm_turn(
                Some("sub result"),
                Some("Wrapping up."),
                &[("t3", "Exit", json!({"message": "subagent done"}))],
                StopReason::ToolUse,
                TokenDelta::default(),
            ),
        ],
        tools: vec![],
        frontend: vec![
            FrontendStep::SendPrompt {
                text: "do it".into(),
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
    assert_eq!(outcome.message.as_deref(), Some("subagent done"));
    assert!(
        script.finished(),
        "remaining: {}",
        script.remaining_summary()
    );

    let events = fixture.events();
    let spawned: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::AgentSpawned {
                parent,
                agent,
                name,
                prompt,
            } => Some((parent.clone(), agent.clone(), name.clone(), prompt.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        spawned,
        vec![(
            "agent-0".to_string(),
            "agent-1".to_string(),
            Some("sub task".to_string()),
            "sub work".to_string()
        )]
    );
    let completed: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::AgentCompleted {
                agent,
                result,
                is_error,
            } => Some((agent.clone(), result.clone(), *is_error)),
            _ => None,
        })
        .collect();
    assert_eq!(
        completed,
        vec![("agent-1".to_string(), "sub result".to_string(), false)]
    );

    // Both the Agent spawn and the AwaitAgent collection are journaled with
    // owner "harness". The Agent result reports the subagent started and
    // carries its id; the AwaitAgent result carries the subagent's id, name,
    // and final text.
    let harness_execs: Vec<_> = fixture
        .records()
        .into_iter()
        .filter_map(|record| match record.entry {
            JournalEntry::ToolExec {
                owner,
                call,
                output,
                ..
            } if owner == "harness" => Some((call.name, output.content)),
            _ => None,
        })
        .collect();
    assert_eq!(harness_execs.len(), 2, "execs: {harness_execs:?}");
    assert_eq!(harness_execs[0].0, "Agent");
    assert!(
        harness_execs[0].1.contains("agent-1") && harness_execs[0].1.contains("background"),
        "agent result: {}",
        harness_execs[0].1
    );
    assert_eq!(harness_execs[1].0, "AwaitAgent");
    assert!(
        harness_execs[1].1.contains("agent-1")
            && harness_execs[1].1.contains("sub task")
            && harness_execs[1].1.contains("sub result"),
        "await result: {}",
        harness_execs[1].1
    );
}
