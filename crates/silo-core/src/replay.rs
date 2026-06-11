//! Deterministic replay of recorded sessions.
//!
//! A [`TestScript`] drives the mock LLM backend, the mock sandbox, the mock
//! frontend, and (optionally) the mock proxy through one session. Scripts
//! can be authored directly or generated from a journal with
//! [`script_from_journal`], which is how a recorded session becomes a
//! regression test. Mock components consume their portion of a
//! [`SharedScript`] strictly in order — sequencing is by script position,
//! never by timers — so replays are race-free.

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::conversation::{CompletionRequest, CompletionResponse, Role};
use crate::error::{LlmError, SandboxError};
use crate::event::Event;
use crate::journal::{JournalEntry, JournalRecord, NetworkRecord};
use crate::tool::{ToolCall, ToolOutput};
use crate::traits::FrontendCommand;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScriptedLlmTurn {
    /// If set, the text of the most recent user message must contain this
    /// substring; otherwise the mock backend reports a mismatch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect_user_contains: Option<String>,
    pub response: CompletionResponse,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScriptedToolExec {
    pub expect_name: String,
    /// If set, every key in this object must appear with an equal value in
    /// the actual tool input (recursively for nested objects).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect_input: Option<serde_json::Value>,
    pub output: ToolOutput,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "step", rename_all = "snake_case")]
