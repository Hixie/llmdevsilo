//! Deterministic replay of recorded sessions.
//!
//! A [`TestScript`] drives the mock LLM backend, the mock sandbox, the mock
//! frontend, and (optionally) the mock proxy through one session. Scripts
//! can be authored directly or generated from a journal with
//! [`script_from_journal`], which is how a recorded session becomes a
//! regression test. Sequencing never uses timers, so replays are race-free.
//!
//! The frontend list is consumed strictly in order (a single positional
//! cursor). The llm and tool lists are matched by *content*: each
//! [`SharedScript::next_llm`] picks the first unconsumed turn whose
//! `expect_user_contains` matches the request, and each
//! [`SharedScript::next_tool`] picks the first unconsumed entry whose
//! `expect_name` (and `expect_input` subset) matches the call. A single
//! agent still consumes these lists in their written order, because each
//! entry matches the call that comes next. Concurrent subagents share the
//! one llm and tool list, and content addressing keeps the replay
//! deterministic when those agents race: a turn is delivered to the agent
//! whose request matches it, not to whichever agent happened to ask first.
//! Exact event order across parallel children is still not guaranteed —
//! assert on the set of per-agent outcomes, not on a fixed interleaving.

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
    /// One flag per `script.llm` entry: true once that turn has been
    /// returned by `next_llm`. Concurrent subagents share the one llm list,
    /// so turns are matched by content (see `next_llm`) rather than by a
    /// single forward position, and a separate flag per entry records which
    /// have been consumed.
    llm_consumed: Vec<bool>,
    /// One flag per `script.tools` entry, consumed by content like the llm
    /// turns above.
    tool_consumed: Vec<bool>,
    frontend_cursor: usize,
    /// Events observed so far, for ExpectEvent checks.
    events_seen: Vec<Event>,
}

/// One script shared by all mock components in a session. Each component
/// consumes its own list; the frontend list is positional, while the llm
/// and tool lists are content-addressed so concurrent subagents racing on
/// the shared script stay deterministic.
#[derive(Clone)]
pub struct SharedScript {
    state: Arc<Mutex<ScriptState>>,
}

impl SharedScript {
    pub fn new(script: TestScript) -> Self {
        let llm_consumed = vec![false; script.llm.len()];
        let tool_consumed = vec![false; script.tools.len()];
        SharedScript {
            state: Arc::new(Mutex::new(ScriptState {
                script,
                llm_consumed,
                tool_consumed,
                frontend_cursor: 0,
                events_seen: Vec::new(),
            })),
        }
    }

    /// Consumes one scripted LLM turn, matched by content against the
    /// request. Among the still-unconsumed turns, the first whose
    /// `expect_user_contains` matches the request's last user text is
    /// chosen; if no unconsumed turn carries an expectation, the first
    /// unconsumed turn is chosen positionally (this is the single-agent
    /// case, where turns are consumed in order). A turn whose expectation
    /// matches nothing pending is left unconsumed, so a genuine mismatch
    /// still surfaces.
    pub fn next_llm(&self, request: &CompletionRequest) -> Result<CompletionResponse, LlmError> {
        let mut state = self.state.lock().expect("script state poisoned");
        if state.llm_consumed.iter().all(|done| *done) {
            return Err(LlmError::ScriptMismatch("llm script exhausted".into()));
        }
        let last_user_text = request
            .messages
            .iter()
            .rev()
            .find(|m| m.role == Role::User)
            .map(|m| m.text())
            .unwrap_or_default();
        let chosen = state
            .script
            .llm
            .iter()
            .enumerate()
            .find(|(index, turn)| {
                !state.llm_consumed[*index]
                    && turn
                        .expect_user_contains
                        .as_ref()
                        .is_some_and(|needle| last_user_text.contains(needle.as_str()))
            })
            .map(|(index, _)| index)
            .or_else(|| {
                // No unconsumed turn carries an expectation that matches;
                // fall back to the first unconsumed turn only when it has
                // no expectation, so an unmet expectation is a mismatch
                // rather than silently satisfied.
                state
                    .script
                    .llm
                    .iter()
                    .enumerate()
                    .find(|(index, _)| !state.llm_consumed[*index])
                    .filter(|(_, turn)| turn.expect_user_contains.is_none())
                    .map(|(index, _)| index)
            });
        let index = chosen.ok_or_else(|| {
            // Every unconsumed turn carries an expectation, and none matched.
            let expected: Vec<&str> = state
                .script
                .llm
                .iter()
                .enumerate()
                .filter(|(index, _)| !state.llm_consumed[*index])
                .filter_map(|(_, turn)| turn.expect_user_contains.as_deref())
                .collect();
            LlmError::ScriptMismatch(format!(
                "no unconsumed llm turn matches the request; \
                 expected one of {expected:?}, got {last_user_text:?}"
            ))
        })?;
        state.llm_consumed[index] = true;
        Ok(state.script.llm[index].response.clone())
    }

    /// Consumes one scripted tool execution, matched by content against the
    /// call. Among the still-unconsumed entries, the first whose
    /// `expect_name` matches (and whose `expect_input` subset matches, when
    /// present) is chosen; entries are consumed in order in the
    /// single-agent case, where each matches in turn.
    pub fn next_tool(&self, call: &ToolCall) -> Result<ToolOutput, SandboxError> {
        let mut state = self.state.lock().expect("script state poisoned");
        if state.tool_consumed.iter().all(|done| *done) {
            return Err(SandboxError::ScriptMismatch("tool script exhausted".into()));
        }
        let chosen = state
            .script
            .tools
            .iter()
            .enumerate()
            .find(|(index, scripted)| {
                !state.tool_consumed[*index]
                    && scripted.expect_name == call.name
                    && scripted
                        .expect_input
                        .as_ref()
                        .is_none_or(|expected| json_subset_matches(expected, &call.input))
            })
            .map(|(index, _)| index);
        let index = chosen.ok_or_else(|| {
            let expected: Vec<&str> = state
                .script
                .tools
                .iter()
                .enumerate()
                .filter(|(index, _)| !state.tool_consumed[*index])
                .map(|(_, scripted)| scripted.expect_name.as_str())
                .collect();
            SandboxError::ScriptMismatch(format!(
                "no unconsumed tool exec matches {:?} with input {}; \
                 expected one of {expected:?}",
                call.name, call.input
            ))
        })?;
        state.tool_consumed[index] = true;
        Ok(state.script.tools[index].output.clone())
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
        state.tool_consumed.iter().filter(|done| **done).count()
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
        state.llm_consumed.iter().all(|done| *done)
            && state.tool_consumed.iter().all(|done| *done)
            && state.frontend_cursor >= state.script.frontend.len()
    }

    /// Describes unconsumed script items, for test failure messages. The
    /// llm and tool counts are how many entries have been consumed across
    /// the whole list (matching by content, so not necessarily a prefix).
    pub fn remaining_summary(&self) -> String {
        let state = self.state.lock().expect("script state poisoned");
        let llm_done = state.llm_consumed.iter().filter(|done| **done).count();
        let tool_done = state.tool_consumed.iter().filter(|done| **done).count();
        format!(
            "llm {}/{}, tools {}/{}, frontend {}/{}",
            llm_done,
            state.script.llm.len(),
            tool_done,
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
