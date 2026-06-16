//! The agent loop: drives one conversation (top level or subagent)
//! against the LLM backend and routes tool calls to their owners.
//!
//! Subagents run asynchronously. The Agent tool spawns a subagent's
//! [`drive`] loop on a background task and returns at once with the new
//! agent's id; the AwaitAgent tool collects a finished subagent's report.
//! Each subagent is scoped to the turn of the agent that spawned it: when
//! that agent's `drive` returns, any subagents it never collected are
//! cancelled.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use tokio::sync::{mpsc, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinHandle;

use silo_core::conversation::{
    AgentId, AgentKind, CompletionRequest, CompletionResponse, ContentBlock, Message, Role,
    StopReason,
};
use silo_core::error::{FrontendError, HarnessError, LlmError, SandboxError};
use silo_core::event::{EventBus, EventPayload};
use silo_core::journal::{JournalEntry, JournalHandle};
use silo_core::tool::{ToolCall, ToolOutput, ToolOwner, ToolRegistry, INTERRUPTED_BY_USER};
use silo_core::traits::{Frontend, LlmBackend, Sandbox};

use crate::shutdown::{AbortReason, ShutdownSignal};

/// Maximum subagent nesting depth. The top-level agent is depth 0.
const MAX_SUBAGENT_DEPTH: u32 = 3;

/// Maximum number of concurrently running subagents.
pub(crate) const MAX_CONCURRENT_SUBAGENTS: usize = 8;

/// Result a subagent's background task reports back to the collecting
/// AwaitAgent call. The task emits its own `AgentCompleted` event (unless
/// the parent cancels it first), so this only carries what the parent's
/// tool result needs.
struct ChildResult {
    /// The subagent's final text, or its error/interrupt output.
    output: String,
    is_error: bool,
    /// True when the subagent ended because a shutdown was requested; the
    /// collecting AwaitAgent turns this into a Shutdown disposition.
    shutdown: bool,
    /// Set when the subagent ended on a mock script mismatch; the
    /// collecting AwaitAgent turns this into a ScriptFailed disposition so
    /// the session ends as a script failure, as in the single-agent flow.
    script_failure: Option<String>,
}

/// One spawned-but-not-collected subagent, held by its parent's pool.
struct ChildHandle {
    name: Option<AgentId>,
    join: JoinHandle<ChildResult>,
    /// Released (returning the slot to `subagent_slots`) when the child is
    /// collected or cancelled.
    _permit: OwnedSemaphorePermit,
    /// Swapped to true by whichever of the task or the cancellation path
    /// reaches the child's terminal `AgentCompleted` first, so exactly one
    /// such event is emitted per child.
    emitted: Arc<AtomicBool>,
}

/// The subagents one agent has spawned and not yet collected, plus a
/// channel each child task signals when it finishes (so an await-any call
/// can learn which child to reap).
struct ChildPool {
    children: HashMap<AgentId, ChildHandle>,
    done_tx: mpsc::UnboundedSender<AgentId>,
    /// The receiving half, parked here between waits. An await-any call
    /// takes it for the duration of its wait and returns it after.
    done_rx: Option<mpsc::UnboundedReceiver<AgentId>>,
}

impl ChildPool {
    fn new() -> ChildPool {
        let (done_tx, done_rx) = mpsc::unbounded_channel();
        ChildPool {
            children: HashMap::new(),
            done_tx,
            done_rx: Some(done_rx),
        }
    }

    fn done_rx_take(&mut self) -> mpsc::UnboundedReceiver<AgentId> {
        self.done_rx
            .take()
            .expect("the done receiver is taken by at most one await at a time")
    }

    fn done_rx_put(&mut self, rx: mpsc::UnboundedReceiver<AgentId>) {
        self.done_rx = Some(rx);
    }
}

/// Per-session registry of every agent's outstanding children, keyed by
/// parent agent id.
type ChildRegistry = Arc<Mutex<HashMap<AgentId, ChildPool>>>;

/// Shared, `'static` view of every component an agent loop needs. Cloned
/// (cheaply, all fields are `Arc`-backed) into each subagent's background
/// task. Shared by the top-level agent and all subagents in a session.
#[derive(Clone)]
pub(crate) struct SessionCtx {
    pub bus: EventBus,
    pub journal: JournalHandle,
    pub backend: Arc<dyn LlmBackend>,
    pub sandbox: Arc<dyn Sandbox>,
    pub frontend: Arc<dyn Frontend>,
    pub registry: Arc<ToolRegistry>,
    pub shutdown: ShutdownSignal,
    pub system: Arc<str>,
    pub max_tokens: u32,
    /// Next subagent number; the top-level agent is `agent-0`.
    pub agent_counter: Arc<AtomicU64>,
    pub subagent_slots: Arc<Semaphore>,
    /// Each agent's spawned-but-uncollected children.
    children: ChildRegistry,
}

