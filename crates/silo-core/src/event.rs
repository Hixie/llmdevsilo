//! The event stream shared by all frontends.
//!
//! Every user-visible occurrence in a harness session is an [`Event`] with a
//! sequence number that starts at zero and increments by one. Connecting
//! clients can request all events from a given sequence number to catch up,
//! and all connected clients observe the same stream.

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::clock::{SharedClock, Timestamp};
use crate::conversation::{AgentId, StopReason};
use crate::cost::{QuotaConfig, UsageSnapshot};
use crate::journal::{JournalEntry, JournalHandle};
use crate::sandbox::AccessReport;
use crate::tool::{ToolCall, ToolOutput};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub seq: u64,
    pub time: Timestamp,
    #[serde(flatten)]
    pub payload: EventPayload,
}

/// Answer recorded in `QuestionAnswered` for questions cancelled by a user
/// interrupt.
pub const INTERRUPTED_ANSWER: &str = "[interrupted]";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QuestionOption {
    pub label: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UserQuestion {
    pub question: String,
    #[serde(default)]
    pub options: Vec<QuestionOption>,
    #[serde(default)]
    pub multi_select: bool,
    #[serde(default)]
    pub allow_free_text: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "origin", rename_all = "snake_case")]
pub enum FileOrigin {
    /// Uploaded by a client; shown in all clients.
    Client { client_id: String },
    /// Sent by the LLM via the SendUserFile tool.
    Llm { agent: AgentId },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventPayload {
    HarnessStarted {
        harness_id: String,
        workspace: String,
        sandbox: String,
        llm: String,
    },
    /// A user prompt was accepted (from whichever client sent it first).
    UserPrompt {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_id: Option<String>,
        /// Display name of the sending client (registered at pairing);
        /// absent for local-token clients and non-interactive frontends.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_name: Option<String>,
        text: String,
    },
    AssistantText {
        agent: AgentId,
        text: String,
    },
    ToolUse {
        agent: AgentId,
        call: ToolCall,
    },
    ToolResult {
        agent: AgentId,
        tool_use_id: String,
        tool_name: String,
        output: ToolOutput,
    },
    AgentSpawned {
        parent: AgentId,
        agent: AgentId,
        /// Display name from the Agent tool's "name" input; absent when the
        /// model gave none.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        prompt: String,
    },
    AgentCompleted {
        agent: AgentId,
        result: String,
        is_error: bool,
    },
    /// Shown on all clients; answered by the first to respond.
    QuestionAsked {
        id: String,
        agent: AgentId,
        question: UserQuestion,
    },
    /// Removes the question UI everywhere and records the answer.
    QuestionAnswered {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_id: Option<String>,
        answer: String,
    },
    FileShared {
        name: String,
        content_b64: String,
        #[serde(flatten)]
        origin: FileOrigin,
    },
    CostReport {
        backend: String,
        usage: UsageSnapshot,
        quota: QuotaConfig,
    },
    TurnComplete {
        agent: AgentId,
        stop_reason: StopReason,
    },
    /// The user aborted the turn; emitted in place of `TurnComplete`.
    Interrupted {
        agent: AgentId,
    },
    /// The harness is idle and the next client input starts the next turn.
    AwaitingInput,
    AccessReportUpdated {
        report: AccessReport,
    },
    Error {
        context: String,
        message: String,
    },
    Shutdown {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
}

impl EventPayload {
    /// Stable name of the payload variant, as used in serialized form and in
    /// test-script expectations.
    pub fn kind(&self) -> &'static str {
        match self {
            EventPayload::HarnessStarted { .. } => "harness_started",
            EventPayload::UserPrompt { .. } => "user_prompt",
            EventPayload::AssistantText { .. } => "assistant_text",
            EventPayload::ToolUse { .. } => "tool_use",
            EventPayload::ToolResult { .. } => "tool_result",
            EventPayload::AgentSpawned { .. } => "agent_spawned",
            EventPayload::AgentCompleted { .. } => "agent_completed",
            EventPayload::QuestionAsked { .. } => "question_asked",
            EventPayload::QuestionAnswered { .. } => "question_answered",
            EventPayload::FileShared { .. } => "file_shared",
            EventPayload::CostReport { .. } => "cost_report",
            EventPayload::TurnComplete { .. } => "turn_complete",
            EventPayload::Interrupted { .. } => "interrupted",
            EventPayload::AwaitingInput => "awaiting_input",
            EventPayload::AccessReportUpdated { .. } => "access_report_updated",
            EventPayload::Error { .. } => "error",
            EventPayload::Shutdown { .. } => "shutdown",
        }
    }
}

struct EventBusInner {
    clock: SharedClock,
    journal: JournalHandle,
    history: Mutex<Vec<Event>>,
    sender: broadcast::Sender<Event>,
}

/// Assigns sequence numbers, retains history for catch-up, journals every
/// event, and broadcasts to subscribers.
#[derive(Clone)]
pub struct EventBus {
    inner: Arc<EventBusInner>,
}

impl EventBus {
    pub fn new(clock: SharedClock, journal: JournalHandle) -> Self {
        let (sender, _) = broadcast::channel(4096);
        EventBus {
            inner: Arc::new(EventBusInner {
                clock,
                journal,
                history: Mutex::new(Vec::new()),
                sender,
            }),
        }
    }

