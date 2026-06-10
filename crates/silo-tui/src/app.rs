//! Client-side state: the transcript model, the pending question, popups,
//! the input line, connection status, and cost tracking. All methods are
//! pure state transitions; messages to send to the server are returned to
//! the caller rather than sent directly, so the whole module is unit
//! testable without a network.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use silo_core::cost::UsageSnapshot;
use silo_core::event::{Event, EventPayload, UserQuestion};
use silo_core::helper::b64;
use silo_core::protocol::{ClientMessage, CostEntry, ServerMessage};
use silo_core::sandbox::AccessReport;

use crate::commands::{self, SlashCommand};
use crate::net::NetEvent;

/// Semantic category of one transcript entry; the renderer maps each kind
/// to a color.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemKind {
    /// User prompt, bold cyan.
    Prompt,
    /// Assistant text, white.
    Assistant,
    /// Tool invocation one-liner, dim yellow.
    ToolUse,
    /// Tool result excerpt, dim.
    ToolResult,
    /// Subagent lifecycle note, magenta.
    AgentNote,
    /// Question shown to the user, blue.
    Question,
    /// Recorded answer to a question, blue.
    Answer,
    /// File upload or download note, green.
    FileNote,
    /// Harness lifecycle and informational notes, dim.
    System,
    /// Errors, red.
    Error,
    /// Shutdown notice, bold red.
    Shutdown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TranscriptItem {
    pub kind: ItemKind,
    pub text: String,
}

impl TranscriptItem {
    fn new(kind: ItemKind, text: impl Into<String>) -> Self {
        TranscriptItem {
            kind,
            text: text.into(),
        }
    }
}

const TOOL_RESULT_MAX_LINES: usize = 4;
const ONE_LINER_MAX_CHARS: usize = 160;

/// Truncates to at most `max` characters, appending an ellipsis when cut.
fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Flattens newlines and truncates, for one-line summaries.
fn one_liner(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_chars(&flat, ONE_LINER_MAX_CHARS)
}

/// Keeps the first few lines of a block of text, noting how many were cut.
fn truncate_block(text: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= max_lines {
        return lines
            .iter()
            .map(|l| truncate_chars(l, ONE_LINER_MAX_CHARS))
            .collect::<Vec<_>>()
            .join("\n");
    }
    let mut kept: Vec<String> = lines[..max_lines]
        .iter()
        .map(|l| truncate_chars(l, ONE_LINER_MAX_CHARS))
        .collect();
    kept.push(format!("… (+{} more lines)", lines.len() - max_lines));
    kept.join("\n")
}

/// Compact summary of a tool input for the one-line tool-use display.
/// Prefers the most informative field when the input is an object.
pub fn compact_input_summary(input: &serde_json::Value) -> String {
    const PREFERRED_KEYS: &[&str] = &[
        "command",
        "path",
        "file_path",
        "url",
        "query",
        "prompt",
        "question",
        "name",
    ];
    if let serde_json::Value::Object(map) = input {
        for key in PREFERRED_KEYS {
            if let Some(serde_json::Value::String(value)) = map.get(*key) {
                return one_liner(&format!("{key}: {value}"));
            }
        }
        if map.is_empty() {
            return String::new();
        }
    }
    one_liner(&input.to_string())
}

fn is_top_level(agent: &str) -> bool {
    agent == "agent-0"
}

/// Indentation and label applied to lines from subagents.
fn agent_prefix(agent: &str) -> String {
    if is_top_level(agent) {
        String::new()
    } else {
        format!("  [{agent}] ")
    }
}

/// Approximate decoded size of a base64 payload, without decoding it.
fn b64_decoded_len(content_b64: &str) -> usize {
    let trimmed = content_b64.trim_end_matches('=');
    trimmed.len() * 3 / 4
}

