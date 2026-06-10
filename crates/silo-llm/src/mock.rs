//! Scripted mock backend for tests and replays.
//!
//! Serves completions from the LLM portion of a shared
//! [`silo_core::replay::TestScript`]. Each call checks the quota, validates
//! the request against the scripted expectation, records the scripted usage
//! in a [`UsageMeter`], and returns the scripted response.

use std::sync::Arc;

use async_trait::async_trait;
use silo_core::config::LlmConfig;
use silo_core::conversation::{CompletionRequest, CompletionResponse, ContentBlock, Message};
use silo_core::cost::{QuotaConfig, UsageMeter, UsageSnapshot};
use silo_core::error::LlmError;
use silo_core::replay::SharedScript;
use silo_core::traits::LlmBackend;

/// Creates a mock backend that replays the LLM turns of `script`. Pricing
/// comes from the configuration; unset pricing meters dollars as zero.
pub fn create(config: &LlmConfig, script: SharedScript) -> Result<Arc<dyn LlmBackend>, LlmError> {
    Ok(Arc::new(MockBackend {
        script,
        meter: UsageMeter::new(config.pricing.unwrap_or_default(), config.quota),
    }))
}

struct MockBackend {
    script: SharedScript,
    meter: UsageMeter,
}

/// Copy of the request where tool-result content is mirrored as text
/// blocks, so a script's `expect_user_contains` can match tool results in
/// the latest user message. Used for script matching only; the response is
/// unaffected.
fn matchable(request: &CompletionRequest) -> CompletionRequest {
    let messages = request
        .messages
        .iter()
        .map(|message| Message {
            role: message.role,
            content: message
                .content
                .iter()
                .map(|block| match block {
                    ContentBlock::ToolResult { content, .. } => ContentBlock::Text {
                        text: content.clone(),
                    },
                    other => other.clone(),
                })
                .collect(),
        })
        .collect();
    CompletionRequest {
        system: request.system.clone(),
        messages,
        tools: request.tools.clone(),
        max_tokens: request.max_tokens,
    }
}

#[async_trait]
impl LlmBackend for MockBackend {
    fn id(&self) -> String {
        "mock".to_string()
    }

    async fn complete(&self, request: &CompletionRequest) -> Result<CompletionResponse, LlmError> {
        self.meter.check_quota()?;
        let response = self.script.next_llm(&matchable(request))?;
        self.meter.record(response.usage);
        Ok(response)
    }

    fn usage(&self) -> UsageSnapshot {
        self.meter.snapshot()
    }

    fn quota(&self) -> QuotaConfig {
        self.meter.quota()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use silo_core::conversation::{ContentBlock, Message, StopReason, TokenDelta};
    use silo_core::cost::Pricing;
    use silo_core::replay::{ScriptedLlmTurn, TestScript};

    fn turn(expect: Option<&str>, text: &str, usage: TokenDelta) -> ScriptedLlmTurn {
        ScriptedLlmTurn {
            expect_user_contains: expect.map(str::to_string),
            response: CompletionResponse {
                content: vec![ContentBlock::Text { text: text.into() }],
                stop_reason: StopReason::EndTurn,
                usage,
            },
        }
    }

    fn backend(config: &LlmConfig, turns: Vec<ScriptedLlmTurn>) -> Arc<dyn LlmBackend> {
        let script = SharedScript::new(TestScript {
            llm: turns,
            ..TestScript::default()
        });
        create(config, script).unwrap()
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
    async fn returns_scripted_response_and_records_usage() {
        let backend = backend(
            &LlmConfig::default(),
            vec![turn(
                Some("hello"),
                "hi there",
                TokenDelta {
                    input_tokens: 3,
                    output_tokens: 5,
                },
            )],
        );
        assert_eq!(backend.id(), "mock");
        let response = backend.complete(&request("hello world")).await.unwrap();
        assert_eq!(response.text(), "hi there");
        let usage = backend.usage();
        assert_eq!(usage.input_tokens, 3);
        assert_eq!(usage.output_tokens, 5);
    }

    #[tokio::test]
    async fn mismatch_is_an_error_and_usage_is_not_recorded() {
        let backend = backend(
            &LlmConfig::default(),
            vec![turn(
                Some("deploy"),
                "ok",
                TokenDelta {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            )],
        );
        let error = backend.complete(&request("unrelated")).await.unwrap_err();
        assert!(matches!(error, LlmError::ScriptMismatch(_)));
        assert_eq!(backend.usage().total_tokens(), 0);
    }

    #[tokio::test]
    async fn exhausted_script_is_a_mismatch() {
        let backend = backend(&LlmConfig::default(), vec![]);
        let error = backend.complete(&request("anything")).await.unwrap_err();
        assert!(matches!(error, LlmError::ScriptMismatch(_)));
    }

    #[tokio::test]
    async fn expectations_match_tool_result_content() {
        let backend = backend(
            &LlmConfig::default(),
            vec![turn(Some("file.txt"), "saw it", TokenDelta::default())],
        );
        let request = CompletionRequest {
            system: String::new(),
            messages: vec![Message {
                role: silo_core::conversation::Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "file.txt".into(),
                    is_error: false,
                }],
            }],
            tools: vec![],
            max_tokens: 64,
        };
        let response = backend.complete(&request).await.unwrap();
        assert_eq!(response.text(), "saw it");
    }

    #[tokio::test]
    async fn exhausted_quota_blocks_the_next_request_before_the_script() {
        let config = LlmConfig {
            quota: QuotaConfig {
                max_total_tokens: Some(10),
                max_usd: None,
            },
            ..LlmConfig::default()
        };
        let backend = backend(
            &config,
            vec![
                turn(
                    None,
                    "first",
                    TokenDelta {
                        input_tokens: 8,
                        output_tokens: 8,
                    },
                ),
                turn(None, "second", TokenDelta::default()),
            ],
        );
        backend.complete(&request("a")).await.unwrap();
        let error = backend.complete(&request("b")).await.unwrap_err();
        assert!(matches!(error, LlmError::QuotaExceeded(_)), "got {error:?}");
        // The scripted second turn is untouched: the quota check runs first.
        assert_eq!(backend.usage().total_tokens(), 16);
    }

    #[tokio::test]
    async fn pricing_and_quota_come_from_the_config() {
        let config = LlmConfig {
            pricing: Some(Pricing {
                usd_per_million_input_tokens: 2.0,
                usd_per_million_output_tokens: 4.0,
            }),
            quota: QuotaConfig {
                max_total_tokens: Some(1000),
                max_usd: Some(1.5),
            },
            ..LlmConfig::default()
        };
        let backend = backend(
            &config,
            vec![turn(
                None,
                "a",
                TokenDelta {
                    input_tokens: 500_000,
                    output_tokens: 250_000,
                },
            )],
        );
        backend.complete(&request("x")).await.unwrap();
        let usage = backend.usage();
        assert!((usage.usd - 2.0).abs() < 1e-9);
        assert_eq!(backend.quota(), config.quota);
    }
}