impl SessionCtx {
    /// Builds the session context from the components the harness owns for
    /// the session.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        bus: EventBus,
        journal: JournalHandle,
        backend: Arc<dyn LlmBackend>,
        sandbox: Arc<dyn Sandbox>,
        frontend: Arc<dyn Frontend>,
        registry: Arc<ToolRegistry>,
        shutdown: ShutdownSignal,
        system: Arc<str>,
        max_tokens: u32,
    ) -> SessionCtx {
        SessionCtx {
            bus,
            journal,
            backend,
            sandbox,
            frontend,
            registry,
            shutdown,
            system,
            max_tokens,
            agent_counter: Arc::new(AtomicU64::new(1)),
            subagent_slots: Arc::new(Semaphore::new(MAX_CONCURRENT_SUBAGENTS)),
            children: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

/// How one conversation ended.
pub(crate) enum TurnOutcome {
    /// The model stopped without further tool calls. `final_text` is the
    /// concatenated text of the last response.
    Completed { final_text: String },
    /// A shutdown was requested mid-turn.
    ShutdownRequested,
    /// The user interrupted the turn. The conversation already carries a
    /// tool result for every tool use of its last assistant message.
    Interrupted,
    /// The LLM request failed; an Error event has been emitted. The message
    /// is the error's display form.
    LlmFailed(String),
    /// A mock component reported a script mismatch. The session ends
    /// immediately; the message is the mismatch detail.
    ScriptFailed(String),
}

enum ToolDisposition {
    Output {
        owner: &'static str,
        output: ToolOutput,
    },
    Shutdown,
    /// The tool call was ended by an interrupt without producing a result;
    /// the caller records a synthetic interrupted result for it.
    Interrupted,
    /// The tool call hit a script mismatch; the session ends.
    ScriptFailed(String),
}

/// Runs the conversation until the model stops calling tools, an LLM error
/// occurs, a shutdown is requested, or the user interrupts. Tool calls
/// within one response run sequentially in order; the Agent tool spawns a
/// subagent on a background task and returns at once, and AwaitAgent
/// collects a finished subagent.
///
/// When this loop is about to return, any subagents this agent spawned and
/// never collected are cancelled, so no child outlives its parent's turn.
///
/// `turn_generation` is the interrupt generation snapshotted when the
/// top-level turn started; an interrupt applies to this turn when the
/// generation grows past it. Interrupt checkpoints: before each LLM call,
/// during each LLM call (the in-flight completion is dropped and the
/// conversation keeps no trace of it), and before each tool call (the
/// remaining tool uses get synthetic interrupted results).
pub(crate) fn drive<'c>(
    ctx: &'c SessionCtx,
    agent: AgentId,
    kind: AgentKind,
    messages: &'c mut Vec<Message>,
    depth: u32,
    turn_generation: u64,
) -> BoxFuture<'c, Result<TurnOutcome, HarnessError>> {
    Box::pin(async move {
        let outcome = drive_inner(ctx, &agent, kind, messages, depth, turn_generation).await;
        // No child may outlive its parent's turn: cancel any this agent
        // spawned and never collected, however the turn ended.
        cancel_uncollected_children(ctx, agent.clone()).await;
        outcome
    })
}

