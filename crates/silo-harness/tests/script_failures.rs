//! Scripted sessions are self-checking: a mock LLM or sandbox mismatch
//! ends the session as a script failure, and entries the session never
//! consumed fail it at the end. A fully consumed script reports no
//! failure.

mod common;

use std::time::Duration;

use serde_json::json;

use silo_core::conversation::{StopReason, TokenDelta};
use silo_core::journal::JournalEntry;
use silo_core::replay::{FrontendStep, ScriptedToolExec, TestScript};
use silo_core::tool::ToolOutput;

#[tokio::test]
async fn an_exhausted_llm_script_fails_the_session_at_once() {
    let fixture = common::Fixture::new();
    // The first turn requests a tool; the follow-up request after the tool
    // result finds the llm list exhausted.
    let script = common::shared(TestScript {
        name: "llm_short".into(),
        llm: vec![common::llm_turn(
            Some("go"),
            Some("Running the build."),
            &[("t1", "Bash", json!({"command": "make"}))],
            StopReason::ToolUse,
            TokenDelta::default(),
        )],
        tools: vec![ScriptedToolExec {
            expect_name: "Bash".into(),
            expect_input: Some(json!({"command": "make"})),
            output: ToolOutput::ok("built"),
        }],
        frontend: vec![FrontendStep::SendPrompt { text: "go".into() }],
        network: vec![],
    });

    let outcome = tokio::time::timeout(
        Duration::from_secs(60),
        silo_harness::run(fixture.config(), fixture.options(script.clone())),
    )
    .await
    .expect("the session ends instead of spinning")
    .expect("the session ends cleanly");

    let failure = outcome.script_failure.expect("script failure");
    assert!(
        failure.contains("llm script exhausted"),
        "failure: {failure}"
    );
    assert!(failure.contains("remaining: llm 1/1"), "failure: {failure}");
    assert!(outcome.llm_failure.is_none());
    assert!(outcome.message.is_none());

    // One served request plus the one that found the script exhausted; the
    // mismatch did not send the loop back to awaiting input.
    let llm_requests = fixture
        .records()
        .iter()
        .filter(|record| matches!(record.entry, JournalEntry::LlmRequest { .. }))
        .count();
    assert_eq!(llm_requests, 2);
    let kinds = common::event_kinds(&fixture.events());
    assert!(!kinds.contains(&"error"), "events: {kinds:?}");
    assert_eq!(
        kinds
            .iter()
            .filter(|kind| **kind == "awaiting_input")
            .count(),
        1,
        "events: {kinds:?}"
    );
}

#[tokio::test]
async fn a_trailing_llm_entry_the_session_never_reached_fails_the_session() {
    let fixture = common::Fixture::new();
    let script = common::shared(TestScript {
        name: "llm_trailing".into(),
        llm: vec![
            common::llm_turn(
                Some("go"),
                Some("Done."),
                &[],
                StopReason::EndTurn,
                TokenDelta::default(),
            ),
            common::llm_turn(
                None,
                Some("never reached"),
                &[],
                StopReason::EndTurn,
                TokenDelta::default(),
            ),
        ],
        frontend: vec![FrontendStep::SendPrompt { text: "go".into() }],
        ..TestScript::default()
    });

    let outcome = silo_harness::run(fixture.config(), fixture.options(script.clone()))
        .await
        .expect("the session ends cleanly");

    // The frontend script ran out as designed, but the trailing llm entry
    // makes the run a failure.
    assert_eq!(
        outcome.message.as_deref(),
        Some("frontend script exhausted")
    );
    let failure = outcome.script_failure.expect("script failure");
    assert!(
        failure.contains("script entries left unconsumed"),
        "failure: {failure}"
    );
    assert!(failure.contains("remaining: llm 1/2"), "failure: {failure}");
    assert!(!script.finished());
}