/// Maps one event payload to zero or more transcript entries. Cost reports
/// and idle markers feed the status bar instead and produce nothing here.
pub fn transcript_items(payload: &EventPayload) -> Vec<TranscriptItem> {
    match payload {
        EventPayload::HarnessStarted {
            harness_id,
            workspace,
            sandbox,
            llm,
        } => vec![TranscriptItem::new(
            ItemKind::System,
            format!("harness {harness_id} started · workspace {workspace} · sandbox {sandbox} · llm {llm}"),
        )],
        EventPayload::UserPrompt { client_id, text } => {
            let client = client_id.as_deref().unwrap_or("client");
            vec![TranscriptItem::new(
                ItemKind::Prompt,
                format!("{client} > {text}"),
            )]
        }
        EventPayload::AssistantText { agent, text } => vec![TranscriptItem::new(
            ItemKind::Assistant,
            format!("{}{text}", agent_prefix(agent)),
        )],
        EventPayload::ToolUse { agent, call } => {
            let summary = compact_input_summary(&call.input);
            let text = if summary.is_empty() {
                format!("{}{}", agent_prefix(agent), call.name)
            } else {
                format!("{}{} {summary}", agent_prefix(agent), call.name)
            };
            vec![TranscriptItem::new(ItemKind::ToolUse, text)]
        }
        EventPayload::ToolResult {
            agent,
            tool_name,
            output,
            ..
        } => {
            let body = truncate_block(&output.content, TOOL_RESULT_MAX_LINES);
            let text = if output.is_error {
                format!("{}{} error: {}", agent_prefix(agent), tool_name, body)
            } else {
                format!("{}{} -> {}", agent_prefix(agent), tool_name, body)
            };
            vec![TranscriptItem::new(ItemKind::ToolResult, text)]
        }
        EventPayload::AgentSpawned {
            parent,
            agent,
            prompt,
        } => vec![TranscriptItem::new(
            ItemKind::AgentNote,
            format!("  [{agent}] spawned by {parent}: {}", one_liner(prompt)),
        )],
        EventPayload::AgentCompleted {
            agent,
            result,
            is_error,
        } => {
            let verb = if *is_error { "failed" } else { "completed" };
            vec![TranscriptItem::new(
                ItemKind::AgentNote,
                format!("  [{agent}] {verb}: {}", one_liner(result)),
            )]
        }
        EventPayload::QuestionAsked {
            agent, question, ..
        } => {
            let mut text = format!("{}? {}", agent_prefix(agent), question.question);
            if !question.options.is_empty() {
                let labels: Vec<&str> =
                    question.options.iter().map(|o| o.label.as_str()).collect();
                text.push_str(&format!(" [{}]", labels.join(" / ")));
            }
            vec![TranscriptItem::new(ItemKind::Question, text)]
        }
        EventPayload::QuestionAnswered {
            client_id, answer, ..
        } => {
            let client = client_id.as_deref().unwrap_or("client");
            vec![TranscriptItem::new(
                ItemKind::Answer,
                format!("answered by {client}: {answer}"),
            )]
        }
        EventPayload::FileShared {
            name,
            content_b64,
            origin,
        } => {
            let bytes = b64_decoded_len(content_b64);
            let text = match origin {
                silo_core::event::FileOrigin::Client { client_id } => {
                    format!("file {name} ({bytes} bytes) uploaded by {client_id}")
                }
                silo_core::event::FileOrigin::Llm { agent } => {
                    format!("file {name} ({bytes} bytes) sent by {agent}")
                }
            };
            vec![TranscriptItem::new(ItemKind::FileNote, text)]
        }
        EventPayload::CostReport { .. } => vec![],
        EventPayload::TurnComplete { .. } => vec![],
        EventPayload::AwaitingInput => vec![],
        EventPayload::AccessReportUpdated { .. } => vec![TranscriptItem::new(
            ItemKind::System,
            "access report updated (/access to view)",
        )],
        EventPayload::Error { context, message } => vec![TranscriptItem::new(
            ItemKind::Error,
            format!("{context}: {message}"),
        )],
        EventPayload::Shutdown { message } => {
            let text = match message {
                Some(m) => format!("harness shut down: {m}"),
                None => "harness shut down".to_string(),
            };
            vec![TranscriptItem::new(ItemKind::Shutdown, text)]
        }
    }
}

/// Formats a number of tokens for the status bar, e.g. "45.6k".
pub fn format_tokens(tokens: u64) -> String {
    if tokens < 1_000 {
        format!("{tokens}")
    } else if tokens < 1_000_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    }
}

/// Status-bar cost summary: the latest cost report per backend, summed.
pub fn format_cost_summary<'a, I>(snapshots: I) -> String
where
    I: IntoIterator<Item = &'a UsageSnapshot>,
{
    let mut usd = 0.0;
    let mut tokens = 0u64;
    for snap in snapshots {
        usd += snap.usd;
        tokens += snap.total_tokens();
    }
    format!("${usd:.4} | {} tok", format_tokens(tokens))
}

#[derive(Clone, Debug, PartialEq)]
pub enum ConnState {
    Connecting { attempt: u32 },
    Connected,
    Reconnecting { reason: String, retry_in_secs: u64 },
    Closed { reason: String },
}

#[derive(Clone, Debug, PartialEq)]
pub struct PendingQuestion {
    pub id: String,
    pub agent: String,
    pub question: UserQuestion,
    pub selected: usize,
    /// Toggled options when the question is multi-select.
    pub checked: BTreeSet<usize>,
    /// Free-text answer buffer; `Some` while the user is typing an answer.
    pub free_text: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Popup {
    Access(AccessReport),
    Cost(Vec<CostEntry>),
    Pairing {
        code: String,
        expires_in_secs: u64,
        addr: String,
        fingerprint: String,
    },
}

pub struct App {
    pub harness_id: String,
    /// Server address, shown in the pairing popup.
    pub addr: String,
    /// Pinned certificate fingerprint, shown in the pairing popup.
    pub fingerprint: String,
    pub conn: ConnState,
    pub transcript: Vec<TranscriptItem>,
    pub last_seq: Option<u64>,
    pub awaiting_input: bool,
    /// Latest usage snapshot per backend.
    pub costs: BTreeMap<String, UsageSnapshot>,
    pub input: String,
    /// Cursor position in the input line, as a character index.
    pub cursor: usize,
    pub question: Option<PendingQuestion>,
    pub popup: Option<Popup>,
    /// Scroll offset in display lines, measured up from the bottom;
    /// zero follows the newest output.
    pub scroll_from_bottom: u16,
    pub should_quit: bool,
    /// Set when the connection failed unrecoverably; reported after the
    /// terminal is restored.
    pub fatal: Option<String>,
}

impl App {
    pub fn new(harness_id: String, addr: String, fingerprint: String) -> Self {
        App {
            harness_id,
            addr,
            fingerprint,
            conn: ConnState::Connecting { attempt: 0 },
            transcript: Vec::new(),
            last_seq: None,
            awaiting_input: false,
            costs: BTreeMap::new(),
            input: String::new(),
            cursor: 0,
            question: None,
            popup: None,
            scroll_from_bottom: 0,
            should_quit: false,
            fatal: None,
        }
    }