/// The conversation loop proper. [`drive`] wraps it to cancel uncollected
/// children whichever way the loop returns.
async fn drive_inner(
    ctx: &SessionCtx,
    agent: &AgentId,
    kind: AgentKind,
    messages: &mut Vec<Message>,
    depth: u32,
    turn_generation: u64,
) -> Result<TurnOutcome, HarnessError> {
    {
        loop {
            if ctx.shutdown.check().await.is_some() {
                return Ok(TurnOutcome::ShutdownRequested);
            }
            if ctx.shutdown.interrupted_since(turn_generation) {
                return Ok(TurnOutcome::Interrupted);
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
            let completion = tokio::select! {
                biased;
                abort = ctx.shutdown.wait_abort(turn_generation) => {
                    return Ok(match abort {
                        AbortReason::Shutdown(_) => TurnOutcome::ShutdownRequested,
                        AbortReason::Interrupted => {
                            ctx.journal.append(JournalEntry::Lifecycle {
                                message: format!("{agent}: llm call aborted by interrupt"),
                            });
                            TurnOutcome::Interrupted
                        }
                    });
                }
                completion = ctx.backend.complete(&request) => completion,
            };
            let response = match completion {
                Ok(response) => response,
                Err(error @ LlmError::ScriptMismatch(_)) => {
                    // A script mismatch is a failure of the test script, not
                    // of the backend: the session ends at once, with no
                    // Error event and no failure counting.
                    let message = error.to_string();
                    ctx.journal.append(JournalEntry::Lifecycle {
                        message: format!("llm script mismatch for {agent}: {message}"),
                    });
                    return Ok(TurnOutcome::ScriptFailed(message));
                }
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
                return Ok(end_of_turn(ctx, agent, kind, &response));
            }

            let mut result_blocks = Vec::with_capacity(tool_calls.len());
            let mut interrupted = false;
            for call in &tool_calls {
                if ctx.shutdown.check().await.is_some() {
                    return Ok(TurnOutcome::ShutdownRequested);
                }
                if ctx.shutdown.interrupted_since(turn_generation) {
                    interrupted = true;
                    break;
                }
                ctx.bus.emit(EventPayload::ToolUse {
                    agent: agent.clone(),
                    call: call.clone(),
                });
                match execute_tool(ctx, agent, call, depth, turn_generation).await? {
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
                        result_blocks.push(ContentBlock::ToolResult {
                            tool_use_id: call.id.clone(),
                            content: output.content,
                            is_error: output.is_error,
                        });
                    }
                    ToolDisposition::Shutdown => return Ok(TurnOutcome::ShutdownRequested),
                    ToolDisposition::Interrupted => {
                        interrupted = true;
                        break;
                    }
                    ToolDisposition::ScriptFailed(message) => {
                        return Ok(TurnOutcome::ScriptFailed(message))
                    }
                }
            }
            if interrupted {
                // Cancelled tool calls were never executed, so they get no
                // ToolExec journal entry and no ToolResult event; the
                // synthetic results below keep the conversation well-formed
                // for the next LLM request.
                let cancelled = tool_calls.len() - result_blocks.len();
                ctx.journal.append(JournalEntry::Lifecycle {
                    message: format!("{agent}: interrupt cancelled {cancelled} tool call(s)"),
                });
                for call in &tool_calls[result_blocks.len()..] {
                    result_blocks.push(ContentBlock::ToolResult {
                        tool_use_id: call.id.clone(),
                        content: INTERRUPTED_BY_USER.into(),
                        is_error: true,
                    });
                }
                messages.push(Message {
                    role: Role::User,
                    content: result_blocks,
                });
                return Ok(TurnOutcome::Interrupted);
            }
            messages.push(Message {
                role: Role::User,
                content: result_blocks,
            });

            if response.stop_reason != StopReason::ToolUse {
                return Ok(end_of_turn(ctx, agent, kind, &response));
            }
        }
    }
}

fn end_of_turn(
    ctx: &SessionCtx,
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
    ctx: &SessionCtx,
    agent: &AgentId,
    call: &ToolCall,
    depth: u32,
    turn_generation: u64,
) -> Result<ToolDisposition, HarnessError> {
    match ctx.registry.owner_of(&call.name) {
        Some(ToolOwner::Sandbox) => run_sandbox_tool(ctx, agent, call, turn_generation).await,
        Some(ToolOwner::Frontend) => run_frontend_tool(ctx, agent, call, turn_generation).await,
        Some(ToolOwner::Harness) if call.name == "AwaitAgent" => {
            run_await_agent_tool(ctx, agent, call, turn_generation).await
        }
        Some(ToolOwner::Harness) => Ok(run_agent_tool(ctx, agent, call, depth)),
        None => Ok(ToolDisposition::Output {
            owner: "harness",
            output: ToolOutput::error(format!("unknown tool: {}", call.name)),
        }),
    }
}

