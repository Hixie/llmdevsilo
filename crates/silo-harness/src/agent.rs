//! The agent loop: drives one conversation (top level or subagent)
//! against the LLM backend and routes tool calls to their owners.

use std::sync::atomic::{AtomicU64, Ordering};

use futures::future::BoxFuture;
use tokio::sync::Semaphore;

use silo_core::conversation::{
    AgentId, AgentKind, CompletionRequest, CompletionResponse, ContentBlock, Message, Role,
    StopReason,
};
use silo_core::error::{FrontendError, HarnessError};
use silo_core::event::{EventBus, EventPayload};
use silo_core::journal::{JournalEntry, JournalHandle};
use silo_core::tool::{ToolCall, ToolOutput, ToolOwner, ToolRegistry};
use silo_core::traits::{Frontend, LlmBackend, Sandbox};

use crate::shutdown::ShutdownSignal;

/// Maximum subagent nesting depth. The top-level agent is depth 0.
const MAX_SUBAGENT_DEPTH: u32 = 3;

/// Maximum number of concurrently running subagents.
pub(crate) const MAX_CONCURRENT_SUBAGENTS: usize = 8;

/// Borrowed view of every component an agent loop needs. Shared by the
/// top-level agent and all subagents in a session.
pub(crate) struct SessionCtx<'a> {
    pub bus: &'a EventBus,
    pub journal: &'a JournalHandle,
    pub backend: &'a dyn LlmBackend,
    pub sandbox: &'a dyn Sandbox,
    pub frontend: &'a dyn Frontend,
    pub registry: &'a ToolRegistry,
    pub shutdown: &'a ShutdownSignal,
    pub system: &'a str,
    pub max_tokens: u32,
    /// Next subagent number; the top-level agent is `agent-0`.
    pub agent_counter: &'a AtomicU64,
    pub subagent_slots: &'a Semaphore,
}

/// How one conversation ended.
pub(crate) enum TurnOutcome {
    /// The model stopped without further tool calls. `final_text` is the
    /// concatenated text of the last response.
    Completed { final_text: String },
    /// A shutdown was requested mid-turn.
    ShutdownRequested,
    /// The LLM request failed; an Error event has been emitted. The message
    /// is the error's display form.
    LlmFailed(String),
}

enum ToolDisposition {
    Output {
        owner: &'static str,
        output: ToolOutput,
    },
    Shutdown,
}

