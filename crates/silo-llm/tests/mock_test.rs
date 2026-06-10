//! Integration tests for the scripted mock backend.

use silo_core::config::{LlmBackendKind, LlmConfig};
use silo_core::conversation::{
    CompletionRequest, CompletionResponse, ContentBlock, Message, StopReason, TokenDelta,
};
use silo_core::cost::{Pricing, QuotaConfig};
use silo_core::error::LlmError;
use silo_core::replay::{ScriptedLlmTurn, SharedScript, TestScript};

fn turn(
    expect_user_contains: Option<&str>,
    text: &str,
    input: u64,
    output: u64,
) -> ScriptedLlmTurn {
    ScriptedLlmTurn {
        expect_user_contains: expect_user_contains.map(str::to_string),
        response: CompletionResponse {
            content: vec![ContentBlock::Text { text: text.into() }],
            stop_reason: StopReason::EndTurn,
            usage: TokenDelta {
                input_tokens: input,
                output_tokens: output,
            },
        },
    }
}

fn shared(turns: Vec<ScriptedLlmTurn>) -> SharedScript {
    SharedScript::new(TestScript {
        llm: turns,
        ..TestScript::default()
    })
}

fn request(text: &str) -> CompletionRequest {
    CompletionRequest {
        system: String::new(),
        messages: vec![Message::user_text(text)],
        tools: vec![],
        max_tokens: 64,
    }
}

#[tokio::test]
async fn scripted_turns_match_and_replay_in_order() {
    let script = shared(vec![
        turn(Some("first"), "one", 1, 1),
        turn(Some("second"), "two", 2, 2),
    ]);
    let backend = silo_llm::mock::create(&LlmConfig::default(), script.clone()).unwrap();
    assert_eq!(backend.id(), "mock");

    let first = backend
        .complete(&request("the first prompt"))
        .await
        .unwrap();
    assert_eq!(first.text(), "one");
    let second = backend
        .complete(&request("the second prompt"))
        .await
        .unwrap();
    assert_eq!(second.text(), "two");
    assert!(script.finished());
}

#[tokio::test]
async fn expectation_mismatch_is_a_script_mismatch_error() {
    let backend = silo_llm::mock::create(
        &LlmConfig::default(),
        shared(vec![turn(Some("deploy"), "ok", 1, 1)]),
    )
    .unwrap();

    let error = backend
        .complete(&request("something else"))
        .await
        .unwrap_err();
    match error {
        LlmError::ScriptMismatch(message) => assert!(message.contains("deploy")),
        other => panic!("expected ScriptMismatch, got {other:?}"),
    }
    assert_eq!(backend.usage().total_tokens(), 0);
}

#[tokio::test]
async fn exhausted_script_reports_mismatch() {
    let backend = silo_llm::mock::create(
        &LlmConfig::default(),
        shared(vec![turn(None, "only", 1, 1)]),
    )
    .unwrap();

    backend.complete(&request("first")).await.unwrap();
    let error = backend.complete(&request("second")).await.unwrap_err();
    assert!(matches!(error, LlmError::ScriptMismatch(_)));
}

#[tokio::test]
async fn usage_accumulates_with_configured_pricing() {
    let config = LlmConfig {
        pricing: Some(Pricing {
            usd_per_million_input_tokens: 2.0,
            usd_per_million_output_tokens: 4.0,
        }),
        quota: QuotaConfig {
            max_total_tokens: Some(10_000),
            max_usd: None,
        },
        ..LlmConfig::default()
    };
    let backend = silo_llm::mock::create(
        &config,
        shared(vec![turn(None, "a", 10, 20), turn(None, "b", 30, 40)]),
    )
    .unwrap();

    backend.complete(&request("x")).await.unwrap();
    backend.complete(&request("y")).await.unwrap();

    let usage = backend.usage();
    assert_eq!(usage.input_tokens, 40);
    assert_eq!(usage.output_tokens, 60);
    let expected_usd = (40.0 * 2.0 + 60.0 * 4.0) / 1_000_000.0;
    assert!((usage.usd - expected_usd).abs() < 1e-12);
    assert_eq!(backend.quota(), config.quota);
}

#[tokio::test]
async fn create_backend_dispatches_to_mock_and_requires_a_script() {
    let config = LlmConfig {
        backend: LlmBackendKind::Mock,
        ..LlmConfig::default()
    };
    let backend = silo_llm::create_backend(&config, Some(shared(vec![turn(None, "hi", 1, 1)])))
        .await
        .unwrap();
    assert_eq!(backend.id(), "mock");

    match silo_llm::create_backend(&config, None).await {
        Err(error) => assert!(matches!(error, LlmError::Config(_))),
        Ok(_) => panic!("expected create_backend to fail without a script"),
    }
}