/// Runs a sandbox tool against the abort signal. A shutdown drops the
/// in-flight execution. An interrupt cancels it through
/// `Sandbox::interrupt` and then awaits the original future — the helper
/// answers promptly once the child process dies — so the partial
/// stdout/stderr becomes the tool result and stays in the conversation;
/// the turn is marked interrupted at the loop's next checkpoint.
async fn run_sandbox_tool(
    ctx: &SessionCtx,
    agent: &AgentId,
    call: &ToolCall,
    turn_generation: u64,
) -> Result<ToolDisposition, HarnessError> {
    let result = {
        let tool_future = ctx.sandbox.run_tool(agent, call);
        tokio::pin!(tool_future);
        tokio::select! {
            biased;
            result = &mut tool_future => result,
            abort = ctx.shutdown.wait_abort(turn_generation) => match abort {
                AbortReason::Shutdown(_) => return Ok(ToolDisposition::Shutdown),
                AbortReason::Interrupted => {
                    if let Err(error) = ctx.sandbox.interrupt().await {
                        ctx.bus.emit(EventPayload::Error {
                            context: "sandbox interrupt".into(),
                            message: error.to_string(),
                        });
                    }
                    tool_future.await
                }
            },
        }
    };
    match result {
        Ok(output) => Ok(ToolDisposition::Output {
            owner: "sandbox",
            output,
        }),
        Err(error @ SandboxError::ScriptMismatch(_)) => {
            // A script mismatch ends the session as a script failure, not
            // as a harness error.
            let message = error.to_string();
            ctx.journal.append(JournalEntry::Lifecycle {
                message: format!("sandbox script mismatch for {agent}: {message}"),
            });
            Ok(ToolDisposition::ScriptFailed(message))
        }
        Err(error) => {
            ctx.bus.emit(EventPayload::Error {
                context: format!("sandbox tool {}", call.name),
                message: error.to_string(),
            });
            Err(error.into())
        }
    }
}

/// Runs a frontend-owned tool. For SendUserFile the file is first read
/// through the sandbox Read path and the content (base64) is injected into
/// the forwarded call input as `content_b64`.
///
/// The tool runs against the abort signal: a shutdown drops it, and an
/// interrupt cancels the frontend's pending interaction
/// (`Frontend::interrupt`) and then records the resolved output as the tool
/// result.
async fn run_frontend_tool(
    ctx: &SessionCtx,
    agent: &AgentId,
    call: &ToolCall,
    turn_generation: u64,
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
                Err(error @ SandboxError::ScriptMismatch(_)) => {
                    let message = error.to_string();
                    ctx.journal.append(JournalEntry::Lifecycle {
                        message: format!("sandbox script mismatch for {agent}: {message}"),
                    });
                    return Ok(ToolDisposition::ScriptFailed(message));
                }
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
    let result = {
        let tool_future = ctx.frontend.run_tool(agent, &forwarded);
        tokio::pin!(tool_future);
        // The tool future is polled first so the frontend registers the
        // interaction (the pending question) before any abort handling.
        tokio::select! {
            biased;
            result = &mut tool_future => result,
            abort = ctx.shutdown.wait_abort(turn_generation) => match abort {
                AbortReason::Shutdown(_) => return Ok(ToolDisposition::Shutdown),
                AbortReason::Interrupted => {
                    if let Err(error) = ctx.frontend.interrupt().await {
                        ctx.bus.emit(EventPayload::Error {
                            context: "frontend interrupt".into(),
                            message: error.to_string(),
                        });
                    }
                    tool_future.await
                }
            },
        }
    };
    match result {
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
/// prompt on a background task and returns at once with the subagent's id.
/// The subagent runs until it finishes; AwaitAgent collects it.
fn run_agent_tool(
    ctx: &SessionCtx,
    agent: &AgentId,
    call: &ToolCall,
    depth: u32,
) -> ToolDisposition {
    let prompt = match call.input.get("prompt").and_then(|value| value.as_str()) {
        Some(prompt) if !prompt.is_empty() => prompt.to_string(),
        _ => {
            return harness_error("Agent requires a non-empty 'prompt' string");
        }
    };
    if depth >= MAX_SUBAGENT_DEPTH {
        return harness_error(format!(
            "subagent depth limit ({MAX_SUBAGENT_DEPTH}) reached"
        ));
    }
    // One session-wide pool of live children: an owned permit moves into the
    // child handle and returns the slot when the child is collected or
    // cancelled.
    let permit = match ctx.subagent_slots.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            return harness_error(format!(
                "subagent concurrency limit ({MAX_CONCURRENT_SUBAGENTS}) reached"
            ));
        }
    };
    let name = call
        .input
        .get("name")
        .and_then(|value| value.as_str())
        .filter(|name| !name.is_empty())
        .map(str::to_string);
    let sub_id: AgentId = format!("agent-{}", ctx.agent_counter.fetch_add(1, Ordering::SeqCst));
    ctx.bus.emit(EventPayload::AgentSpawned {
        parent: agent.clone(),
        agent: sub_id.clone(),
        name: name.clone(),
        prompt: prompt.clone(),
    });

    // The child's turn generation is the current one, so an interrupt or
    // shutdown requested after the spawn unblocks the child's own `drive`
    // checks.
    let turn_generation = ctx.shutdown.interrupt_generation();
    let emitted = Arc::new(AtomicBool::new(false));
    let done_tx = {
        let mut registry = ctx.children.lock().expect("child registry poisoned");
        registry
            .entry(agent.clone())
            .or_insert_with(ChildPool::new)
            .done_tx
            .clone()
    };
    let task_ctx = ctx.clone();
    let task_id = sub_id.clone();
    let task_emitted = emitted.clone();
    let join = tokio::spawn(async move {
        let mut sub_messages = vec![Message::user_text(prompt)];
        let outcome = drive(
            &task_ctx,
            task_id.clone(),
            AgentKind::Subagent,
            &mut sub_messages,
            depth + 1,
            turn_generation,
        )
        .await;
        let result = child_result_of(outcome);
        // Whoever wins the swap emits the terminal AgentCompleted; if the
        // parent cancelled this child first it has already emitted, so we
        // skip.
        if !task_emitted.swap(true, Ordering::SeqCst) {
            task_ctx.bus.emit(EventPayload::AgentCompleted {
                agent: task_id.clone(),
                result: result.output.clone(),
                is_error: result.is_error,
            });
        }
        let _ = done_tx.send(task_id);
        result
    });

    {
        let mut registry = ctx.children.lock().expect("child registry poisoned");
        let pool = registry.entry(agent.clone()).or_insert_with(ChildPool::new);
        pool.children.insert(
            sub_id.clone(),
            ChildHandle {
                name: name.clone(),
                join,
                _permit: permit,
                emitted,
            },
        );
    }

    let label = name.as_deref().unwrap_or(&sub_id);
    harness_ok(format!(
        "Started subagent '{label}' ({sub_id}). It runs in the background; \
         collect it with AwaitAgent. Pass agent {sub_id:?} to wait for this one."
    ))
}