    fn push(&mut self, kind: ItemKind, text: impl Into<String>) {
        self.transcript.push(TranscriptItem::new(kind, text));
    }

    pub fn handle_net(&mut self, event: NetEvent) {
        match event {
            NetEvent::Connecting { attempt } => {
                self.conn = ConnState::Connecting { attempt };
            }
            NetEvent::Connected { harness_id } => {
                self.conn = ConnState::Connected;
                self.harness_id = harness_id;
            }
            NetEvent::Disconnected {
                reason,
                retry_in_secs,
            } => {
                self.conn = ConnState::Reconnecting {
                    reason,
                    retry_in_secs,
                };
            }
            NetEvent::Fatal { message } => {
                self.fatal = Some(message);
                self.should_quit = true;
            }
            NetEvent::Server(message) => self.handle_server(message),
        }
    }

    pub fn handle_server(&mut self, message: ServerMessage) {
        match message {
            ServerMessage::Event { event } => self.apply_event(event),
            ServerMessage::Events { events } => {
                for event in events {
                    self.apply_event(event);
                }
            }
            ServerMessage::AccessReport { report } => {
                self.popup = Some(Popup::Access(report));
            }
            ServerMessage::Cost { entries } => {
                for entry in &entries {
                    self.costs.insert(entry.backend.clone(), entry.usage);
                }
                self.popup = Some(Popup::Cost(entries));
            }
            ServerMessage::PairingCode {
                code,
                expires_in_secs,
            } => {
                self.popup = Some(Popup::Pairing {
                    code,
                    expires_in_secs,
                    addr: self.addr.clone(),
                    fingerprint: self.fingerprint.clone(),
                });
            }
            ServerMessage::Error { message } => {
                self.push(ItemKind::Error, format!("server: {message}"));
            }
            ServerMessage::ShuttingDown { message } => {
                let text = match message {
                    Some(m) => format!("harness shutting down: {m}"),
                    None => "harness shutting down".to_string(),
                };
                self.push(ItemKind::Shutdown, text);
                self.conn = ConnState::Closed {
                    reason: "harness shut down".into(),
                };
                self.question = None;
            }
            // Hello and the authentication exchange are consumed by the
            // connection task; a heartbeat reply needs no handling.
            ServerMessage::Hello { .. }
            | ServerMessage::AuthChallenge { .. }
            | ServerMessage::AuthOk { .. }
            | ServerMessage::AuthError { .. }
            | ServerMessage::Pong { .. } => {}
        }
    }

    /// Applies one event from the shared stream. Events at or below the
    /// last seen sequence number are duplicates from a catch-up overlap and
    /// are skipped.
    pub fn apply_event(&mut self, event: Event) {
        if let Some(last) = self.last_seq {
            if event.seq <= last {
                return;
            }
        }
        self.last_seq = Some(event.seq);
        match &event.payload {
            EventPayload::QuestionAsked {
                id,
                agent,
                question,
            } => {
                let free_text = if question.options.is_empty() {
                    Some(String::new())
                } else {
                    None
                };
                self.question = Some(PendingQuestion {
                    id: id.clone(),
                    agent: agent.clone(),
                    question: question.clone(),
                    selected: 0,
                    checked: BTreeSet::new(),
                    free_text,
                });
            }
            EventPayload::QuestionAnswered { id, .. } => {
                if self.question.as_ref().is_some_and(|q| &q.id == id) {
                    self.question = None;
                }
            }
            EventPayload::CostReport { backend, usage, .. } => {
                self.costs.insert(backend.clone(), *usage);
            }
            EventPayload::AwaitingInput => {
                self.awaiting_input = true;
            }
            EventPayload::UserPrompt { .. } => {
                self.awaiting_input = false;
            }
            _ => {}
        }
        self.transcript.extend(transcript_items(&event.payload));
    }

