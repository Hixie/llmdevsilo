//! Pieces shared by all LLM backends.

use std::future::Future;
use std::time::Duration;

use serde_json::json;
use silo_core::cost::Pricing;
use silo_core::error::LlmError;
use silo_core::tool::{ToolAvailability, ToolDef};

/// The Agent tool, contributed by the LLM backend layer per the design:
/// any agent can spawn a subagent that shares the same sandbox. The
/// harness executes this tool. The call returns immediately with the new
/// subagent's id; the subagent runs in the background and is collected
/// with [`await_agent_tool_def`].
pub fn agent_tool_def() -> ToolDef {
    ToolDef {
        name: "Agent".to_string(),
        description: "Launch a subagent to handle a self-contained task. The subagent runs \
                      in the same sandbox and workspace, receives the prompt as its task \
                      description, and works autonomously (it cannot ask the user questions). \
                      This call returns immediately with the subagent's id; the subagent runs \
                      in the background. Collect its final report with AwaitAgent. Launch \
                      several subagents, then await them, to run work in parallel."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "Complete, self-contained task description for the subagent."
                },
                "name": {
                    "type": "string",
                    "description": "Optional short display name for the subagent."
                }
            },
            "required": ["prompt"]
        }),
        availability: ToolAvailability::Both,
    }
}

/// The AwaitAgent tool, contributed by the LLM backend layer alongside
/// [`agent_tool_def`]. The harness executes it: it blocks until one of the
/// calling agent's subagents finishes and returns that subagent's report.
pub fn await_agent_tool_def() -> ToolDef {
    ToolDef {
        name: "AwaitAgent".to_string(),
        description: "Wait for a subagent launched with Agent to finish and collect its final \
                      report. With no input, waits for the first of your still-running \
                      subagents to finish. With an 'agent' id, waits for that specific \
                      subagent. Returns the subagent's id, display name, and final report (or \
                      its error output if it failed). Each subagent is collected once."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "agent": {
                    "type": "string",
                    "description": "Optional id of a specific subagent to wait for, as returned \
                                    by the Agent tool. Omit to wait for the first of your \
                                    running subagents to finish."
                }
            }
        }),
        availability: ToolAvailability::Both,
    }
}

/// Built-in price list, matched by substring against the model name.
/// Approximate and overridable via `LlmConfig::pricing`.
pub fn default_pricing_for_model(model: &str) -> Option<Pricing> {
    const TABLE: &[(&str, f64, f64)] = &[
        ("claude-opus", 15.0, 75.0),
        ("claude-sonnet", 3.0, 15.0),
        ("claude-haiku", 0.80, 4.0),
        ("gpt-4o-mini", 0.15, 0.60),
        ("gpt-4o", 2.50, 10.0),
        ("gpt-4.1-mini", 0.40, 1.60),
        ("gpt-4.1", 2.0, 8.0),
        ("gpt-5", 1.25, 10.0),
        ("o3", 2.0, 8.0),
    ];
    TABLE
        .iter()
        .find(|(needle, _, _)| model.contains(needle))
        .map(|(_, input, output)| Pricing {
            usd_per_million_input_tokens: *input,
            usd_per_million_output_tokens: *output,
        })
}

/// True for errors worth retrying: transport failures and rate limits or
/// server errors surfaced as `Api` with a 429/5xx prefix (backends format
/// such errors as "status <code>: ...").
pub fn is_retryable(error: &LlmError) -> bool {
    match error {
        LlmError::Transport(_) => true,
        LlmError::Api(message) => {
            message.starts_with("status 429")
                || message.starts_with("status 5")
                || message.contains("overloaded")
        }
        _ => false,
    }
}

/// Runs `attempt` up to `max_attempts` times with exponential backoff,
/// retrying only errors classified by [`is_retryable`].
pub async fn retry_with_backoff<T, F, Fut>(
    max_attempts: u32,
    base_delay: Duration,
    mut attempt: F,
) -> Result<T, LlmError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, LlmError>>,
{
    let mut delay = base_delay;
    let mut tries = 0;
    loop {
        tries += 1;
        match attempt().await {
            Ok(value) => return Ok(value),
            Err(error) if tries < max_attempts && is_retryable(&error) => {
                tokio::time::sleep(delay).await;
                delay *= 2;
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn pricing_table_matches_by_substring() {
        let pricing = default_pricing_for_model("claude-sonnet-4-6").unwrap();
        assert_eq!(pricing.usd_per_million_input_tokens, 3.0);
        assert!(default_pricing_for_model("unknown-model").is_none());
    }

    #[tokio::test]
    async fn retry_retries_transport_errors_only() {
        let calls = AtomicU32::new(0);
        let result: Result<u32, LlmError> = retry_with_backoff(3, Duration::from_millis(1), || {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            async move {
                if n < 2 {
                    Err(LlmError::Transport("reset".into()))
                } else {
                    Ok(42)
                }
            }
        })
        .await;
        assert_eq!(result.unwrap(), 42);
        assert_eq!(calls.load(Ordering::SeqCst), 3);

        let calls = AtomicU32::new(0);
        let result: Result<u32, LlmError> = retry_with_backoff(3, Duration::from_millis(1), || {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Err(LlmError::Config("bad".into())) }
        })
        .await;
        assert!(result.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