pub enum FrontendStep {
    /// The mock frontend sends this prompt when the harness asks for input.
    SendPrompt { text: String },
    /// The mock frontend asserts that an event of this kind (and optionally
    /// containing this substring in its JSON form) has been observed before
    /// proceeding.
    ExpectEvent {
        kind: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        contains: Option<String>,
    },
    /// The mock frontend answers the next AskUserQuestion with this.
    AnswerQuestion {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        contains: Option<String>,
        answer: String,
    },
    /// The mock frontend emits a client-origin FileShared event with this
    /// content, then waits until the harness upload listener has consumed
    /// the corresponding scripted sandbox Write.
    UploadFile { name: String, content_b64: String },
    /// The mock frontend sends `FrontendCommand::Interrupt`. Consumed while
    /// answering an AskUserQuestion (the question resolves as interrupted)
    /// or while supplying user input (the interrupt arrives while the
    /// harness is idle).
    Interrupt,
    /// The session is expected to end (Exit tool or client shutdown).
    ExpectShutdown {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message_contains: Option<String>,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TestScript {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub llm: Vec<ScriptedLlmTurn>,
    #[serde(default)]
    pub tools: Vec<ScriptedToolExec>,
    #[serde(default)]
    pub frontend: Vec<FrontendStep>,
    /// Expected network operations, for tests that exercise the mock proxy.
    #[serde(default)]
    pub network: Vec<NetworkRecord>,
}

impl TestScript {
    pub fn load(path: &std::path::Path) -> std::io::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        serde_json::from_str(&text)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    pub fn save(&self, path: &std::path::Path) -> std::io::Result<()> {
        let text = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(path, text)
    }
}

/// Returns true when every key/value in `expected` appears in `actual`
/// (recursively for objects; exact equality for everything else).
pub fn json_subset_matches(expected: &serde_json::Value, actual: &serde_json::Value) -> bool {
    match (expected, actual) {
        (serde_json::Value::Object(exp), serde_json::Value::Object(act)) => {
            exp.iter().all(|(key, value)| {
                act.get(key)
                    .is_some_and(|actual_value| json_subset_matches(value, actual_value))
            })
        }
        _ => expected == actual,
    }
}

struct ScriptState {
    script: TestScript,
    llm_cursor: usize,
    tool_cursor: usize,
    frontend_cursor: usize,
    /// Events observed so far, for ExpectEvent checks.
    events_seen: Vec<Event>,
}

/// One script shared by all mock components in a session. Each component
/// consumes its own list in order; cursors are independent.
#[derive(Clone)]
pub struct SharedScript {
    state: Arc<Mutex<ScriptState>>,
}

impl SharedScript {
    pub fn new(script: TestScript) -> Self {
        SharedScript {
            state: Arc::new(Mutex::new(ScriptState {
                script,
                llm_cursor: 0,
                tool_cursor: 0,
                frontend_cursor: 0,
                events_seen: Vec::new(),
            })),
        }
    }

    /// Consumes the next scripted LLM turn, validating expectations against
    /// the actual request.
    pub fn next_llm(&self, request: &CompletionRequest) -> Result<CompletionResponse, LlmError> {
        let mut state = self.state.lock().expect("script state poisoned");
        let cursor = state.llm_cursor;
        let turn = state
            .script
            .llm
            .get(cursor)
            .cloned()
            .ok_or_else(|| LlmError::ScriptMismatch("llm script exhausted".into()))?;
        if let Some(expected) = &turn.expect_user_contains {
            let last_user_text = request
                .messages
                .iter()
                .rev()
                .find(|m| m.role == Role::User)
                .map(|m| m.text())
                .unwrap_or_default();
            if !last_user_text.contains(expected.as_str()) {
                return Err(LlmError::ScriptMismatch(format!(
                    "llm turn {cursor}: expected user message containing {expected:?}, got {last_user_text:?}"
                )));
            }
        }
        state.llm_cursor += 1;
        Ok(turn.response)
    }

    /// Consumes the next scripted tool execution, validating the call.
    pub fn next_tool(&self, call: &ToolCall) -> Result<ToolOutput, SandboxError> {
        let mut state = self.state.lock().expect("script state poisoned");
        let cursor = state.tool_cursor;
        let scripted = state
            .script
            .tools
            .get(cursor)
            .cloned()
            .ok_or_else(|| SandboxError::ScriptMismatch("tool script exhausted".into()))?;
        if scripted.expect_name != call.name {
            return Err(SandboxError::ScriptMismatch(format!(
                "tool exec {cursor}: expected tool {:?}, got {:?}",
                scripted.expect_name, call.name
            )));
        }
        if let Some(expected_input) = &scripted.expect_input {
            if !json_subset_matches(expected_input, &call.input) {
                return Err(SandboxError::ScriptMismatch(format!(
                    "tool exec {cursor} ({}): input {} does not match expectation {}",
                    call.name, call.input, expected_input
                )));
            }
        }
        state.tool_cursor += 1;
        Ok(scripted.output)
    }

    /// Consumes the next frontend step, if any.
    pub fn next_frontend(&self) -> Option<FrontendStep> {
        let mut state = self.state.lock().expect("script state poisoned");
        let step = state.script.frontend.get(state.frontend_cursor).cloned();
        if step.is_some() {
            state.frontend_cursor += 1;
        }
        step
    }

    pub fn peek_frontend(&self) -> Option<FrontendStep> {
        let state = self.state.lock().expect("script state poisoned");
        state.script.frontend.get(state.frontend_cursor).cloned()
    }

    /// Number of scripted tool executions consumed so far.
    pub fn tools_consumed(&self) -> usize {
        let state = self.state.lock().expect("script state poisoned");
        state.tool_cursor
    }

    /// Records an observed event for later ExpectEvent checks.
    pub fn observe_event(&self, event: Event) {
        let mut state = self.state.lock().expect("script state poisoned");
        state.events_seen.push(event);
    }

    /// Checks an ExpectEvent step against all events observed so far.
    pub fn event_was_seen(&self, kind: &str, contains: Option<&str>) -> bool {
        let state = self.state.lock().expect("script state poisoned");
        state.events_seen.iter().any(|event| {
            if event.payload.kind() != kind {
                return false;
            }
            match contains {
                None => true,
                Some(needle) => serde_json::to_string(&event.payload)
                    .map(|json| json.contains(needle))
                    .unwrap_or(false),
            }
        })
    }

    /// True when every scripted item has been consumed.
    pub fn finished(&self) -> bool {
        let state = self.state.lock().expect("script state poisoned");
        state.llm_cursor >= state.script.llm.len()
            && state.tool_cursor >= state.script.tools.len()
            && state.frontend_cursor >= state.script.frontend.len()
    }

    /// Describes unconsumed script items, for test failure messages.
    pub fn remaining_summary(&self) -> String {
        let state = self.state.lock().expect("script state poisoned");
        format!(
            "llm {}/{}, tools {}/{}, frontend {}/{}",
            state.llm_cursor,
            state.script.llm.len(),
            state.tool_cursor,
            state.script.tools.len(),
            state.frontend_cursor,
            state.script.frontend.len()
        )
    }
}

/// Builds a replayable script from a recorded journal.
pub fn script_from_journal(records: &[JournalRecord], name: &str) -> TestScript {
    let mut script = TestScript {
        name: name.to_string(),
        ..TestScript::default()
    };
    for record in records {
        match &record.entry {
            JournalEntry::LlmResponse { response, .. } => {
                script.llm.push(ScriptedLlmTurn {
                    expect_user_contains: None,
                    response: response.clone(),
                });
            }
            JournalEntry::ToolExec {
                owner,
                call,
                output,
                ..
            } if owner == "sandbox" => {
                script.tools.push(ScriptedToolExec {
                    expect_name: call.name.clone(),
                    expect_input: Some(call.input.clone()),
                    output: output.clone(),
                });
            }
            JournalEntry::Event { event } => match &event.payload {
                crate::event::EventPayload::UserPrompt { text, .. } => {
                    script
                        .frontend
                        .push(FrontendStep::SendPrompt { text: text.clone() });
                }
                crate::event::EventPayload::QuestionAnswered {
                    client_id, answer, ..
                } => {
                    // Answers recorded when an interrupt cancels a question
                    // are covered by the Interrupt step generated from the
                    // journaled frontend command; only client answers
                    // become AnswerQuestion steps.
                    if client_id.is_some() || answer != crate::event::INTERRUPTED_ANSWER {
                        script.frontend.push(FrontendStep::AnswerQuestion {
                            contains: None,
                            answer: answer.clone(),
                        });
                    }
                }
                crate::event::EventPayload::FileShared {
                    name,
                    content_b64,
                    origin: crate::event::FileOrigin::Client { .. },
                } => {
                    script.frontend.push(FrontendStep::UploadFile {
                        name: name.clone(),
                        content_b64: content_b64.clone(),
                    });
                }
                crate::event::EventPayload::Shutdown { message } => {
                    script.frontend.push(FrontendStep::ExpectShutdown {
                        message_contains: message.clone(),
                    });
                }
                _ => {}
            },
            JournalEntry::FrontendCommand { command } => {
                // An interrupt is journaled as a frontend command at the
                // point where the harness consumed it, which is where the
                // replay re-injects it. The Interrupted event derived from
                // the command produces no step.
                if let Ok(FrontendCommand::Interrupt) =
                    serde_json::from_value::<FrontendCommand>(command.clone())
                {
                    script.frontend.push(FrontendStep::Interrupt);
                }
            }
            JournalEntry::Network { record } => {
                script.network.push(record.clone());
            }
            _ => {}
        }
    }
    script
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation::{ContentBlock, StopReason, TokenDelta};
    use serde_json::json;

    fn response(text: &str) -> CompletionResponse {
        CompletionResponse {
            content: vec![ContentBlock::Text { text: text.into() }],
            stop_reason: StopReason::EndTurn,
            usage: TokenDelta::default(),
        }
    }

    #[test]
    fn llm_script_validates_and_exhausts() {
        let script = SharedScript::new(TestScript {
            llm: vec![ScriptedLlmTurn {
                expect_user_contains: Some("hello".into()),
                response: response("hi"),
            }],
            ..TestScript::default()
        });
        let request = CompletionRequest {
            system: String::new(),
            messages: vec![crate::conversation::Message::user_text("hello world")],
            tools: vec![],
            max_tokens: 100,
        };
        let resp = script.next_llm(&request).unwrap();
        assert_eq!(resp.text(), "hi");
        assert!(script.next_llm(&request).is_err());
        assert!(script.finished());
    }

    #[test]
    fn tool_script_checks_subset_input() {
        let script = SharedScript::new(TestScript {
            tools: vec![ScriptedToolExec {
                expect_name: "Bash".into(),
                expect_input: Some(json!({"command": "ls"})),
                output: ToolOutput::ok("file.txt"),
            }],
            ..TestScript::default()
        });
        let call = ToolCall {
            id: "t1".into(),
            name: "Bash".into(),
            input: json!({"command": "ls", "timeout_ms": 500}),
        };
        assert_eq!(script.next_tool(&call).unwrap().content, "file.txt");
    }

    #[test]
    fn tool_script_rejects_wrong_tool() {
        let script = SharedScript::new(TestScript {
            tools: vec![ScriptedToolExec {
                expect_name: "Read".into(),
                expect_input: None,
                output: ToolOutput::ok(""),
            }],
            ..TestScript::default()
        });
        let call = ToolCall {
            id: "t1".into(),
            name: "Bash".into(),
            input: json!({}),
        };
        assert!(script.next_tool(&call).is_err());
    }

    #[test]
    fn tools_consumed_tracks_the_cursor() {
        let script = SharedScript::new(TestScript {
            tools: vec![ScriptedToolExec {
                expect_name: "Write".into(),
                expect_input: None,
                output: ToolOutput::ok(""),
            }],
            ..TestScript::default()
        });
        assert_eq!(script.tools_consumed(), 0);
        let call = ToolCall {
            id: "t1".into(),
            name: "Write".into(),
            input: json!({}),
        };
        script.next_tool(&call).unwrap();
        assert_eq!(script.tools_consumed(), 1);
    }

    #[test]
    fn client_file_shared_becomes_an_upload_step() {
        use crate::clock::Timestamp;
        use crate::event::{Event, EventPayload, FileOrigin};

        let time = Timestamp {
            logical: 0,
            wall_ms: None,
        };
        let event_record = |seq: u64, payload: EventPayload| JournalRecord {
            seq,
            time,
            entry: JournalEntry::Event {
                event: Event { seq, time, payload },
            },
        };
        let records = vec![
            event_record(
                0,
                EventPayload::FileShared {
                    name: "blob.bin".into(),
                    content_b64: "3q2+7w==".into(),
                    origin: FileOrigin::Client {
                        client_id: "c1".into(),
                    },
                },
            ),
            event_record(
                1,
                EventPayload::FileShared {
                    name: "report.txt".into(),
                    content_b64: "aGk=".into(),
                    origin: FileOrigin::Llm {
                        agent: "agent-0".into(),
                    },
                },
            ),
        ];
        let script = script_from_journal(&records, "uploads");
        assert_eq!(
            script.frontend,
            vec![FrontendStep::UploadFile {
                name: "blob.bin".into(),
                content_b64: "3q2+7w==".into(),
            }]
        );
    }

    #[test]
    fn interrupt_commands_become_steps_and_derived_events_do_not() {
        use crate::clock::Timestamp;
        use crate::event::{Event, EventPayload, INTERRUPTED_ANSWER};

        let time = Timestamp {
            logical: 0,
            wall_ms: None,
        };
        let event_record = |seq: u64, payload: EventPayload| JournalRecord {
            seq,
            time,
            entry: JournalEntry::Event {
                event: Event { seq, time, payload },
            },
        };
        let command_record = |seq: u64, command: serde_json::Value| JournalRecord {
            seq,
            time,
            entry: JournalEntry::FrontendCommand { command },
        };
        let records = vec![
            event_record(
                0,
                EventPayload::UserPrompt {
                    client_id: None,
                    client_name: None,
                    text: "ask me".into(),
                },
            ),
            command_record(1, json!({"command": "interrupt"})),
            // Derived from the interrupt: produces no step.
            event_record(
                2,
                EventPayload::QuestionAnswered {
                    id: "q1".into(),
                    client_id: None,
                    answer: INTERRUPTED_ANSWER.into(),
                },
            ),
            event_record(
                3,
                EventPayload::Interrupted {
                    agent: "agent-0".into(),
                },
            ),
            // A real answer still becomes an AnswerQuestion step.
            event_record(
                4,
                EventPayload::QuestionAnswered {
                    id: "q2".into(),
                    client_id: Some("c1".into()),
                    answer: "blue".into(),
                },
            ),
            // Shutdown commands map through the Shutdown event, not here.
            command_record(5, json!({"command": "shutdown"})),
        ];
        let script = script_from_journal(&records, "interrupts");
        assert_eq!(
            script.frontend,
            vec![
                FrontendStep::SendPrompt {
                    text: "ask me".into()
                },
                FrontendStep::Interrupt,
                FrontendStep::AnswerQuestion {
                    contains: None,
                    answer: "blue".into()
                },
            ]
        );
    }

    #[test]
    fn pretty_name_fields_do_not_change_the_script() {
        use crate::clock::Timestamp;
        use crate::event::{Event, EventPayload};

        let time = Timestamp {
            logical: 0,
            wall_ms: None,
        };
        let event_record = |seq: u64, payload: EventPayload| JournalRecord {
            seq,
            time,
            entry: JournalEntry::Event {
                event: Event { seq, time, payload },
            },
        };
        let records = vec![
            event_record(
                0,
                EventPayload::UserPrompt {
                    client_id: Some("c1".into()),
                    client_name: Some("Ian's phone".into()),
                    text: "go".into(),
                },
            ),
            event_record(
                1,
                EventPayload::AgentSpawned {
                    parent: "agent-0".into(),
                    agent: "agent-1".into(),
                    name: Some("refactor tests".into()),
                    prompt: "do it".into(),
                },
            ),
        ];
        let script = script_from_journal(&records, "names");
        // The prompt step carries only the text; agent_spawned produces no
        // step at all.
        assert_eq!(
            script.frontend,
            vec![FrontendStep::SendPrompt { text: "go".into() }]
        );
    }

    #[test]
    fn subset_matching_is_recursive() {
        let expected = json!({"a": {"b": 1}});
        let actual = json!({"a": {"b": 1, "c": 2}, "d": 3});
        assert!(json_subset_matches(&expected, &actual));
        assert!(!json_subset_matches(&json!({"a": {"b": 2}}), &actual));
    }
}
