//! Mock frontend for deterministic end-to-end tests.
//!
//! Consumes the frontend portion of a [`SharedScript`] strictly in order.
//! `SendPrompt` steps supply user input, `ExpectEvent` steps block until a
//! matching event has been observed on the bus, `AnswerQuestion` steps
//! answer AskUserQuestion calls, `Interrupt` steps send
//! `FrontendCommand::Interrupt` (resolving the current AskUserQuestion as
//! interrupted when consumed inside one), `UploadFile` steps emit a
//! client-origin FileShared event and block until the harness has stored
//! the upload via the scripted sandbox Write, and `ExpectShutdown` matches
//! the final shutdown message. Sequencing is by script position and event
//! observation only; no timers are involved.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::{broadcast, mpsc, Notify};
use tokio::task::JoinHandle;

use silo_core::config::FrontendConfig;
use silo_core::conversation::AgentId;
use silo_core::error::FrontendError;
use silo_core::event::{EventBus, EventPayload, FileOrigin};
use silo_core::replay::{FrontendStep, SharedScript};
use silo_core::tool::{ToolCall, ToolDef, ToolOutput, INTERRUPTED_BY_USER};
use silo_core::traits::{Frontend, FrontendCommand, FrontendContext};

use crate::tools;

pub fn create(
    _config: &FrontendConfig,
    script: SharedScript,
) -> Result<Box<dyn Frontend>, FrontendError> {
    Ok(Box::new(MockFrontend {
        script,
        notify: Arc::new(Notify::new()),
        question_counter: AtomicU64::new(0),
        started: None,
    }))
}

pub struct MockFrontend {
    script: SharedScript,
    /// Signalled after every observed event, waking ExpectEvent waiters.
    notify: Arc<Notify>,
    question_counter: AtomicU64,
    started: Option<Started>,
}

struct Started {
    bus: EventBus,
    commands: mpsc::Sender<FrontendCommand>,
    observer: JoinHandle<()>,
}

impl MockFrontend {
    fn require_started(&self) -> Result<&Started, FrontendError> {
        self.started
            .as_ref()
            .ok_or_else(|| FrontendError::Setup("the mock frontend has not been started".into()))
    }

    /// Blocks until an event of `kind` (optionally containing `contains` in
    /// its serialized form) has been observed. Re-checks after every
    /// notification; never uses timers.
    async fn wait_for_event(&self, kind: &str, contains: Option<&str>) {
        loop {
            let notified = self.notify.notified();
            if self.script.event_was_seen(kind, contains) {
                return;
            }
            notified.await;
        }
    }

    fn mismatch(&self, context: &str) -> FrontendError {
        FrontendError::ScriptMismatch(format!(
            "{context} (remaining: {})",
            self.script.remaining_summary()
        ))
    }
}

