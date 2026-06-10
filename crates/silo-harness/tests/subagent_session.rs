//! Subagent flow: the top-level agent delegates work through the Agent
//! tool, and the subagent's final text comes back as the tool result.

mod common;

use serde_json::json;

use silo_core::conversation::{StopReason, TokenDelta};
use silo_core::event::EventPayload;
use silo_core::journal::JournalEntry;
use silo_core::replay::{FrontendStep, TestScript};

#[tokio::test]
async fn subagent_result_feeds_the_parent_turn() {
    let fixture = common::Fixture::new();
    let script = common::shared(TestScript {
        name: "subagent_session".into(),
        llm: vec![
            common::llm_turn(
                Some("do it"),
                Some("Delegating."),
                &[("t1", "Agent", json!({"prompt": "sub work"}))],
                StopReason::ToolUse,
                TokenDelta::default(),
            ),
            // Consumed by the subagent, seeded with the Agent prompt.
            common::llm_turn(
                Some("sub work"),
                Some("sub result"),
                &[],
                StopReason::EndTurn,
                TokenDelta::default(),
            ),
            // Back on the top level: the tool result carries the subagent
            // text.
            common::llm_turn(
                Some("sub result"),
                Some("Wrapping up."),
                &[("t2", "Exit", json!({"message": "subagent done"}))],
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
                prompt,
            } => Some((parent.clone(), agent.clone(), prompt.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        spawned,
        vec![(
            "agent-0".to_string(),
            "agent-1".to_string(),
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

    // The Agent tool execution is journaled with owner "harness".
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
    assert_eq!(
        harness_execs,
        vec![("Agent".to_string(), "sub result".to_string())]
    );
}