/// Runs the conversation until the model stops calling tools, an LLM error
/// occurs, or a shutdown is requested. Tool calls within one response run
/// sequentially in order; the Agent tool recurses into this function for
/// the subagent and awaits it inline.
pub(crate) fn drive<'c>(
    ctx: &'c SessionCtx<'c>,
    agent: AgentId,
    kind: AgentKind,
    messages: &'c mut Vec<Message>,
    depth: u32,
) -> BoxFuture<'c, Result<TurnOutcome, HarnessError>> {
    Box::pin(async move {
        loop {
            if ctx.shutdown.check().await.is_some() {
                return Ok(TurnOutcome::ShutdownRequested);
            }
            let request = CompletionRequest {
                system: ctx.system.to_string(),
                messages: messages.clone(),
                tools: ctx.registry.defs_for(kind),
                max_tokens: ctx.max_tokens,
            };
            ctx.journal.append(JournalEntry::LlmRequest {
                agent: agent.clone(),
                backend: ctx.backend.id(),
                request: request.clone(),
            });
            let response = match ctx.backend.complete(&request).await {
                Ok(response) => response,
                Err(error) => {
                    let message = error.to_string();
                    ctx.journal.append(JournalEntry::Lifecycle {
                        message: format!("llm request failed for {agent}: {message}"),
                    });
                    ctx.bus.emit(EventPayload::Error {
                        context: format!("llm:{agent}"),
                        message: message.clone(),
                    });
                    return Ok(TurnOutcome::LlmFailed(message));
                }
            };
            ctx.journal.append(JournalEntry::LlmResponse {
                agent: agent.clone(),
                backend: ctx.backend.id(),
                response: response.clone(),
            });
            for block in &response.content {
                if let ContentBlock::Text { text } = block {
                    ctx.bus.emit(EventPayload::AssistantText {
                        agent: agent.clone(),
                        text: text.clone(),
                    });
                }
            }
            messages.push(Message::assistant(response.content.clone()));
            ctx.bus.emit(EventPayload::CostReport {
                backend: ctx.backend.id(),
                usage: ctx.backend.usage(),
                quota: ctx.backend.quota(),
            });

            let tool_calls: Vec<ToolCall> = response
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::ToolUse { id, name, input } => Some(ToolCall {
                        id: id.clone(),
                        name: name.clone(),
                        input: input.clone(),
                    }),
                    _ => None,
                })
                .collect();

            if tool_calls.is_empty() {
                return Ok(end_of_turn(ctx, &agent, kind, &response));
            }

            let mut result_blocks = Vec::with_capacity(tool_calls.len());
            for call in &tool_calls {
                if ctx.shutdown.check().await.is_some() {
                    return Ok(TurnOutcome::ShutdownRequested);
                }
                ctx.bus.emit(EventPayload::ToolUse {
                    agent: agent.clone(),
                    call: call.clone(),
                });
                let output = match execute_tool(ctx, &agent, call, depth).await? {
                    ToolDisposition::Output { owner, output } => {
                        ctx.journal.append(JournalEntry::ToolExec {
                            agent: agent.clone(),
                            owner: owner.to_string(),
                            call: call.clone(),
                            output: output.clone(),
                        });
                        ctx.bus.emit(EventPayload::ToolResult {
                            agent: agent.clone(),
                            tool_use_id: call.id.clone(),
                            tool_name: call.name.clone(),
                            output: output.clone(),
                        });
                        output
                    }
                    ToolDisposition::Shutdown => return Ok(TurnOutcome::ShutdownRequested),
                };
                result_blocks.push(ContentBlock::ToolResult {
                    tool_use_id: call.id.clone(),
                    content: output.content,
                    is_error: output.is_error,
                });
            }
            messages.push(Message {
                role: Role::User,
                content: result_blocks,
            });

            if response.stop_reason != StopReason::ToolUse {
                return Ok(end_of_turn(ctx, &agent, kind, &response));
            }
        }
    })
}

fn end_of_turn(
    ctx: &SessionCtx<'_>,
    agent: &AgentId,
    kind: AgentKind,
    response: &CompletionResponse,
) -> TurnOutcome {
    if response.stop_reason == StopReason::MaxTokens {
        ctx.journal.append(JournalEntry::Lifecycle {
            message: format!("{agent}: response hit max_tokens; treated as end of turn"),
        });
    }
    if kind == AgentKind::TopLevel {
        ctx.bus.emit(EventPayload::TurnComplete {
            agent: agent.clone(),
            stop_reason: response.stop_reason.clone(),
        });
    }
    TurnOutcome::Completed {
        final_text: response.text(),
    }
}

async fn execute_tool(
    ctx: &SessionCtx<'_>,
    agent: &AgentId,
    call: &ToolCall,
    depth: u32,
) -> Result<ToolDisposition, HarnessError> {
    match ctx.registry.owner_of(&call.name) {
        Some(ToolOwner::Sandbox) => match ctx.sandbox.run_tool(agent, call).await {
            Ok(output) => Ok(ToolDisposition::Output {
                owner: "sandbox",
                output,
            }),
            Err(error) => {
                ctx.bus.emit(EventPayload::Error {
                    context: format!("sandbox tool {}", call.name),
                    message: error.to_string(),
                });
                Err(error.into())
            }
        },
        Some(ToolOwner::Frontend) => run_frontend_tool(ctx, agent, call).await,
        Some(ToolOwner::Harness) => run_agent_tool(ctx, agent, call, depth).await,
        None => Ok(ToolDisposition::Output {
            owner: "harness",
            output: ToolOutput::error(format!("unknown tool: {}", call.name)),
        }),
    }
}