/// Maps a finished subagent's turn outcome (or a harness error from its
/// task) into the [`ChildResult`] the collecting AwaitAgent formats.
fn child_result_of(outcome: Result<TurnOutcome, HarnessError>) -> ChildResult {
    match outcome {
        Ok(TurnOutcome::Completed { final_text }) => ChildResult {
            output: final_text,
            is_error: false,
            shutdown: false,
            script_failure: None,
        },
        Ok(TurnOutcome::LlmFailed(message)) => ChildResult {
            output: message,
            is_error: true,
            shutdown: false,
            script_failure: None,
        },
        Ok(TurnOutcome::ScriptFailed(message)) => ChildResult {
            output: message.clone(),
            is_error: true,
            shutdown: false,
            script_failure: Some(message),
        },
        Ok(TurnOutcome::Interrupted) => ChildResult {
            output: "interrupted by the user".into(),
            is_error: true,
            shutdown: false,
            script_failure: None,
        },
        Ok(TurnOutcome::ShutdownRequested) => ChildResult {
            output: "session shutdown requested".into(),
            is_error: true,
            shutdown: true,
            script_failure: None,
        },
        Err(error) => ChildResult {
            output: error.to_string(),
            is_error: true,
            shutdown: false,
            script_failure: None,
        },
    }
}

