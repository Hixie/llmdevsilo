//! Headless quota exhaustion: the headless frontend answers every input
//! request immediately, so the session must end on the first LLM failure
//! instead of looping on the exhausted quota.

mod common;

use std::time::Duration;

use silo_core::config::{FrontendKind, HarnessConfig};
use silo_core::conversation::{StopReason, TokenDelta};
use silo_core::cost::QuotaConfig;
use silo_core::journal::JournalEntry;
use silo_core::replay::TestScript;

#[tokio::test]
async fn headless_session_ends_on_the_first_quota_failure() {
    let fixture = common::Fixture::new();
    let mut config: HarnessConfig = fixture.config();
    config.frontend.kind = FrontendKind::Headless;
    config.frontend.headless_prompt = Some("do the thing".into());
    config.llm.quota = QuotaConfig {
        max_total_tokens: Some(10),
        max_usd: None,
    };

    // One scripted turn whose usage exceeds the whole quota. The next
    // request must fail the quota check and end the session.
    let script = common::shared(TestScript {
        name: "headless_quota_session".into(),
        llm: vec![common::llm_turn(
            Some("do the thing"),
            Some("Starting."),
            &[],
            StopReason::EndTurn,
            TokenDelta {
                input_tokens: 30,
                output_tokens: 12,
            },
        )],
        ..TestScript::default()
    });

    let outcome = tokio::time::timeout(
        Duration::from_secs(60),
        silo_harness::run(config, fixture.options(script.clone())),
    )
    .await
    .expect("the session terminates instead of spinning")
    .expect("the session ends cleanly");

    let message = outcome.message.expect("outcome message");
    assert!(message.contains("quota"), "message: {message}");
    let failure = outcome
        .llm_failure
        .as_deref()
        .expect("the outcome records the LLM failure");
    assert!(failure.contains("quota"), "failure: {failure}");
    assert!(
        script.finished(),
        "remaining: {}",
        script.remaining_summary()
    );

    // One served request plus the one refused by the quota check.
    let llm_requests = fixture
        .records()
        .iter()
        .filter(|record| matches!(record.entry, JournalEntry::LlmRequest { .. }))
        .count();
    assert!(llm_requests <= 2, "{llm_requests} llm_request entries");
}
