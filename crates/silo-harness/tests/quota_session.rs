//! Quota enforcement: once the scripted usage exceeds the configured
//! quota, the next LLM request fails with a quota error, the harness
//! surfaces it as an Error event, and the session survives back to the
//! input loop.

mod common;

use serde_json::json;

use silo_core::config::HarnessConfig;
use silo_core::conversation::{StopReason, TokenDelta};
use silo_core::cost::{Pricing, QuotaConfig};
use silo_core::event::EventPayload;
use silo_core::journal::JournalEntry;
use silo_core::replay::{FrontendStep, ScriptedToolExec, TestScript};
use silo_core::tool::ToolOutput;

#[tokio::test]
async fn exceeded_quota_is_an_error_event_and_the_session_survives() {
    let fixture = common::Fixture::new();
    let mut config: HarnessConfig = fixture.config();
    config.llm.pricing = Some(Pricing {
        usd_per_million_input_tokens: 3.0,
        usd_per_million_output_tokens: 15.0,
    });
    config.llm.quota = QuotaConfig {
        max_total_tokens: Some(10),
        max_usd: None,
    };

    let script = common::shared(TestScript {
        name: "quota_session".into(),
        llm: vec![
            // This single turn consumes more than the whole quota. The
            // follow-up request after the tool result must never reach the
            // script.
            common::llm_turn(
                Some("go"),
                Some("Working."),
                &[("t1", "Bash", json!({"command": "make"}))],
                StopReason::ToolUse,
                TokenDelta {
                    input_tokens: 30,
                    output_tokens: 12,
                },
            ),
        ],
        tools: vec![ScriptedToolExec {
            expect_name: "Bash".into(),
            expect_input: Some(json!({"command": "make"})),
            output: ToolOutput::ok("built"),
        }],
        frontend: vec![FrontendStep::SendPrompt { text: "go".into() }],
        network: vec![],
    });

    let outcome = silo_harness::run(config, fixture.options(script.clone()))
        .await
        .expect("session survives the quota error");

    // The exhausted frontend script ends the session.
    assert_eq!(
        outcome.message.as_deref(),
        Some("frontend script exhausted")
    );
    assert!(
        script.finished(),
        "remaining: {}",
        script.remaining_summary()
    );

    let events = fixture.events();
    let errors: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::Error { message, .. } => Some(message.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        errors.len(),
        1,
        "events: {:?}",
        common::event_kinds(&events)
    );
    assert!(errors[0].contains("quota"), "error: {}", errors[0]);

    // Exactly one LLM response was served; the second request was refused
    // before reaching the script.
    let responses = fixture
        .records()
        .iter()
        .filter(|record| matches!(record.entry, JournalEntry::LlmResponse { .. }))
        .count();
    assert_eq!(responses, 1);

    // After the error the harness returned to awaiting input before the
    // shutdown.
    let kinds = common::event_kinds(&events);
    let error_pos = kinds
        .iter()
        .position(|k| *k == "error")
        .expect("error event");
    assert_eq!(
        &kinds[error_pos..],
        &["error", "awaiting_input", "shutdown"]
    );
}