/// Runs the AwaitAgent tool: blocks until one of the calling agent's
/// subagents finishes (a specific one when `agent` is given, otherwise the
/// first to finish), then returns its report. Selects against the abort
/// signal so an interrupt or shutdown unblocks the wait.
async fn run_await_agent_tool(
    ctx: &SessionCtx,
    agent: &AgentId,
    call: &ToolCall,
    turn_generation: u64,
) -> Result<ToolDisposition, HarnessError> {
    let requested = call
        .input
        .get("agent")
        .and_then(|value| value.as_str())
        .filter(|id| !id.is_empty())
        .map(str::to_string);

    // Resolve which child to collect, draining the completion channel for
    // the await-any case. Returns the child id, or an error tool result.
    let child_id = match requested {
        Some(id) => {
            let known = {
                let registry = ctx.children.lock().expect("child registry poisoned");
                registry
                    .get(agent)
                    .is_some_and(|pool| pool.children.contains_key(&id))
            };
            if !known {
                return Ok(harness_error(format!(
                    "AwaitAgent: {id:?} is not an outstanding subagent of this agent"
                )));
            }
            id
        }
        None => {
            let has_children = {
                let registry = ctx.children.lock().expect("child registry poisoned");
                registry
                    .get(agent)
                    .is_some_and(|pool| !pool.children.is_empty())
            };
            if !has_children {
                return Ok(harness_error("AwaitAgent: no subagents are running"));
            }
            // Wait for the first child to signal completion. The wait races
            // against the abort signal.
            match await_any_child(ctx, agent, turn_generation).await {
                AwaitAny::Child(id) => id,
                AwaitAny::Shutdown => return Ok(ToolDisposition::Shutdown),
                AwaitAny::Interrupted => return Ok(ToolDisposition::Interrupted),
            }
        }
    };

    // Collect the chosen child: for a specific id we still race the join
    // against the abort signal so an interrupt or shutdown unblocks us.
    let handle = {
        let mut registry = ctx.children.lock().expect("child registry poisoned");
        registry
            .get_mut(agent)
            .and_then(|pool| pool.children.remove(&child_id))
    };
    let Some(handle) = handle else {
        return Ok(harness_error(format!(
            "AwaitAgent: {child_id:?} was already collected"
        )));
    };
    let ChildHandle {
        name,
        mut join,
        _permit,
        emitted,
    } = handle;
    // Dropping the permit (after the join) returns the slot.
    let result = tokio::select! {
        biased;
        abort = ctx.shutdown.wait_abort(turn_generation) => {
            // Re-park the handle so the turn-end cancellation reaps it.
            reinsert_child(
                ctx,
                agent,
                child_id.clone(),
                ChildHandle {
                    name: name.clone(),
                    join,
                    _permit,
                    emitted,
                },
            );
            return Ok(match abort {
                AbortReason::Shutdown(_) => ToolDisposition::Shutdown,
                AbortReason::Interrupted => ToolDisposition::Interrupted,
            });
        }
        joined = &mut join => {
            drop(_permit);
            joined
        }
    };
    let result = match result {
        Ok(result) => result,
        Err(join_error) => {
            return Ok(harness_error(format!(
                "AwaitAgent: subagent {child_id} task failed: {join_error}"
            )));
        }
    };

    if result.shutdown {
        return Ok(ToolDisposition::Shutdown);
    }
    if let Some(detail) = result.script_failure {
        return Ok(ToolDisposition::ScriptFailed(detail));
    }
    let label = name.as_deref().unwrap_or(&child_id);
    let heading = format!("Subagent '{label}' ({child_id}) finished.\n");
    let body = format!("{heading}{}", result.output);
    Ok(ToolDisposition::Output {
        owner: "harness",
        output: ToolOutput {
            content: body,
            is_error: result.is_error,
        },
    })
}

/// Outcome of waiting for the first of an agent's children to finish.
enum AwaitAny {
    Child(AgentId),
    Shutdown,
    Interrupted,
}

/// Waits for the first of `agent`'s children to signal completion on the
/// pool's done channel, racing the abort signal. The done receiver lives in
/// the registry; it is taken out for the wait and replaced afterwards so a
/// concurrent await on the same agent (not expected, agents are
/// single-threaded loops) does not lose it.
async fn await_any_child(ctx: &SessionCtx, agent: &AgentId, turn_generation: u64) -> AwaitAny {
    loop {
        // Drain any already-finished children first.
        if let Some(id) = take_finished_child(ctx, agent) {
            return AwaitAny::Child(id);
        }
        let mut done_rx = {
            let mut registry = ctx.children.lock().expect("child registry poisoned");
            match registry.get_mut(agent) {
                Some(pool) => pool.done_rx_take(),
                None => return AwaitAny::Interrupted,
            }
        };
        let signalled = tokio::select! {
            biased;
            abort = ctx.shutdown.wait_abort(turn_generation) => {
                put_done_rx(ctx, agent, done_rx);
                return match abort {
                    AbortReason::Shutdown(_) => AwaitAny::Shutdown,
                    AbortReason::Interrupted => AwaitAny::Interrupted,
                };
            }
            received = done_rx.recv() => received,
        };
        put_done_rx(ctx, agent, done_rx);
        match signalled {
            Some(id) => {
                // The signalled child may already have been collected by a
                // specific-id await; only return it if it is still parked.
                let still_outstanding = {
                    let registry = ctx.children.lock().expect("child registry poisoned");
                    registry
                        .get(agent)
                        .is_some_and(|pool| pool.children.contains_key(&id))
                };
                if still_outstanding {
                    return AwaitAny::Child(id);
                }
            }
            None => return AwaitAny::Interrupted,
        }
    }
}

/// Returns the id of one of `agent`'s children whose task has already
/// finished, if any.
fn take_finished_child(ctx: &SessionCtx, agent: &AgentId) -> Option<AgentId> {
    let registry = ctx.children.lock().expect("child registry poisoned");
    registry.get(agent).and_then(|pool| {
        pool.children
            .iter()
            .find(|(_, handle)| handle.join.is_finished())
            .map(|(id, _)| id.clone())
    })
}