    pub fn emit(&self, payload: EventPayload) -> Event {
        let mut history = self.inner.history.lock().expect("event history poisoned");
        let event = Event {
            seq: history.len() as u64,
            time: self.inner.clock.now(),
            payload,
        };
        history.push(event.clone());
        drop(history);
        self.inner.journal.append(JournalEntry::Event {
            event: event.clone(),
        });
        let _ = self.inner.sender.send(event.clone());
        event
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.inner.sender.subscribe()
    }

    /// All events with `seq >= from_seq`, for clients catching up.
    pub fn since(&self, from_seq: u64) -> Vec<Event> {
        let history = self.inner.history.lock().expect("event history poisoned");
        history
            .iter()
            .filter(|e| e.seq >= from_seq)
            .cloned()
            .collect()
    }

    pub fn next_seq(&self) -> u64 {
        self.inner
            .history
            .lock()
            .expect("event history poisoned")
            .len() as u64
    }

    pub fn clock(&self) -> &SharedClock {
        &self.inner.clock
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::FakeClock;
    use crate::journal::JournalWriter;

    fn bus() -> EventBus {
        let clock: SharedClock = Arc::new(FakeClock::default());
        EventBus::new(
            clock.clone(),
            JournalHandle::new(JournalWriter::disabled(clock)),
        )
    }

    #[tokio::test]
    async fn events_are_sequenced_from_zero_and_broadcast() {
        let bus = bus();
        let mut rx = bus.subscribe();
        let e0 = bus.emit(EventPayload::AwaitingInput);
        let e1 = bus.emit(EventPayload::UserPrompt {
            client_id: None,
            client_name: None,
            text: "hi".into(),
        });
        assert_eq!(e0.seq, 0);
        assert_eq!(e1.seq, 1);
        assert_eq!(rx.recv().await.unwrap().seq, 0);
        assert_eq!(rx.recv().await.unwrap().seq, 1);
        assert_eq!(bus.since(1).len(), 1);
        assert_eq!(bus.next_seq(), 2);
    }

    #[test]
    fn payload_kind_matches_serialized_tag() {
        let payload = EventPayload::UserPrompt {
            client_id: None,
            client_name: None,
            text: "x".into(),
        };
        let value = serde_json::to_value(&payload).unwrap();
        assert_eq!(value["kind"], payload.kind());
    }

    #[test]
    fn user_prompt_wire_format_with_and_without_client_name() {
        let named = EventPayload::UserPrompt {
            client_id: Some("c1".into()),
            client_name: Some("Ian's phone".into()),
            text: "hello".into(),
        };
        let value = serde_json::to_value(&named).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "kind": "user_prompt",
                "client_id": "c1",
                "client_name": "Ian's phone",
                "text": "hello",
            })
        );
        let parsed: EventPayload = serde_json::from_value(value).unwrap();
        assert_eq!(parsed, named);

        // The name is omitted when absent, and old payloads without the
        // field still parse.
        let anonymous = EventPayload::UserPrompt {
            client_id: None,
            client_name: None,
            text: "hello".into(),
        };
        let value = serde_json::to_value(&anonymous).unwrap();
        assert_eq!(
            value,
            serde_json::json!({"kind": "user_prompt", "text": "hello"})
        );
        let parsed: EventPayload =
            serde_json::from_value(serde_json::json!({"kind": "user_prompt", "text": "hello"}))
                .unwrap();
        assert_eq!(parsed, anonymous);
    }

    #[test]
    fn agent_spawned_wire_format_with_and_without_name() {
        let named = EventPayload::AgentSpawned {
            parent: "agent-0".into(),
            agent: "agent-1".into(),
            name: Some("refactor tests".into()),
            prompt: "fix them".into(),
        };
        let value = serde_json::to_value(&named).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "kind": "agent_spawned",
                "parent": "agent-0",
                "agent": "agent-1",
                "name": "refactor tests",
                "prompt": "fix them",
            })
        );
        let parsed: EventPayload = serde_json::from_value(value).unwrap();
        assert_eq!(parsed, named);

        let unnamed_json = serde_json::json!({
            "kind": "agent_spawned",
            "parent": "agent-0",
            "agent": "agent-1",
            "prompt": "fix them",
        });
        let unnamed = EventPayload::AgentSpawned {
            parent: "agent-0".into(),
            agent: "agent-1".into(),
            name: None,
            prompt: "fix them".into(),
        };
        assert_eq!(serde_json::to_value(&unnamed).unwrap(), unnamed_json);
        let parsed: EventPayload = serde_json::from_value(unnamed_json).unwrap();
        assert_eq!(parsed, unnamed);
    }

    #[test]
    fn interrupted_payload_wire_format() {
        let payload = EventPayload::Interrupted {
            agent: "agent-0".into(),
        };
        assert_eq!(payload.kind(), "interrupted");
        let value = serde_json::to_value(&payload).unwrap();
        assert_eq!(
            value,
            serde_json::json!({"kind": "interrupted", "agent": "agent-0"})
        );
        let parsed: EventPayload = serde_json::from_value(value).unwrap();
        assert_eq!(parsed, payload);
    }
}