/// Runs a frontend-owned tool. For SendUserFile the file is first read
/// through the sandbox Read path and the content (base64) is injected into
/// the forwarded call input as `content_b64`.
async fn run_frontend_tool(
    ctx: &SessionCtx<'_>,
    agent: &AgentId,
    call: &ToolCall,
) -> Result<ToolDisposition, HarnessError> {
    let mut forwarded = call.clone();
    if call.name == "SendUserFile" {
        if let Some(path) = call.input.get("path").and_then(|value| value.as_str()) {
            let read_call = ToolCall {
                id: format!("{}-read", call.id),
                name: "Read".into(),
                input: serde_json::json!({ "path": path }),
            };
            let read_output = match ctx.sandbox.run_tool(agent, &read_call).await {
                Ok(output) => output,
                Err(error) => {
                    ctx.bus.emit(EventPayload::Error {
                        context: "sandbox tool Read (SendUserFile)".into(),
                        message: error.to_string(),
                    });
                    return Err(error.into());
                }
            };
            ctx.journal.append(JournalEntry::ToolExec {
                agent: agent.clone(),
                owner: "sandbox".to_string(),
                call: read_call,
                output: read_output.clone(),
            });
            if read_output.is_error {
                return Ok(ToolDisposition::Output {
                    owner: "frontend",
                    output: ToolOutput::error(format!(
                        "SendUserFile: cannot read {path}: {}",
                        read_output.content
                    )),
                });
            }
            let encoded = silo_core::helper::b64(read_output.content.as_bytes());
            if let serde_json::Value::Object(map) = &mut forwarded.input {
                map.insert("content_b64".into(), serde_json::Value::String(encoded));
            }
        }
    }
    match ctx.frontend.run_tool(agent, &forwarded).await {
        Ok(output) => Ok(ToolDisposition::Output {
            owner: "frontend",
            output,
        }),
        Err(error) => {
            if ctx.shutdown.check().await.is_some() {
                return Ok(ToolDisposition::Shutdown);
            }
            ctx.bus.emit(EventPayload::Error {
                context: format!("frontend tool {}", call.name),
                message: error.to_string(),
            });
            Err(error.into())
        }
    }
}

/// Runs the Agent tool: spawns a subagent conversation seeded with the
/// prompt and returns its final text as the tool output.
async fn run_agent_tool(
    ctx: &SessionCtx<'_>,
    agent: &AgentId,
    call: &ToolCall,
    depth: u32,
) -> Result<ToolDisposition, HarnessError> {
    let prompt = match call.input.get("prompt").and_then(|value| value.as_str()) {
        Some(prompt) if !prompt.is_empty() => prompt.to_string(),
        _ => {
            return Ok(ToolDisposition::Output {
                owner: "harness",
                output: ToolOutput::error("Agent requires a non-empty 'prompt' string"),
            })
        }
    };
    if depth >= MAX_SUBAGENT_DEPTH {
        return Ok(ToolDisposition::Output {
            owner: "harness",
            output: ToolOutput::error(format!(
                "subagent depth limit ({MAX_SUBAGENT_DEPTH}) reached"
            )),
        });
    }
    let permit = match ctx.subagent_slots.try_acquire() {
        Ok(permit) => permit,
        Err(_) => {
            return Ok(ToolDisposition::Output {
                owner: "harness",
                output: ToolOutput::error(format!(
                    "subagent concurrency limit ({MAX_CONCURRENT_SUBAGENTS}) reached"
                )),
            })
        }
    };
    let sub_id: AgentId = format!("agent-{}", ctx.agent_counter.fetch_add(1, Ordering::SeqCst));
    ctx.bus.emit(EventPayload::AgentSpawned {
        parent: agent.clone(),
        agent: sub_id.clone(),
        prompt: prompt.clone(),
    });
    let mut sub_messages = vec![Message::user_text(prompt)];
    let outcome = drive(
        ctx,
        sub_id.clone(),
        AgentKind::Subagent,
        &mut sub_messages,
        depth + 1,
    )
    .await?;
    drop(permit);
    match outcome {
        TurnOutcome::Completed { final_text } => {
            ctx.bus.emit(EventPayload::AgentCompleted {
                agent: sub_id,
                result: final_text.clone(),
                is_error: false,
            });
            Ok(ToolDisposition::Output {
                owner: "harness",
                output: ToolOutput::ok(final_text),
            })
        }
        TurnOutcome::LlmFailed(message) => {
            ctx.bus.emit(EventPayload::AgentCompleted {
                agent: sub_id,
                result: message.clone(),
                is_error: true,
            });
            Ok(ToolDisposition::Output {
                owner: "harness",
                output: ToolOutput::error(message),
            })
        }
        TurnOutcome::ShutdownRequested => {
            ctx.bus.emit(EventPayload::AgentCompleted {
                agent: sub_id,
                result: "session shutdown requested".into(),
                is_error: true,
            });
            Ok(ToolDisposition::Shutdown)
        }
    }
}