    /// Handles one key press. Returns the messages to send to the server.
    pub fn handle_key(&mut self, key: KeyEvent) -> Vec<ClientMessage> {
        if key.kind != KeyEventKind::Press {
            return vec![];
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return vec![];
        }
        if self.popup.is_some() {
            self.popup = None;
            return vec![];
        }
        if self.question.is_some() {
            return self.handle_question_key(key);
        }
        match key.code {
            KeyCode::PageUp => {
                let limit = self.scroll_limit();
                self.scroll_from_bottom = (self.scroll_from_bottom + 5).min(limit);
                vec![]
            }
            KeyCode::PageDown => {
                self.scroll_from_bottom = self.scroll_from_bottom.saturating_sub(5);
                vec![]
            }
            KeyCode::End => {
                self.scroll_from_bottom = 0;
                vec![]
            }
            KeyCode::Enter => self.submit_input(),
            KeyCode::Left => {
                self.cursor = self.cursor.saturating_sub(1);
                vec![]
            }
            KeyCode::Right => {
                self.cursor = (self.cursor + 1).min(self.input.chars().count());
                vec![]
            }
            KeyCode::Home => {
                self.cursor = 0;
                vec![]
            }
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    let byte = byte_index(&self.input, self.cursor - 1);
                    self.input.remove(byte);
                    self.cursor -= 1;
                }
                vec![]
            }
            KeyCode::Delete => {
                if self.cursor < self.input.chars().count() {
                    let byte = byte_index(&self.input, self.cursor);
                    self.input.remove(byte);
                }
                vec![]
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                let byte = byte_index(&self.input, self.cursor);
                self.input.insert(byte, c);
                self.cursor += 1;
                vec![]
            }
            _ => vec![],
        }
    }

    /// Upper bound for scrolling back; an overestimate is harmless because
    /// the renderer clamps the effective offset.
    fn scroll_limit(&self) -> u16 {
        let lines: usize = self
            .transcript
            .iter()
            .map(|item| item.text.lines().count().max(1) + 1)
            .sum();
        lines.min(u16::MAX as usize) as u16
    }

    fn handle_question_key(&mut self, key: KeyEvent) -> Vec<ClientMessage> {
        let Some(pending) = self.question.as_mut() else {
            return vec![];
        };
        if let Some(buffer) = pending.free_text.as_mut() {
            match key.code {
                KeyCode::Enter => {
                    let answer = buffer.trim().to_string();
                    if answer.is_empty() {
                        return vec![];
                    }
                    let id = pending.id.clone();
                    self.question = None;
                    return vec![ClientMessage::AnswerQuestion {
                        question_id: id,
                        answer,
                    }];
                }
                KeyCode::Backspace => {
                    buffer.pop();
                }
                KeyCode::Esc => {
                    if !pending.question.options.is_empty() {
                        pending.free_text = None;
                    }
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    buffer.push(c);
                }
                _ => {}
            }
            return vec![];
        }
        let option_count = pending.question.options.len();
        match key.code {
            KeyCode::Up => {
                pending.selected = pending.selected.saturating_sub(1);
            }
            KeyCode::Down => {
                if option_count > 0 {
                    pending.selected = (pending.selected + 1).min(option_count - 1);
                }
            }
            KeyCode::Char(' ') if pending.question.multi_select => {
                if pending.checked.contains(&pending.selected) {
                    pending.checked.remove(&pending.selected);
                } else {
                    pending.checked.insert(pending.selected);
                }
            }
            KeyCode::Enter => {
                let answer = if pending.question.multi_select && !pending.checked.is_empty() {
                    pending
                        .checked
                        .iter()
                        .filter_map(|i| pending.question.options.get(*i))
                        .map(|o| o.label.clone())
                        .collect::<Vec<_>>()
                        .join(", ")
                } else {
                    match pending.question.options.get(pending.selected) {
                        Some(option) => option.label.clone(),
                        None => return vec![],
                    }
                };
                let id = pending.id.clone();
                self.question = None;
                return vec![ClientMessage::AnswerQuestion {
                    question_id: id,
                    answer,
                }];
            }
            KeyCode::Char(c)
                if pending.question.allow_free_text
                    && !key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                pending.free_text = Some(c.to_string());
            }
            _ => {}
        }
        vec![]
    }

    /// Submits the input line: either a prompt or a slash command. The
    /// input is cleared on success and kept on error so it can be fixed.
    fn submit_input(&mut self) -> Vec<ClientMessage> {
        let text = self.input.trim().to_string();
        if text.is_empty() {
            return vec![];
        }
        match commands::parse(&text) {
            None => {
                self.clear_input();
                vec![ClientMessage::Prompt { text }]
            }
            Some(Err(message)) => {
                self.push(ItemKind::Error, message);
                vec![]
            }
            Some(Ok(command)) => self.run_command(command),
        }
    }

    fn run_command(&mut self, command: SlashCommand) -> Vec<ClientMessage> {
        match command {
            SlashCommand::Access => {
                self.clear_input();
                vec![ClientMessage::RequestAccessReport]
            }
            SlashCommand::Cost => {
                self.clear_input();
                vec![ClientMessage::RequestCost]
            }
            SlashCommand::Pair => {
                self.clear_input();
                vec![ClientMessage::RequestPairingCode]
            }
            SlashCommand::Quit => {
                self.should_quit = true;
                vec![]
            }
            SlashCommand::Shutdown => {
                self.clear_input();
                self.push(ItemKind::System, "shutdown requested");
                vec![ClientMessage::Shutdown]
            }
            SlashCommand::Upload { path } => match self.read_upload(Path::new(&path)) {
                Ok(message) => {
                    self.clear_input();
                    vec![message]
                }
                Err(error) => {
                    self.push(ItemKind::Error, format!("upload {path}: {error}"));
                    vec![]
                }
            },
        }
    }

    fn read_upload(&mut self, path: &Path) -> Result<ClientMessage, String> {
        let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("upload")
            .to_string();
        self.push(
            ItemKind::System,
            format!("uploading {name} ({} bytes)", bytes.len()),
        );
        Ok(ClientMessage::UploadFile {
            name,
            content_b64: b64(&bytes),
        })
    }

    fn clear_input(&mut self) {
        self.input.clear();
        self.cursor = 0;
    }
}