fn put_done_rx(ctx: &SessionCtx, agent: &AgentId, rx: mpsc::UnboundedReceiver<AgentId>) {
    let mut registry = ctx.children.lock().expect("child registry poisoned");
    if let Some(pool) = registry.get_mut(agent) {
        pool.done_rx_put(rx);
    }
}

fn reinsert_child(ctx: &SessionCtx, agent: &AgentId, id: AgentId, handle: ChildHandle) {
    let mut registry = ctx.children.lock().expect("child registry poisoned");
    registry
        .entry(agent.clone())
        .or_insert_with(ChildPool::new)
        .children
        .insert(id, handle);
}

/// Cancels every subagent the agent spawned and never collected, and every
/// descendant of those. Called as the agent's `drive` returns, so no child
/// outlives its parent's turn. Each cancelled task is aborted and then
/// joined, so by the time this returns no background task is still running
/// (and no task still holds its clone of the session context). The walk is
/// over the registry directly rather than relying on an aborted task to run
/// its own cleanup, so a grandchild is never orphaned.
fn cancel_uncollected_children(ctx: &SessionCtx, agent: AgentId) -> BoxFuture<'_, ()> {
    Box::pin(async move {
        let pool = {
            let mut registry = ctx.children.lock().expect("child registry poisoned");
            registry.remove(&agent)
        };
        let Some(pool) = pool else {
            return;
        };
        let mut cancelled = 0;
        for (id, handle) in pool.children {
            handle.join.abort();
            // Cancel this child's own descendants before joining it.
            cancel_uncollected_children(ctx, id.clone()).await;
            let _ = handle.join.await;
            // Emit the terminal AgentCompleted only if the child's own task
            // did not already emit it.
            if !handle.emitted.swap(true, Ordering::SeqCst) {
                ctx.bus.emit(EventPayload::AgentCompleted {
                    agent: id,
                    result: "cancelled (parent ended turn without collecting)".into(),
                    is_error: true,
                });
                cancelled += 1;
            }
            drop(handle._permit);
        }
        if cancelled > 0 {
            ctx.journal.append(JournalEntry::Lifecycle {
                message: format!("{agent}: cancelled {cancelled} uncollected subagent(s)"),
            });
        }
    })
}

fn harness_ok(message: impl Into<String>) -> ToolDisposition {
    ToolDisposition::Output {
        owner: "harness",
        output: ToolOutput::ok(message),
    }
}

fn harness_error(message: impl Into<String>) -> ToolDisposition {
    ToolDisposition::Output {
        owner: "harness",
        output: ToolOutput::error(message),
    }
}

/// How the top-level session ended.
pub(crate) struct SessionEnd {
    /// Final message (from the shutdown request or the last failure).
    pub message: Option<String>,
    /// The last failure message when the session ended through the
    /// consecutive-LLM-failure path.
    pub llm_failure: Option<String>,
    /// The mismatch detail when the session ended on an LLM or sandbox
    /// script mismatch.
    pub script_mismatch: Option<String>,
}

impl SessionEnd {
    fn normal(message: Option<String>) -> SessionEnd {
        SessionEnd {
            message,
            llm_failure: None,
            script_mismatch: None,
        }
    }
}

/// Appends a user prompt to the conversation. When the conversation
/// already ends with a user message — tool results from an interrupted
/// turn, or an earlier prompt whose turn was interrupted before any
/// assistant message — the prompt is added to that message as a text
/// block, so strict-alternation backends never see two consecutive user
/// messages. Otherwise it becomes a new user message.
fn push_user_prompt(messages: &mut Vec<Message>, text: String) {
    if let Some(last) = messages.last_mut() {
        if last.role == Role::User {
            last.content.push(ContentBlock::Text { text });
            return;
        }
    }
    messages.push(Message::user_text(text));
}