/// The top-level loop: emits AwaitingInput, waits for the next user input
/// (or a shutdown request), runs the turn, and repeats. Returns the final
/// shutdown message.
pub(crate) async fn top_level_loop(ctx: &SessionCtx<'_>) -> Result<Option<String>, HarnessError> {
    let agent = silo_core::conversation::top_level_agent_id();
    let mut messages: Vec<Message> = Vec::new();
    // The headless frontend answers every input request immediately, so a
    // persistently failing backend (e.g. an exhausted quota) would spin the
    // loop at CPU speed forever. Headless sessions end on the first LLM
    // failure; other frontends tolerate a bounded run of consecutive
    // failures (interactive sessions block on human input in between).
    const MAX_CONSECUTIVE_LLM_FAILURES: u32 = 8;
    let mut consecutive_llm_failures: u32 = 0;
    loop {
        if let Some(message) = ctx.shutdown.check().await {
            return Ok(message);
        }
        ctx.bus.emit(EventPayload::AwaitingInput);
        let input = tokio::select! {
            biased;
            message = ctx.shutdown.wait() => return Ok(message),
            input = ctx.frontend.next_user_input() => input,
        };
        match input {
            Ok(text) => {
                // The interactive frontend emits UserPrompt itself when a
                // client sends a prompt, so all clients see it immediately,
                // even mid-turn. For every other frontend kind the harness
                // emits UserPrompt when it consumes the input.
                if ctx.frontend.kind() != "interactive" {
                    ctx.bus.emit(EventPayload::UserPrompt {
                        client_id: None,
                        text: text.clone(),
                    });
                }
                messages.push(Message::user_text(text));
                match drive(ctx, agent.clone(), AgentKind::TopLevel, &mut messages, 0).await? {
                    TurnOutcome::Completed { .. } => {
                        consecutive_llm_failures = 0;
                        continue;
                    }
                    TurnOutcome::LlmFailed(message) => {
                        consecutive_llm_failures += 1;
                        let ended = if ctx.frontend.kind() == "headless" {
                            true
                        } else {
                            consecutive_llm_failures >= MAX_CONSECUTIVE_LLM_FAILURES
                        };
                        if ended {
                            ctx.journal.append(JournalEntry::Lifecycle {
                                message: format!(
                                    "session ended after {consecutive_llm_failures} \
                                     consecutive LLM failure(s): {message}"
                                ),
                            });
                            return Ok(Some(message));
                        }
                        continue;
                    }
                    TurnOutcome::ShutdownRequested => {
                        return Ok(ctx.shutdown.check().await.unwrap_or(None));
                    }
                }
            }
            Err(FrontendError::ScriptMismatch(detail)) => {
                // A scripted frontend with no further steps ends the
                // session.
                ctx.journal.append(JournalEntry::Lifecycle {
                    message: format!("frontend script exhausted: {detail}"),
                });
                return Ok(Some("frontend script exhausted".to_string()));
            }
            Err(error) => {
                ctx.bus.emit(EventPayload::Error {
                    context: "frontend input".into(),
                    message: error.to_string(),
                });
                return Err(error.into());
            }
        }
    }
}