/// Byte offset of the `char_index`-th character of `text`.
fn byte_index(text: &str, char_index: usize) -> usize {
    text.char_indices()
        .nth(char_index)
        .map(|(i, _)| i)
        .unwrap_or(text.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use silo_core::clock::Timestamp;
    use silo_core::cost::QuotaConfig;
    use silo_core::event::{FileOrigin, QuestionOption};
    use silo_core::tool::{ToolCall, ToolOutput};

    fn event(seq: u64, payload: EventPayload) -> Event {
        Event {
            seq,
            time: Timestamp {
                logical: seq,
                wall_ms: None,
            },
            payload,
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn app() -> App {
        App::new("h1".into(), "127.0.0.1:7777".into(), "ab".repeat(32))
    }

    fn question(options: &[&str], multi: bool, free: bool) -> UserQuestion {
        UserQuestion {
            question: "Pick one".into(),
            options: options
                .iter()
                .map(|label| QuestionOption {
                    label: label.to_string(),
                    description: String::new(),
                })
                .collect(),
            multi_select: multi,
            allow_free_text: free,
        }
    }

    // --- transcript mapping, one test per payload variant ---

    #[test]
    fn maps_harness_started() {
        let items = transcript_items(&EventPayload::HarnessStarted {
            harness_id: "h1".into(),
            workspace: "/ws".into(),
            sandbox: "mock".into(),
            llm: "mock".into(),
        });
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, ItemKind::System);
        assert!(items[0].text.contains("h1"));
        assert!(items[0].text.contains("/ws"));
    }

    #[test]
    fn maps_user_prompt_with_client_prefix() {
        let items = transcript_items(&EventPayload::UserPrompt {
            client_id: Some("client-3".into()),
            text: "do the thing".into(),
        });
        assert_eq!(
            items,
            vec![TranscriptItem::new(
                ItemKind::Prompt,
                "client-3 > do the thing"
            )]
        );

        let anonymous = transcript_items(&EventPayload::UserPrompt {
            client_id: None,
            text: "hi".into(),
        });
        assert_eq!(anonymous[0].text, "client > hi");
    }

    #[test]
    fn maps_assistant_text_with_subagent_indent() {
        let top = transcript_items(&EventPayload::AssistantText {
            agent: "agent-0".into(),
            text: "hello".into(),
        });
        assert_eq!(top[0].kind, ItemKind::Assistant);
        assert_eq!(top[0].text, "hello");

        let sub = transcript_items(&EventPayload::AssistantText {
            agent: "agent-2".into(),
            text: "working".into(),
        });
        assert_eq!(sub[0].text, "  [agent-2] working");
    }

    #[test]
    fn maps_tool_use_to_compact_one_liner() {
        let items = transcript_items(&EventPayload::ToolUse {
            agent: "agent-0".into(),
            call: ToolCall {
                id: "t1".into(),
                name: "Bash".into(),
                input: serde_json::json!({"command": "ls -la", "timeout_ms": 500}),
            },
        });
        assert_eq!(items[0].kind, ItemKind::ToolUse);
        assert_eq!(items[0].text, "Bash command: ls -la");
    }

    #[test]
    fn maps_tool_result_truncated() {
        let long = (0..10)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let items = transcript_items(&EventPayload::ToolResult {
            agent: "agent-0".into(),
            tool_use_id: "t1".into(),
            tool_name: "Bash".into(),
            output: ToolOutput::ok(long),
        });
        assert_eq!(items[0].kind, ItemKind::ToolResult);
        assert!(items[0].text.contains("line0"));
        assert!(items[0].text.contains("(+6 more lines)"));
        assert!(!items[0].text.contains("line9"));
    }

    #[test]
    fn maps_tool_result_error() {
        let items = transcript_items(&EventPayload::ToolResult {
            agent: "agent-0".into(),
            tool_use_id: "t1".into(),
            tool_name: "Read".into(),
            output: ToolOutput::error("no such file"),
        });
        assert!(items[0].text.contains("Read error: no such file"));
    }

    #[test]
    fn maps_agent_spawned_and_completed() {
        let spawned = transcript_items(&EventPayload::AgentSpawned {
            parent: "agent-0".into(),
            agent: "agent-1".into(),
            prompt: "explore the\ncodebase".into(),
        });
        assert_eq!(spawned[0].kind, ItemKind::AgentNote);
        assert_eq!(
            spawned[0].text,
            "  [agent-1] spawned by agent-0: explore the codebase"
        );

        let completed = transcript_items(&EventPayload::AgentCompleted {
            agent: "agent-1".into(),
            result: "done".into(),
            is_error: false,
        });
        assert_eq!(completed[0].text, "  [agent-1] completed: done");

        let failed = transcript_items(&EventPayload::AgentCompleted {
            agent: "agent-1".into(),
            result: "oops".into(),
            is_error: true,
        });
        assert!(failed[0].text.contains("failed: oops"));
    }

    #[test]
    fn maps_question_asked_and_answered() {
        let asked = transcript_items(&EventPayload::QuestionAsked {
            id: "q1".into(),
            agent: "agent-0".into(),
            question: question(&["yes", "no"], false, false),
        });
        assert_eq!(asked[0].kind, ItemKind::Question);
        assert!(asked[0].text.contains("Pick one"));
        assert!(asked[0].text.contains("[yes / no]"));

        let answered = transcript_items(&EventPayload::QuestionAnswered {
            id: "q1".into(),
            client_id: Some("client-9".into()),
            answer: "yes".into(),
        });
        assert_eq!(answered[0].kind, ItemKind::Answer);
        assert_eq!(answered[0].text, "answered by client-9: yes");
    }

    #[test]
    fn maps_file_shared_from_client_and_llm() {
        let upload = transcript_items(&EventPayload::FileShared {
            name: "a.txt".into(),
            content_b64: b64(b"12345"),
            origin: FileOrigin::Client {
                client_id: "client-1".into(),
            },
        });
        assert_eq!(upload[0].kind, ItemKind::FileNote);
        assert_eq!(upload[0].text, "file a.txt (5 bytes) uploaded by client-1");

        let sent = transcript_items(&EventPayload::FileShared {
            name: "b.bin".into(),
            content_b64: b64(b"123"),
            origin: FileOrigin::Llm {
                agent: "agent-0".into(),
            },
        });
        assert_eq!(sent[0].text, "file b.bin (3 bytes) sent by agent-0");
    }

    #[test]
    fn cost_turn_complete_and_awaiting_produce_no_transcript() {
        assert!(transcript_items(&EventPayload::CostReport {
            backend: "mock".into(),
            usage: UsageSnapshot::default(),
            quota: QuotaConfig::default(),
        })
        .is_empty());
        assert!(transcript_items(&EventPayload::TurnComplete {
            agent: "agent-0".into(),
            stop_reason: silo_core::conversation::StopReason::EndTurn,
        })
        .is_empty());
        assert!(transcript_items(&EventPayload::AwaitingInput).is_empty());
    }

    #[test]
    fn maps_access_report_updated_error_and_shutdown() {
        let access = transcript_items(&EventPayload::AccessReportUpdated {
            report: AccessReport::default(),
        });
        assert_eq!(access[0].kind, ItemKind::System);

        let error = transcript_items(&EventPayload::Error {
            context: "llm".into(),
            message: "boom".into(),
        });
        assert_eq!(error[0].kind, ItemKind::Error);
        assert_eq!(error[0].text, "llm: boom");

        let shutdown = transcript_items(&EventPayload::Shutdown {
            message: Some("bye".into()),
        });
        assert_eq!(shutdown[0].kind, ItemKind::Shutdown);
        assert_eq!(shutdown[0].text, "harness shut down: bye");
        let silent = transcript_items(&EventPayload::Shutdown { message: None });
        assert_eq!(silent[0].text, "harness shut down");
    }

    // --- cost formatting ---

    #[test]
    fn formats_token_counts() {
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(45_600), "45.6k");
        assert_eq!(format_tokens(1_200_000), "1.2M");
    }

    #[test]
    fn cost_summary_sums_latest_per_backend() {
        let mut app = app();
        app.apply_event(event(
            0,
            EventPayload::CostReport {
                backend: "anthropic".into(),
                usage: UsageSnapshot {
                    input_tokens: 10_000,
                    output_tokens: 5_000,
                    usd: 0.5,
                },
                quota: QuotaConfig::default(),
            },
        ));
        // A later report for the same backend replaces the earlier one.
        app.apply_event(event(
            1,
            EventPayload::CostReport {
                backend: "anthropic".into(),
                usage: UsageSnapshot {
                    input_tokens: 30_000,
                    output_tokens: 15_600,
                    usd: 0.01,
                },
                quota: QuotaConfig::default(),
            },
        ));
        app.apply_event(event(
            2,
            EventPayload::CostReport {
                backend: "openai".into(),
                usage: UsageSnapshot {
                    input_tokens: 0,
                    output_tokens: 0,
                    usd: 0.0023,
                },
                quota: QuotaConfig::default(),
            },
        ));
        let summary = format_cost_summary(app.costs.values());
        assert_eq!(summary, "$0.0123 | 45.6k tok");
    }

    #[test]
    fn empty_cost_summary_is_zero() {
        assert_eq!(format_cost_summary(std::iter::empty()), "$0.0000 | 0 tok");
    }

    // --- event application ---

    #[test]
    fn duplicate_and_stale_events_are_skipped() {
        let mut app = app();
        app.apply_event(event(0, EventPayload::AwaitingInput));
        app.apply_event(event(
            1,
            EventPayload::UserPrompt {
                client_id: None,
                text: "hi".into(),
            },
        ));
        let before = app.transcript.len();
        app.apply_event(event(
            1,
            EventPayload::UserPrompt {
                client_id: None,
                text: "hi".into(),
            },
        ));
        app.apply_event(event(
            0,
            EventPayload::UserPrompt {
                client_id: None,
                text: "older".into(),
            },
        ));
        assert_eq!(app.transcript.len(), before);
        assert_eq!(app.last_seq, Some(1));
    }

    #[test]
    fn awaiting_input_toggles_with_prompts() {
        let mut app = app();
        app.apply_event(event(0, EventPayload::AwaitingInput));
        assert!(app.awaiting_input);
        app.apply_event(event(
            1,
            EventPayload::UserPrompt {
                client_id: None,
                text: "go".into(),
            },
        ));
        assert!(!app.awaiting_input);
    }

    #[test]
    fn question_opens_and_closes_via_events() {
        let mut app = app();
        app.apply_event(event(
            0,
            EventPayload::QuestionAsked {
                id: "q1".into(),
                agent: "agent-0".into(),
                question: question(&["a", "b"], false, false),
            },
        ));
        assert!(app.question.is_some());
        // An answer for a different question leaves the modal open.
        app.apply_event(event(
            1,
            EventPayload::QuestionAnswered {
                id: "other".into(),
                client_id: None,
                answer: "x".into(),
            },
        ));
        assert!(app.question.is_some());
        app.apply_event(event(
            2,
            EventPayload::QuestionAnswered {
                id: "q1".into(),
                client_id: Some("client-2".into()),
                answer: "a".into(),
            },
        ));
        assert!(app.question.is_none());
    }

    #[test]
    fn question_option_navigation_and_answer() {
        let mut app = app();
        app.apply_event(event(
            0,
            EventPayload::QuestionAsked {
                id: "q1".into(),
                agent: "agent-0".into(),
                question: question(&["alpha", "beta"], false, false),
            },
        ));
        assert!(app.handle_key(key(KeyCode::Down)).is_empty());
        let sent = app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            sent,
            vec![ClientMessage::AnswerQuestion {
                question_id: "q1".into(),
                answer: "beta".into(),
            }]
        );
        assert!(app.question.is_none());
    }

    #[test]
    fn multi_select_question_joins_checked_labels() {
        let mut app = app();
        app.apply_event(event(
            0,
            EventPayload::QuestionAsked {
                id: "q1".into(),
                agent: "agent-0".into(),
                question: question(&["a", "b", "c"], true, false),
            },
        ));
        app.handle_key(key(KeyCode::Char(' ')));
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Char(' ')));
        let sent = app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            sent,
            vec![ClientMessage::AnswerQuestion {
                question_id: "q1".into(),
                answer: "a, c".into(),
            }]
        );
    }

    #[test]
    fn free_text_question_switches_on_typing() {
        let mut app = app();
        app.apply_event(event(
            0,
            EventPayload::QuestionAsked {
                id: "q1".into(),
                agent: "agent-0".into(),
                question: question(&["a"], false, true),
            },
        ));
        app.handle_key(key(KeyCode::Char('h')));
        app.handle_key(key(KeyCode::Char('i')));
        let sent = app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            sent,
            vec![ClientMessage::AnswerQuestion {
                question_id: "q1".into(),
                answer: "hi".into(),
            }]
        );
    }

    #[test]
    fn question_without_options_starts_in_free_text() {
        let mut app = app();
        app.apply_event(event(
            0,
            EventPayload::QuestionAsked {
                id: "q1".into(),
                agent: "agent-0".into(),
                question: question(&[], false, true),
            },
        ));
        assert!(app.question.as_ref().unwrap().free_text.is_some());
    }

    // --- input line and commands ---

    #[test]
    fn typing_and_enter_sends_a_prompt() {
        let mut app = app();
        for c in "hello".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        let sent = app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            sent,
            vec![ClientMessage::Prompt {
                text: "hello".into()
            }]
        );
        assert!(app.input.is_empty());
        assert_eq!(app.cursor, 0);
    }

    #[test]
    fn cursor_editing_works_mid_line() {
        let mut app = app();
        for c in "ac".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        app.handle_key(key(KeyCode::Left));
        app.handle_key(key(KeyCode::Char('b')));
        assert_eq!(app.input, "abc");
        app.handle_key(key(KeyCode::Backspace));
        assert_eq!(app.input, "ac");
        app.handle_key(key(KeyCode::Home));
        app.handle_key(key(KeyCode::Delete));
        assert_eq!(app.input, "c");
    }

    #[test]
    fn slash_commands_produce_requests() {
        let mut app = app();
        app.input = "/access".into();
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            vec![ClientMessage::RequestAccessReport]
        );
        app.input = "/cost".into();
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            vec![ClientMessage::RequestCost]
        );
        app.input = "/pair".into();
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            vec![ClientMessage::RequestPairingCode]
        );
        app.input = "/shutdown".into();
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            vec![ClientMessage::Shutdown]
        );
    }

    #[test]
    fn quit_command_sets_the_flag() {
        let mut app = app();
        app.input = "/quit".into();
        assert!(app.handle_key(key(KeyCode::Enter)).is_empty());
        assert!(app.should_quit);
    }

    #[test]
    fn unknown_command_keeps_input_and_logs_error() {
        let mut app = app();
        app.input = "/bogus".into();
        app.cursor = 6;
        assert!(app.handle_key(key(KeyCode::Enter)).is_empty());
        assert_eq!(app.input, "/bogus");
        assert!(matches!(
            app.transcript.last(),
            Some(TranscriptItem {
                kind: ItemKind::Error,
                ..
            })
        ));
    }

    #[test]
    fn upload_reads_the_file_and_encodes_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.txt");
        std::fs::write(&path, b"payload").unwrap();
        let mut app = app();
        app.input = format!("/upload {}", path.display());
        let sent = app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            sent,
            vec![ClientMessage::UploadFile {
                name: "data.txt".into(),
                content_b64: b64(b"payload"),
            }]
        );
    }

    #[test]
    fn upload_of_a_missing_file_is_a_local_error() {
        let mut app = app();
        app.input = "/upload /definitely/not/here.bin".into();
        assert!(app.handle_key(key(KeyCode::Enter)).is_empty());
        assert!(matches!(
            app.transcript.last(),
            Some(TranscriptItem {
                kind: ItemKind::Error,
                ..
            })
        ));
    }

    // --- popups and server messages ---

    #[test]
    fn access_cost_and_pairing_open_popups_and_any_key_closes() {
        let mut app = app();
        app.handle_server(ServerMessage::AccessReport {
            report: AccessReport::default(),
        });
        assert!(matches!(app.popup, Some(Popup::Access(_))));
        app.handle_key(key(KeyCode::Char('x')));
        assert!(app.popup.is_none());

        app.handle_server(ServerMessage::Cost { entries: vec![] });
        assert!(matches!(app.popup, Some(Popup::Cost(_))));
        app.handle_key(key(KeyCode::Enter));
        assert!(app.popup.is_none());

        app.handle_server(ServerMessage::PairingCode {
            code: "ABCD1234".into(),
            expires_in_secs: 120,
        });
        match &app.popup {
            Some(Popup::Pairing {
                code,
                addr,
                fingerprint,
                ..
            }) => {
                assert_eq!(code, "ABCD1234");
                assert_eq!(addr, "127.0.0.1:7777");
                assert_eq!(fingerprint.len(), 64);
            }
            other => panic!("expected pairing popup, got {other:?}"),
        }
    }

    #[test]
    fn cost_message_updates_the_status_totals() {
        let mut app = app();
        app.handle_server(ServerMessage::Cost {
            entries: vec![CostEntry {
                backend: "anthropic".into(),
                usage: UsageSnapshot {
                    input_tokens: 5,
                    output_tokens: 5,
                    usd: 1.0,
                },
                quota: QuotaConfig::default(),
            }],
        });
        assert_eq!(app.costs.len(), 1);
    }

    #[test]
    fn shutting_down_closes_the_connection_state() {
        let mut app = app();
        app.handle_server(ServerMessage::ShuttingDown {
            message: Some("done".into()),
        });
        assert!(matches!(app.conn, ConnState::Closed { .. }));
        assert!(matches!(
            app.transcript.last(),
            Some(TranscriptItem {
                kind: ItemKind::Shutdown,
                ..
            })
        ));
    }

    #[test]
    fn net_events_update_connection_state() {
        let mut app = app();
        app.handle_net(NetEvent::Connected {
            harness_id: "real-id".into(),
        });
        assert_eq!(app.conn, ConnState::Connected);
        assert_eq!(app.harness_id, "real-id");
        app.handle_net(NetEvent::Disconnected {
            reason: "lost".into(),
            retry_in_secs: 2,
        });
        assert!(matches!(app.conn, ConnState::Reconnecting { .. }));
        app.handle_net(NetEvent::Fatal {
            message: "auth failed".into(),
        });
        assert!(app.should_quit);
        assert_eq!(app.fatal.as_deref(), Some("auth failed"));
    }

    #[test]
    fn ctrl_c_quits() {
        let mut app = app();
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.should_quit);
    }

    #[test]
    fn compact_summary_prefers_known_keys() {
        assert_eq!(
            compact_input_summary(&serde_json::json!({"command": "ls"})),
            "command: ls"
        );
        assert_eq!(
            compact_input_summary(&serde_json::json!({"path": "/a", "limit": 5})),
            "path: /a"
        );
        assert_eq!(compact_input_summary(&serde_json::json!({})), "");
        let generic = compact_input_summary(&serde_json::json!({"k": 1}));
        assert!(generic.contains("\"k\":1"));
    }
}
