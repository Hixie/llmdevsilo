//! User-interrupt flows: a question interrupted mid-turn (with a replay of
//! the recorded journal), an interrupt that cancels remaining tool calls,
//! and an interrupt arriving while the harness is idle.

mod common;

use serde_json::json;

use silo_core::conversation::{ContentBlock, Role, StopReason, TokenDelta};
use silo_core::event::EventPayload;
use silo_core::journal::JournalEntry;
use silo_core::replay::{script_from_journal, FrontendStep, TestScript};

fn question_interrupt_script() -> TestScript {
    TestScript {
        name: "interrupt_session".into(),
        llm: vec![
            common::llm_turn(
                Some("ask me"),
                None,
                &[(
                    "t1",
                    "AskUserQuestion",
                    json!({"question": "Which color should the theme use?"}),
                )],
                StopReason::ToolUse,
                TokenDelta::default(),
            ),
            common::llm_turn(
                Some("[interrupted by the user]"),
                Some("Resuming without the answer."),
                &[("t2", "Exit", json!({"message": "wrapped up"}))],
                StopReason::ToolUse,
                TokenDelta::default(),
            ),
        ],
        tools: vec![],
        frontend: vec![
            FrontendStep::SendPrompt {
                text: "ask me".into(),
            },
            FrontendStep::Interrupt,
            FrontendStep::SendPrompt {
                text: "carry on without it".into(),
            },
            FrontendStep::ExpectShutdown {
                message_contains: Some("wrapped up".into()),
            },
        ],
        network: vec![],
    }
}

fn payloads(events: &[silo_core::event::Event]) -> Vec<serde_json::Value> {
    events
        .iter()
        .map(|event| serde_json::to_value(&event.payload).expect("payload serializes"))
        .collect()
}