#[tokio::test]
async fn a_sandbox_tool_name_mismatch_fails_the_session() {
    let fixture = common::Fixture::new();
    let script = common::shared(TestScript {
        name: "tool_mismatch".into(),
        llm: vec![common::llm_turn(
            Some("go"),
            Some("Listing."),
            &[("t1", "Bash", json!({"command": "ls"}))],
            StopReason::ToolUse,
            TokenDelta::default(),
        )],
        tools: vec![ScriptedToolExec {
            expect_name: "Read".into(),
            expect_input: None,
            output: ToolOutput::ok(""),
        }],
        frontend: vec![FrontendStep::SendPrompt { text: "go".into() }],
        network: vec![],
    });

    let outcome = silo_harness::run(fixture.config(), fixture.options(script.clone()))
        .await
        .expect("the session ends cleanly");

    let failure = outcome.script_failure.expect("script failure");
    assert!(
        failure.contains("sandbox script mismatch"),
        "failure: {failure}"
    );
    // The mock matches tool executions by content, so an unmatched call
    // reports the names it could not be matched against.
    assert!(
        failure.contains("no unconsumed tool exec matches"),
        "failure: {failure}"
    );
    assert!(failure.contains("\"Bash\""), "failure: {failure}");
    assert!(failure.contains("remaining:"), "failure: {failure}");
    assert!(outcome.llm_failure.is_none());
    assert!(outcome.message.is_none());
}

#[tokio::test]
async fn a_fully_consumed_script_reports_no_failure() {
    let fixture = common::Fixture::new();
    let script = common::shared(TestScript {
        name: "fully_consumed".into(),
        llm: vec![
            common::llm_turn(
                Some("build it"),
                Some("Listing the sources."),
                &[("t1", "Bash", json!({"command": "ls"}))],
                StopReason::ToolUse,
                TokenDelta::default(),
            ),
            common::llm_turn(
                Some("src"),
                Some("Build complete."),
                &[("t2", "Exit", json!({"message": "done: built"}))],
                StopReason::ToolUse,
                TokenDelta::default(),
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

    assert!(
        outcome.script_failure.is_none(),
        "failure: {:?}",
        outcome.script_failure
    );
    assert_eq!(outcome.message.as_deref(), Some("done: built"));
    assert!(script.finished());
}

#[tokio::test]
async fn a_truncated_script_with_a_trailing_shutdown_step_still_fails() {
    // The llm list ends one turn short, and the frontend script still has a
    // trailing ExpectShutdown with a message filter. The mismatch suppresses
    // the final message, so the shutdown step cannot match; the session must
    // still report a script failure rather than a generic frontend error.
    let fixture = common::Fixture::new();
    let script = common::shared(TestScript {
        name: "truncated_with_shutdown".into(),
        llm: vec![common::llm_turn(
            Some("go"),
            Some("Running the build."),
            &[("t1", "Bash", json!({"command": "make"}))],
            StopReason::ToolUse,
            TokenDelta::default(),
        )],
        tools: vec![ScriptedToolExec {
            expect_name: "Bash".into(),
            expect_input: Some(json!({"command": "make"})),
            output: ToolOutput::ok("built"),
        }],
        frontend: vec![
            FrontendStep::SendPrompt { text: "go".into() },
            FrontendStep::ExpectShutdown {
                message_contains: Some("done".into()),
            },
        ],
        network: vec![],
    });

    let outcome = silo_harness::run(fixture.config(), fixture.options(script))
        .await
        .expect("session completes with a script failure, not a hard error");
    assert!(
        outcome.script_failure.is_some(),
        "expected a script failure, got {outcome:?}"
    );
}

#[tokio::test]
async fn a_diverged_frontend_step_fails_the_session() {
    // The frontend script answers a question that was never asked: when the
    // harness asks for input it finds an AnswerQuestion step, which does not
    // belong there. The cursor stays on it, so the script is unconsumed and
    // the session fails rather than exiting cleanly.
    let fixture = common::Fixture::new();
    let script = common::shared(TestScript {
        name: "diverged_frontend".into(),
        llm: vec![common::llm_turn(
            Some("go"),
            Some("Done."),
            &[("t1", "Exit", json!({"message": "done"}))],
            StopReason::ToolUse,
            TokenDelta::default(),
        )],
        tools: vec![],
        frontend: vec![
            FrontendStep::SendPrompt { text: "go".into() },
            FrontendStep::AnswerQuestion {
                contains: None,
                answer: "never asked".into(),
            },
        ],
        network: vec![],
    });

    let outcome = silo_harness::run(fixture.config(), fixture.options(script))
        .await
        .expect("session completes");
    assert!(
        outcome.script_failure.is_some(),
        "a diverged frontend script must fail, got {outcome:?}"
    );
}