#[async_trait]
impl Frontend for MockFrontend {
    fn kind(&self) -> &'static str {
        "mock"
    }

    fn tool_defs(&self) -> Vec<ToolDef> {
        vec![
            tools::ask_user_question_def(),
            tools::send_user_file_def(),
            tools::exit_def(),
        ]
    }

    async fn start(&mut self, ctx: FrontendContext) -> Result<(), FrontendError> {
        if self.started.is_some() {
            return Err(FrontendError::Setup(
                "the mock frontend is already started".into(),
            ));
        }
        let script = self.script.clone();
        let notify = self.notify.clone();
        let bus = ctx.bus.clone();
        let mut events_rx = bus.subscribe();
        let mut next = 0u64;
        for event in bus.since(0) {
            next = event.seq + 1;
            script.observe_event(event);
        }
        notify.notify_waiters();
        let observer_bus = bus.clone();
        let observer = tokio::spawn(async move {
            loop {
                match events_rx.recv().await {
                    Ok(event) => {
                        if event.seq >= next {
                            next = event.seq + 1;
                            script.observe_event(event);
                            notify.notify_waiters();
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        for event in observer_bus.since(next) {
                            next = event.seq + 1;
                            script.observe_event(event);
                        }
                        notify.notify_waiters();
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        self.started = Some(Started {
            bus,
            commands: ctx.commands.clone(),
            observer,
        });
        Ok(())
    }

    async fn next_user_input(&self) -> Result<String, FrontendError> {
        let started = self.require_started()?;
        loop {
            match self.script.next_frontend() {
                Some(FrontendStep::ExpectEvent { kind, contains }) => {
                    self.wait_for_event(&kind, contains.as_deref()).await;
                }
                Some(FrontendStep::UploadFile { name, content_b64 }) => {
                    // The harness upload listener stores the upload through
                    // a scripted sandbox Write on another task. This step
                    // completes once that execution has been consumed, so
                    // later steps stay ordered after the stored upload.
                    let tools_before = self.script.tools_consumed();
                    started.bus.emit(EventPayload::FileShared {
                        name,
                        content_b64,
                        origin: FileOrigin::Client {
                            client_id: "mock".into(),
                        },
                    });
                    while self.script.tools_consumed() <= tools_before {
                        tokio::task::yield_now().await;
                    }
                }
                Some(FrontendStep::SendPrompt { text }) => return Ok(text),
                Some(FrontendStep::Interrupt) => {
                    // An interrupt while the harness is idle: send the
                    // command and continue with the next step.
                    started
                        .commands
                        .send(FrontendCommand::Interrupt)
                        .await
                        .map_err(|_| {
                            FrontendError::Closed("the harness command channel is closed".into())
                        })?;
                }
                Some(other) => {
                    return Err(self.mismatch(&format!(
                        "expected a SendPrompt step when asked for user input, found {other:?}"
                    )))
                }
                None => {
                    return Err(self.mismatch("frontend script exhausted when asked for user input"))
                }
            }
        }
    }

    async fn run_tool(
        &self,
        agent: &AgentId,
        call: &ToolCall,
    ) -> Result<ToolOutput, FrontendError> {
        let started = self.require_started()?;
        match call.name.as_str() {
            "Exit" => {
                let message = tools::parse_exit_message(&call.input).ok();
                started
                    .commands
                    .send(FrontendCommand::Shutdown { message })
                    .await
                    .map_err(|_| {
                        FrontendError::Closed("the harness command channel is closed".into())
                    })?;
                Ok(ToolOutput::ok("exiting"))
            }
            "SendUserFile" => {
                let file = match tools::parse_send_user_file(&call.input) {
                    Ok(file) => file,
                    Err(error) => return Ok(ToolOutput::error(error)),
                };
                started.bus.emit(EventPayload::FileShared {
                    name: file.name.clone(),
                    content_b64: file.content_b64,
                    origin: FileOrigin::Llm {
                        agent: agent.clone(),
                    },
                });
                Ok(ToolOutput::ok(format!("sent {} to the user", file.name)))
            }
            "AskUserQuestion" => {
                let question = match tools::parse_question(&call.input) {
                    Ok(question) => question,
                    Err(error) => return Ok(ToolOutput::error(error)),
                };
                let question_json = serde_json::to_string(&question)
                    .map_err(|e| FrontendError::Setup(format!("unserializable question: {e}")))?;
                loop {
                    match self.script.next_frontend() {
                        Some(FrontendStep::ExpectEvent { kind, contains }) => {
                            self.wait_for_event(&kind, contains.as_deref()).await;
                        }
                        Some(FrontendStep::AnswerQuestion { contains, answer }) => {
                            if let Some(needle) = contains {
                                if !question_json.contains(&needle) {
                                    return Err(self.mismatch(&format!(
                                        "question {question_json} does not contain {needle:?}"
                                    )));
                                }
                            }
                            let id = format!(
                                "q-{}",
                                self.question_counter.fetch_add(1, Ordering::SeqCst) + 1
                            );
                            started.bus.emit(EventPayload::QuestionAsked {
                                id: id.clone(),
                                agent: agent.clone(),
                                question: question.clone(),
                            });
                            started.bus.emit(EventPayload::QuestionAnswered {
                                id,
                                client_id: None,
                                answer: answer.clone(),
                            });
                            return Ok(ToolOutput::ok(answer));
                        }
                        Some(FrontendStep::Interrupt) => {
                            // The interrupt cancels this question: send the
                            // command and resolve the call as interrupted.
                            started
                                .commands
                                .send(FrontendCommand::Interrupt)
                                .await
                                .map_err(|_| {
                                    FrontendError::Closed(
                                        "the harness command channel is closed".into(),
                                    )
                                })?;
                            return Ok(ToolOutput::error(INTERRUPTED_BY_USER));
                        }
                        Some(other) => {
                            return Err(self.mismatch(&format!(
                            "expected an AnswerQuestion step for AskUserQuestion, found {other:?}"
                        )))
                        }
                        None => {
                            return Err(self.mismatch(
                                "frontend script exhausted while answering AskUserQuestion",
                            ))
                        }
                    }
                }
            }
            other => Err(self.mismatch(&format!("unexpected frontend tool {other:?}"))),
        }
    }

    async fn shutdown(&mut self, message: Option<String>) -> Result<(), FrontendError> {
        if matches!(
            self.script.peek_frontend(),
            Some(FrontendStep::ExpectShutdown { .. })
        ) {
            if let Some(FrontendStep::ExpectShutdown {
                message_contains: Some(needle),
            }) = self.script.next_frontend()
            {
                let actual = message.clone().unwrap_or_default();
                if !actual.contains(&needle) {
                    return Err(FrontendError::ScriptMismatch(format!(
                        "shutdown message {actual:?} does not contain {needle:?}"
                    )));
                }
            }
        }
        if let Some(started) = self.started.take() {
            started.observer.abort();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;
    use silo_core::clock::{FakeClock, SharedClock};
    use silo_core::config::FrontendKind;
    use silo_core::journal::JournalHandle;
    use silo_core::replay::TestScript;
    use silo_core::sandbox::AccessReport;

    use super::*;

    fn bus() -> EventBus {
        let clock: SharedClock = Arc::new(FakeClock::default());
        EventBus::new(clock.clone(), JournalHandle::disabled(clock))
    }

    fn config() -> FrontendConfig {
        FrontendConfig {
            kind: FrontendKind::Mock,
            ..FrontendConfig::default()
        }
    }

    async fn started_frontend(
        script: SharedScript,
        bus: &EventBus,
    ) -> (Box<dyn Frontend>, mpsc::Receiver<FrontendCommand>) {
        let (tx, rx) = mpsc::channel(4);
        let mut frontend = create(&config(), script).unwrap();
        let ctx = FrontendContext {
            harness_id: "h1".into(),
            bus: bus.clone(),
            commands: tx,
            access: AccessReport::default(),
            state_dir: std::env::temp_dir(),
            workspace: "/tmp/ws".into(),
            configured_read_allowlist: Vec::new(),
        };
        frontend.start(ctx).await.unwrap();
        (frontend, rx)
    }

    #[tokio::test]
    async fn events_emitted_before_start_are_observed() {
        let bus = bus();
        bus.emit(EventPayload::AwaitingInput);
        let script = SharedScript::new(TestScript {
            frontend: vec![
                FrontendStep::ExpectEvent {
                    kind: "awaiting_input".into(),
                    contains: None,
                },
                FrontendStep::SendPrompt { text: "go".into() },
            ],
            ..TestScript::default()
        });
        let (frontend, _rx) = started_frontend(script.clone(), &bus).await;
        assert_eq!(frontend.next_user_input().await.unwrap(), "go");
        assert!(script.finished());
    }

    #[tokio::test]
    async fn unexpected_step_for_input_is_a_script_mismatch() {
        let bus = bus();
        let script = SharedScript::new(TestScript {
            frontend: vec![FrontendStep::AnswerQuestion {
                contains: None,
                answer: "x".into(),
            }],
            ..TestScript::default()
        });
        let (frontend, _rx) = started_frontend(script, &bus).await;
        assert!(matches!(
            frontend.next_user_input().await,
            Err(FrontendError::ScriptMismatch(_))
        ));
    }

    #[tokio::test]
    async fn exhausted_script_is_a_script_mismatch() {
        let bus = bus();
        let script = SharedScript::new(TestScript::default());
        let (frontend, _rx) = started_frontend(script, &bus).await;
        assert!(matches!(
            frontend.next_user_input().await,
            Err(FrontendError::ScriptMismatch(_))
        ));
    }

    #[tokio::test]
    async fn answer_question_checks_the_contains_filter() {
        let bus = bus();
        let script = SharedScript::new(TestScript {
            frontend: vec![FrontendStep::AnswerQuestion {
                contains: Some("color".into()),
                answer: "blue".into(),
            }],
            ..TestScript::default()
        });
        let (frontend, _rx) = started_frontend(script, &bus).await;
        let call = ToolCall {
            id: "t1".into(),
            name: "AskUserQuestion".into(),
            input: json!({"question": "Which size?"}),
        };
        assert!(matches!(
            frontend.run_tool(&"agent-0".to_string(), &call).await,
            Err(FrontendError::ScriptMismatch(_))
        ));
    }

    #[tokio::test]
    async fn upload_step_emits_file_shared_and_waits_for_the_write() {
        use silo_core::replay::ScriptedToolExec;

        let bus = bus();
        let script = SharedScript::new(TestScript {
            tools: vec![ScriptedToolExec {
                expect_name: "Write".into(),
                expect_input: None,
                output: ToolOutput::ok(""),
            }],
            frontend: vec![
                FrontendStep::UploadFile {
                    name: "blob.bin".into(),
                    content_b64: "3q2+7w==".into(),
                },
                FrontendStep::SendPrompt { text: "go".into() },
            ],
            ..TestScript::default()
        });
        let (frontend, _rx) = started_frontend(script.clone(), &bus).await;

        // Stand-in for the harness upload listener: consume the scripted
        // Write once the client-origin FileShared event arrives.
        let listener_script = script.clone();
        let mut events = bus.subscribe();
        let listener = tokio::spawn(async move {
            loop {
                let event = events.recv().await.expect("event");
                if let EventPayload::FileShared {
                    name,
                    content_b64,
                    origin: FileOrigin::Client { .. },
                } = event.payload
                {
                    assert_eq!(name, "blob.bin");
                    assert_eq!(content_b64, "3q2+7w==");
                    listener_script
                        .next_tool(&ToolCall {
                            id: "u1".into(),
                            name: "Write".into(),
                            input: json!({}),
                        })
                        .unwrap();
                    return;
                }
            }
        });

        assert_eq!(frontend.next_user_input().await.unwrap(), "go");
        listener.await.unwrap();
        assert!(
            script.finished(),
            "remaining: {}",
            script.remaining_summary()
        );
    }

    #[tokio::test]
    async fn interrupt_step_during_input_sends_the_command_and_continues() {
        let bus = bus();
        let script = SharedScript::new(TestScript {
            frontend: vec![
                FrontendStep::Interrupt,
                FrontendStep::SendPrompt { text: "go".into() },
            ],
            ..TestScript::default()
        });
        let (frontend, mut rx) = started_frontend(script.clone(), &bus).await;
        assert_eq!(frontend.next_user_input().await.unwrap(), "go");
        assert_eq!(rx.recv().await.unwrap(), FrontendCommand::Interrupt);
        assert!(script.finished());
    }

    #[tokio::test]
    async fn interrupt_step_during_a_question_resolves_it_as_interrupted() {
        let bus = bus();
        let script = SharedScript::new(TestScript {
            frontend: vec![FrontendStep::Interrupt],
            ..TestScript::default()
        });
        let (frontend, mut rx) = started_frontend(script.clone(), &bus).await;
        let call = ToolCall {
            id: "t1".into(),
            name: "AskUserQuestion".into(),
            input: json!({"question": "Proceed?"}),
        };
        let output = frontend
            .run_tool(&"agent-0".to_string(), &call)
            .await
            .unwrap();
        assert!(output.is_error);
        assert_eq!(output.content, "[interrupted by the user]");
        assert_eq!(rx.recv().await.unwrap(), FrontendCommand::Interrupt);
        assert!(script.finished());
    }

    #[tokio::test]
    async fn shutdown_without_expect_step_leaves_the_script_alone() {
        let bus = bus();
        let script = SharedScript::new(TestScript {
            frontend: vec![FrontendStep::SendPrompt {
                text: "unused".into(),
            }],
            ..TestScript::default()
        });
        let (mut frontend, _rx) = started_frontend(script.clone(), &bus).await;
        frontend.shutdown(Some("bye".into())).await.unwrap();
        assert!(!script.finished());
        assert!(matches!(
            script.peek_frontend(),
            Some(FrontendStep::SendPrompt { .. })
        ));
    }

    #[tokio::test]
    async fn shutdown_message_mismatch_is_reported() {
        let bus = bus();
        let script = SharedScript::new(TestScript {
            frontend: vec![FrontendStep::ExpectShutdown {
                message_contains: Some("done".into()),
            }],
            ..TestScript::default()
        });
        let (mut frontend, _rx) = started_frontend(script, &bus).await;
        assert!(matches!(
            frontend.shutdown(Some("failed".into())).await,
            Err(FrontendError::ScriptMismatch(_))
        ));
    }
}