#[tokio::test]
async fn question_interrupt_unwinds_the_turn_and_replays() {
    let mut fixture = common::Fixture::new();
    let config = fixture.config();
    let script = common::shared(question_interrupt_script());

    let outcome = silo_harness::run(config.clone(), fixture.options(script.clone()))
        .await
        .expect("session completes");
    assert_eq!(outcome.message.as_deref(), Some("wrapped up"));
    assert!(
        script.finished(),
        "remaining: {}",
        script.remaining_summary()
    );

    let events = fixture.events();
    let kinds = common::event_kinds(&events);

    // Exactly one interrupted event, for the top-level agent, and no
    // turn_complete: the interrupt replaces it.
    let interrupted_agents: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::Interrupted { agent } => Some(agent.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(interrupted_agents, vec!["agent-0".to_string()]);
    assert!(!kinds.contains(&"turn_complete"));
    // The mock resolved the question itself; the harness emits no question
    // events for it.
    assert!(!kinds.contains(&"question_asked"));
    assert!(!kinds.contains(&"question_answered"));

    // Stream order: the question's tool_use precedes interrupted, the
    // harness returns to awaiting input, and the follow-up prompt comes
    // after that.
    let position = |kind: &str| {
        kinds
            .iter()
            .position(|k| *k == kind)
            .unwrap_or_else(|| panic!("no {kind} event"))
    };
    let tool_use_at = position("tool_use");
    let interrupted_at = position("interrupted");
    assert!(tool_use_at < interrupted_at);
    assert_eq!(kinds[interrupted_at + 1], "awaiting_input");
    let followup_at = events
        .iter()
        .position(|event| {
            matches!(
                &event.payload,
                EventPayload::UserPrompt { text, .. } if text == "carry on without it"
            )
        })
        .expect("follow-up prompt event");
    assert!(followup_at > interrupted_at + 1);
    common::assert_strictly_increasing_from_zero(&events);

    // Replay: the journal regenerates a script with the interrupt at the
    // same position, and the replayed session produces the same events.
    let records = fixture.records();
    let generated = script_from_journal(&records, "generated");
    assert_eq!(
        generated
            .frontend
            .iter()
            .filter(|step| matches!(step, FrontendStep::Interrupt))
            .count(),
        1
    );

    fixture.reset_journal();
    let replay = common::shared(generated);
    let replay_outcome = silo_harness::run(config, fixture.options(replay.clone()))
        .await
        .expect("replay completes");
    assert_eq!(replay_outcome.message.as_deref(), Some("wrapped up"));
    assert!(
        replay.finished(),
        "remaining: {}",
        replay.remaining_summary()
    );
    let replay_events = fixture.events();
    assert_eq!(kinds, common::event_kinds(&replay_events));
    assert_eq!(payloads(&events), payloads(&replay_events));
}

#[tokio::test]
async fn interrupt_cancels_the_remaining_tool_calls() {
    let fixture = common::Fixture::new();
    let script = common::shared(TestScript {
        name: "interrupt_cancels_tools".into(),
        llm: vec![
            common::llm_turn(
                Some("ask then run"),
                None,
                &[
                    (
                        "t1",
                        "AskUserQuestion",
                        json!({"question": "Run the build?"}),
                    ),
                    ("t2", "Bash", json!({"command": "make build"})),
                ],
                StopReason::ToolUse,
                TokenDelta::default(),
            ),
            common::llm_turn(
                Some("[interrupted by the user]"),
                Some("Stopping here."),
                &[("t3", "Exit", json!({"message": "stopped early"}))],
                StopReason::ToolUse,
                TokenDelta::default(),
            ),
        ],
        // The Bash call is cancelled by the interrupt and never executes.
        tools: vec![],
        frontend: vec![
            FrontendStep::SendPrompt {
                text: "ask then run".into(),
            },
            FrontendStep::Interrupt,
            FrontendStep::SendPrompt {
                text: "fine, stop".into(),
            },
            FrontendStep::ExpectShutdown {
                message_contains: Some("stopped early".into()),
            },
        ],
        network: vec![],
    });

    let outcome = silo_harness::run(fixture.config(), fixture.options(script.clone()))
        .await
        .expect("session completes");
    assert_eq!(outcome.message.as_deref(), Some("stopped early"));
    assert!(
        script.finished(),
        "remaining: {}",
        script.remaining_summary()
    );

    // Only the question and the Exit produced tool_use events; the
    // cancelled Bash call never started.
    let events = fixture.events();
    let tool_uses: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::ToolUse { call, .. } => Some(call.name.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(tool_uses, vec!["AskUserQuestion", "Exit"]);

    let records = fixture.records();
    assert!(records.iter().any(|record| matches!(
        &record.entry,
        JournalEntry::Lifecycle { message }
            if message.contains("interrupt cancelled 1 tool call(s)")
    )));

    // The second LLM request carries a tool result for both tool uses: the
    // question's recorded resolution and the synthetic result for the
    // cancelled Bash call.
    let second_request = records
        .iter()
        .filter_map(|record| match &record.entry {
            JournalEntry::LlmRequest { request, .. } => Some(request.clone()),
            _ => None,
        })
        .nth(1)
        .expect("two llm requests");
    let last_user = second_request
        .messages
        .iter()
        .rev()
        .find(|message| message.role == Role::User)
        .expect("a user message");
    for id in ["t1", "t2"] {
        let result = last_user
            .content
            .iter()
            .find_map(|block| match block {
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } if tool_use_id == id => Some((content.clone(), *is_error)),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no tool result for {id}"));
        assert_eq!(result, ("[interrupted by the user]".to_string(), true));
    }
}

#[tokio::test]
async fn idle_interrupt_is_inert() {
    let fixture = common::Fixture::new();
    let script = common::shared(TestScript {
        name: "idle_interrupt".into(),
        llm: vec![common::llm_turn(
            Some("go"),
            Some("Done."),
            &[("t1", "Exit", json!({"message": "ran to completion"}))],
            StopReason::ToolUse,
            TokenDelta::default(),
        )],
        tools: vec![],
        frontend: vec![
            FrontendStep::Interrupt,
            FrontendStep::SendPrompt { text: "go".into() },
            FrontendStep::ExpectShutdown {
                message_contains: Some("ran to completion".into()),
            },
        ],
        network: vec![],
    });

    let outcome = silo_harness::run(fixture.config(), fixture.options(script.clone()))
        .await
        .expect("session completes");
    assert_eq!(outcome.message.as_deref(), Some("ran to completion"));
    assert!(
        script.finished(),
        "remaining: {}",
        script.remaining_summary()
    );

    // The interrupt command was delivered and consumed without aborting
    // anything: no interrupted event, and the turn ran normally.
    let records = fixture.records();
    assert!(records.iter().any(|record| matches!(
        &record.entry,
        JournalEntry::FrontendCommand { command }
            if command == &json!({"command": "interrupt"})
    )));
    let kinds = common::event_kinds(&fixture.events());
    assert!(!kinds.contains(&"interrupted"));
    assert!(kinds.contains(&"assistant_text"));
}