/// The top-level loop: emits AwaitingInput, waits for the next user input
/// (or a shutdown request), runs the turn, and repeats. Returns how the
/// session ended, with the final shutdown message.
pub(crate) async fn top_level_loop(ctx: &SessionCtx) -> Result<SessionEnd, HarnessError> {
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
            return Ok(SessionEnd::normal(message));
        }
        ctx.bus.emit(EventPayload::AwaitingInput);
        let input = tokio::select! {
            biased;
            message = ctx.shutdown.wait() => return Ok(SessionEnd::normal(message)),
            input = ctx.frontend.next_user_input() => input,
        };
        match input {
            Ok(text) => {
                // Commands sent while the harness was idle are applied
                // before the generation snapshot below, so a pre-turn
                // interrupt is consumed here and does not abort the new
                // turn.
                if let Some(message) = ctx.shutdown.check().await {
                    return Ok(SessionEnd::normal(message));
                }
                let turn_generation = ctx.shutdown.interrupt_generation();
                // The interactive frontend emits UserPrompt itself when a
                // client sends a prompt, so all clients see it immediately,
                // even mid-turn. For every other frontend kind the harness
                // emits UserPrompt when it consumes the input.
                if ctx.frontend.kind() != "interactive" {
                    ctx.bus.emit(EventPayload::UserPrompt {
                        client_id: None,
                        client_name: None,
                        text: text.clone(),
                    });
                }
                push_user_prompt(&mut messages, text);
                match drive(
                    ctx,
                    agent.clone(),
                    AgentKind::TopLevel,
                    &mut messages,
                    0,
                    turn_generation,
                )
                .await?
                {
                    TurnOutcome::Completed { .. } => {
                        consecutive_llm_failures = 0;
                        continue;
                    }
                    TurnOutcome::Interrupted => {
                        if let Err(error) = ctx.frontend.interrupt().await {
                            ctx.bus.emit(EventPayload::Error {
                                context: "frontend interrupt".into(),
                                message: error.to_string(),
                            });
                        }
                        ctx.bus.emit(EventPayload::Interrupted {
                            agent: agent.clone(),
                        });
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
                            return Ok(SessionEnd {
                                message: Some(message.clone()),
                                llm_failure: Some(message),
                                script_mismatch: None,
                            });
                        }
                        continue;
                    }
                    TurnOutcome::ScriptFailed(message) => {
                        ctx.journal.append(JournalEntry::Lifecycle {
                            message: format!("session ended by a script mismatch: {message}"),
                        });
                        return Ok(SessionEnd {
                            message: None,
                            llm_failure: None,
                            script_mismatch: Some(message),
                        });
                    }
                    TurnOutcome::ShutdownRequested => {
                        return Ok(SessionEnd::normal(
                            ctx.shutdown.check().await.unwrap_or(None),
                        ));
                    }
                }
            }
            Err(FrontendError::ScriptMismatch(detail)) => {
                // A scripted frontend with no further steps ends the
                // session.
                ctx.journal.append(JournalEntry::Lifecycle {
                    message: format!("frontend script exhausted: {detail}"),
                });
                return Ok(SessionEnd::normal(Some(
                    "frontend script exhausted".to_string(),
                )));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_merges_into_a_trailing_tool_result_message() {
        let mut messages = vec![
            Message::user_text("start"),
            Message::assistant(vec![ContentBlock::ToolUse {
                id: "t1".into(),
                name: "Bash".into(),
                input: serde_json::json!({"command": "ls"}),
            }]),
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "out".into(),
                    is_error: false,
                }],
            },
        ];
        push_user_prompt(&mut messages, "carry on".into());
        assert_eq!(messages.len(), 3);
        let last = messages.last().unwrap();
        assert_eq!(last.role, Role::User);
        assert_eq!(last.content.len(), 2);
        assert!(matches!(
            &last.content[1],
            ContentBlock::Text { text } if text == "carry on"
        ));
    }

    #[test]
    fn prompt_merges_into_any_trailing_user_message() {
        // An early interrupt can leave a plain user message (no tool
        // results) at the end of the conversation; the next prompt must
        // merge into it so user messages never appear back to back.
        let mut messages = vec![Message::user_text("first prompt")];
        push_user_prompt(&mut messages, "second prompt".into());
        assert_eq!(messages.len(), 1);
        let only = &messages[0];
        assert_eq!(only.role, Role::User);
        assert_eq!(only.content.len(), 2);
        assert!(matches!(
            &only.content[0],
            ContentBlock::Text { text } if text == "first prompt"
        ));
        assert!(matches!(
            &only.content[1],
            ContentBlock::Text { text } if text == "second prompt"
        ));
    }

    #[test]
    fn prompt_after_an_assistant_message_starts_a_new_user_message() {
        let mut messages = vec![
            Message::user_text("hi"),
            Message::assistant(vec![ContentBlock::Text {
                text: "hello".into(),
            }]),
        ];
        push_user_prompt(&mut messages, "next".into());
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[2].role, Role::User);
    }

    #[test]
    fn prompt_into_an_empty_conversation_is_a_user_message() {
        let mut messages = Vec::new();
        push_user_prompt(&mut messages, "go".into());
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, Role::User);
    }
}
