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
    /// Interrupt notice, dim red.
    Interrupted,
    /// Errors, red.
    Error,
    /// Shutdown notice, bold red.
    Shutdown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TranscriptItem {
    pub kind: ItemKind,
    pub text: String,
    /// Raw ids behind this line (agent ids, tool_use ids, client ids).
    /// Rendered in dim brackets when debug mode is on.
    pub debug: Option<String>,
}

impl TranscriptItem {
    fn new(kind: ItemKind, text: impl Into<String>) -> Self {
        TranscriptItem {
            kind,
            text: text.into(),
            debug: None,
        }
    }

    fn with_debug(kind: ItemKind, text: impl Into<String>, debug: impl Into<String>) -> Self {
        TranscriptItem {
            kind,
            text: text.into(),
            debug: Some(debug.into()),
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

/// Last path component of a workspace path, used to identify the harness.
pub fn workspace_folder_name(workspace: &str) -> String {
    std::path::Path::new(workspace)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| workspace.to_string())
}

/// Display label for a subagent: "subagent {name}" when the Agent tool
/// gave one, else "subagent {ordinal}" derived from the agent id.
pub fn subagent_label(agent: &str, names: &BTreeMap<String, String>) -> String {
    if let Some(name) = names.get(agent) {
        return format!("subagent {name}");
    }
    match agent.strip_prefix("agent-") {
        Some(ordinal) => format!("subagent {ordinal}"),
        None => format!("subagent {agent}"),
    }
}

/// Indentation and label applied to lines from subagents.
fn agent_prefix(agent: &str, names: &BTreeMap<String, String>) -> String {
    if is_top_level(agent) {
        String::new()
    } else {
        format!("  [{}] ", subagent_label(agent, names))
    }
}

/// Approximate decoded size of a base64 payload, without decoding it.
fn b64_decoded_len(content_b64: &str) -> usize {
    let trimmed = content_b64.trim_end_matches('=');
    trimmed.len() * 3 / 4
}

/// Maps one event payload to zero or more transcript entries. Cost reports
/// and idle markers feed the status bar instead and produce nothing here.
/// `names` maps subagent ids to their given names. Raw ids (agent ids,
/// tool_use ids, client ids) go into the items' debug field, shown only in
/// debug mode.
pub fn transcript_items(
    payload: &EventPayload,
    names: &BTreeMap<String, String>,
) -> Vec<TranscriptItem> {
    match payload {
        EventPayload::HarnessStarted {
            harness_id,
            workspace,
            sandbox,
            llm,
        } => vec![TranscriptItem::with_debug(
            ItemKind::System,
            format!(
                "harness {} started · workspace {workspace} · sandbox {sandbox} · llm {llm}",
                workspace_folder_name(workspace)
            ),
            harness_id.clone(),
        )],
        EventPayload::UserPrompt {
            client_id,
            client_name,
            text,
        } => {
            let line = match client_name {
                Some(name) => format!("{name} > {text}"),
                None => format!("> {text}"),
            };
            vec![match client_id {
                Some(id) => TranscriptItem::with_debug(ItemKind::Prompt, line, id.clone()),
                None => TranscriptItem::new(ItemKind::Prompt, line),
            }]
        }
        EventPayload::AssistantText { agent, text } => vec![TranscriptItem::with_debug(
            ItemKind::Assistant,
            format!("{}{text}", agent_prefix(agent, names)),
            agent.clone(),
        )],
        EventPayload::ToolUse { agent, call } => {
            let summary = compact_input_summary(&call.input);
            let text = if summary.is_empty() {
                format!("{}{}", agent_prefix(agent, names), call.name)
            } else {
                format!("{}{} {summary}", agent_prefix(agent, names), call.name)
            };
            vec![TranscriptItem::with_debug(
                ItemKind::ToolUse,
                text,
                format!("{agent} {}", call.id),
            )]
        }
        EventPayload::ToolResult {
            agent,
            tool_use_id,
            tool_name,
            output,
        } => {
            let body = truncate_block(&output.content, TOOL_RESULT_MAX_LINES);
            let text = if output.is_error {
                format!(
                    "{}{} error: {}",
                    agent_prefix(agent, names),
                    tool_name,
                    body
                )
            } else {
                format!("{}{} -> {}", agent_prefix(agent, names), tool_name, body)
            };
            vec![TranscriptItem::with_debug(
                ItemKind::ToolResult,
                text,
                format!("{agent} {tool_use_id}"),
            )]
        }
        EventPayload::AgentSpawned {
            parent,
            agent,
            name: _,
            prompt,
        } => vec![TranscriptItem::with_debug(
            ItemKind::AgentNote,
            format!(
                "  [{}] spawned: {}",
                subagent_label(agent, names),
                one_liner(prompt)
            ),
            format!("{agent}, parent {parent}"),
        )],
        EventPayload::AgentCompleted {
            agent,
            result,
            is_error,
        } => {
            let verb = if *is_error { "failed" } else { "completed" };
            vec![TranscriptItem::with_debug(
                ItemKind::AgentNote,
                format!(
                    "  [{}] {verb}: {}",
                    subagent_label(agent, names),
                    one_liner(result)
                ),
                agent.clone(),
            )]
        }
        EventPayload::QuestionAsked {
            id,
            agent,
            question,
        } => {
            let mut text = format!("{}? {}", agent_prefix(agent, names), question.question);
            if !question.options.is_empty() {
                let labels: Vec<&str> = question.options.iter().map(|o| o.label.as_str()).collect();
                text.push_str(&format!(" [{}]", labels.join(" / ")));
            }
            vec![TranscriptItem::with_debug(
                ItemKind::Question,
                text,
                format!("{agent} {id}"),
            )]
        }
        EventPayload::QuestionAnswered {
            id,
            client_id,
            answer,
        } => {
            let debug = match client_id {
                Some(client) => format!("{id} by {client}"),
                None => id.clone(),
            };
            vec![TranscriptItem::with_debug(
                ItemKind::Answer,
                format!("answered: {answer}"),
                debug,
            )]
        }
        EventPayload::FileShared {
            name,
            content_b64,
            origin,
        } => {
            let bytes = b64_decoded_len(content_b64);
            let item = match origin {
                silo_core::event::FileOrigin::Client { client_id } => TranscriptItem::with_debug(
                    ItemKind::FileNote,
                    format!("file {name} ({bytes} bytes) uploaded"),
                    client_id.clone(),
                ),
                silo_core::event::FileOrigin::Llm { agent } => {
                    let sender = if is_top_level(agent) {
                        "the assistant".to_string()
                    } else {
                        subagent_label(agent, names)
                    };
                    TranscriptItem::with_debug(
                        ItemKind::FileNote,
                        format!("file {name} ({bytes} bytes) sent by {sender}"),
                        agent.clone(),
                    )
                }
            };
            vec![item]
        }
        EventPayload::CostReport { .. } => vec![],
        EventPayload::TurnComplete { .. } => vec![],
        EventPayload::Interrupted { agent } => vec![TranscriptItem::with_debug(
            ItemKind::Interrupted,
            "■ interrupted by the user",
            agent.clone(),
        )],
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

/// Busy state implied by one event: `Some(true)` for events that show the
/// model working, `Some(false)` for events that return the harness to
/// idle, `None` for events that carry no busy information.
pub fn busy_after(payload: &EventPayload) -> Option<bool> {
    match payload {
        EventPayload::UserPrompt { .. }
        | EventPayload::AssistantText { .. }
        | EventPayload::ToolUse { .. }
        | EventPayload::ToolResult { .. }
        | EventPayload::AgentSpawned { .. }
        | EventPayload::AgentCompleted { .. }
        | EventPayload::QuestionAsked { .. }
        | EventPayload::QuestionAnswered { .. } => Some(true),
        EventPayload::AwaitingInput
        | EventPayload::Interrupted { .. }
        | EventPayload::Shutdown { .. } => Some(false),
        _ => None,
    }
}

/// Spinner frames for the busy indicator; the frame advances on each
/// received event, not on a timer.
const SPINNER_FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

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
    /// Command list; the content renders from `commands::COMMANDS`.
    Help,
}

pub struct App {
    /// Raw harness id, shown only in debug mode.
    pub harness_id: String,
    /// Server address, shown in the pairing popup.
    pub addr: String,
    /// Pinned certificate fingerprint, shown in the pairing popup.
    pub fingerprint: String,
    /// Workspace path, from the run file or the harness_started event.
    pub workspace: Option<String>,
    pub conn: ConnState,
    pub transcript: Vec<TranscriptItem>,
    pub last_seq: Option<u64>,
    pub awaiting_input: bool,
    /// True while the model is working, derived from the event stream.
    pub busy: bool,
    /// Spinner frame index; advances with each applied event.
    pub spinner_frame: usize,
    /// Latest usage snapshot per backend.
    pub costs: BTreeMap<String, UsageSnapshot>,
    /// Subagent names from agent_spawned events, keyed by agent id.
    pub agent_names: BTreeMap<String, String>,
    /// Debug mode: raw ids in the status bar and transcript. Per-session,
    /// never persisted.
    pub debug: bool,
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
    pub fn new(addr: String, fingerprint: String, workspace: Option<String>) -> Self {
        App {
            harness_id: String::new(),
            addr,
            fingerprint,
            workspace,
            conn: ConnState::Connecting { attempt: 0 },
            transcript: Vec::new(),
            last_seq: None,
            awaiting_input: false,
            busy: false,
            spinner_frame: 0,
            costs: BTreeMap::new(),
            agent_names: BTreeMap::new(),
            debug: false,
            input: String::new(),
            cursor: 0,
            question: None,
            popup: None,
            scroll_from_bottom: 0,
            should_quit: false,
            fatal: None,
        }
    }

    /// Label identifying the harness: the workspace folder name when
    /// known, else the server address.
    pub fn harness_label(&self) -> String {
        match &self.workspace {
            Some(workspace) => workspace_folder_name(workspace),
            None => self.addr.clone(),
        }
    }

    fn push(&mut self, kind: ItemKind, text: impl Into<String>) {
        self.transcript.push(TranscriptItem::new(kind, text));
    }

    /// Current spinner character for the busy indicator.
    pub fn spinner_char(&self) -> char {
        SPINNER_FRAMES[self.spinner_frame % SPINNER_FRAMES.len()]
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
                self.busy = false;
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
        self.spinner_frame = self.spinner_frame.wrapping_add(1);
        if let Some(busy) = busy_after(&event.payload) {
            self.busy = busy;
        }
        match &event.payload {
            EventPayload::HarnessStarted {
                harness_id,
                workspace,
                ..
            } => {
                self.harness_id = harness_id.clone();
                self.workspace = Some(workspace.clone());
            }
            EventPayload::AgentSpawned {
                agent,
                name: Some(name),
                ..
            } => {
                self.agent_names.insert(agent.clone(), name.clone());
            }
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
        self.transcript
            .extend(transcript_items(&event.payload, &self.agent_names));
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
            KeyCode::Esc if self.busy => vec![ClientMessage::Interrupt],
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
            SlashCommand::Help => {
                self.clear_input();
                self.popup = Some(Popup::Help);
                vec![]
            }
            SlashCommand::Debug => {
                self.clear_input();
                self.debug = !self.debug;
                let note = if self.debug {
                    "debug mode on: raw ids are shown"
                } else {
                    "debug mode off"
                };
                self.push(ItemKind::System, note);
                vec![]
            }
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
            SlashCommand::Stop => {
                self.clear_input();
                self.push(ItemKind::System, "interrupt requested");
                vec![ClientMessage::Interrupt]
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
        App::new("127.0.0.1:7777".into(), "ab".repeat(32), None)
    }

    /// Transcript mapping with no known subagent names.
    fn items(payload: &EventPayload) -> Vec<TranscriptItem> {
        transcript_items(payload, &BTreeMap::new())
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
    fn maps_harness_started_with_the_workspace_folder_name() {
        let mapped = items(&EventPayload::HarnessStarted {
            harness_id: "deadbeef42".into(),
            workspace: "/home/user/projects/myproject".into(),
            sandbox: "mock".into(),
            llm: "mock".into(),
        });
        assert_eq!(mapped.len(), 1);
        assert_eq!(mapped[0].kind, ItemKind::System);
        assert!(mapped[0].text.contains("harness myproject started"));
        assert!(mapped[0].text.contains("/home/user/projects/myproject"));
        // The hex id only appears in the debug field.
        assert!(!mapped[0].text.contains("deadbeef42"));
        assert_eq!(mapped[0].debug.as_deref(), Some("deadbeef42"));
    }

    #[test]
    fn maps_user_prompt_with_client_name_when_present() {
        let named = items(&EventPayload::UserPrompt {
            client_id: Some("client-3".into()),
            client_name: Some("Ian's phone".into()),
            text: "do the thing".into(),
        });
        assert_eq!(named[0].kind, ItemKind::Prompt);
        assert_eq!(named[0].text, "Ian's phone > do the thing");
        assert!(!named[0].text.contains("client-3"));
        assert_eq!(named[0].debug.as_deref(), Some("client-3"));

        let unnamed = items(&EventPayload::UserPrompt {
            client_id: Some("client-3".into()),
            client_name: None,
            text: "hi".into(),
        });
        assert_eq!(unnamed[0].text, "> hi");

        let anonymous = items(&EventPayload::UserPrompt {
            client_id: None,
            client_name: None,
            text: "hi".into(),
        });
        assert_eq!(anonymous[0].text, "> hi");
        assert_eq!(anonymous[0].debug, None);
    }

    #[test]
    fn maps_assistant_text_with_subagent_indent() {
        let top = items(&EventPayload::AssistantText {
            agent: "agent-0".into(),
            text: "hello".into(),
        });
        assert_eq!(top[0].kind, ItemKind::Assistant);
        assert_eq!(top[0].text, "hello");
        assert_eq!(top[0].debug.as_deref(), Some("agent-0"));

        let sub = items(&EventPayload::AssistantText {
            agent: "agent-2".into(),
            text: "working".into(),
        });
        assert_eq!(sub[0].text, "  [subagent 2] working");

        let mut names = BTreeMap::new();
        names.insert("agent-2".to_string(), "refactor tests".to_string());
        let named = transcript_items(
            &EventPayload::AssistantText {
                agent: "agent-2".into(),
                text: "working".into(),
            },
            &names,
        );
        assert_eq!(named[0].text, "  [subagent refactor tests] working");
    }

    #[test]
    fn maps_tool_use_to_compact_one_liner_without_ids() {
        let mapped = items(&EventPayload::ToolUse {
            agent: "agent-0".into(),
            call: ToolCall {
                id: "toolu_xyz".into(),
                name: "Bash".into(),
                input: serde_json::json!({"command": "ls -la", "timeout_ms": 500}),
            },
        });
        assert_eq!(mapped[0].kind, ItemKind::ToolUse);
        assert_eq!(mapped[0].text, "Bash command: ls -la");
        assert_eq!(mapped[0].debug.as_deref(), Some("agent-0 toolu_xyz"));
    }

    #[test]
    fn maps_tool_result_truncated() {
        let long = (0..10)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mapped = items(&EventPayload::ToolResult {
            agent: "agent-0".into(),
            tool_use_id: "t1".into(),
            tool_name: "Bash".into(),
            output: ToolOutput::ok(long),
        });
        assert_eq!(mapped[0].kind, ItemKind::ToolResult);
        assert!(mapped[0].text.contains("line0"));
        assert!(mapped[0].text.contains("(+6 more lines)"));
        assert!(!mapped[0].text.contains("line9"));
        assert!(!mapped[0].text.contains("t1"));
        assert_eq!(mapped[0].debug.as_deref(), Some("agent-0 t1"));
    }

    #[test]
    fn maps_tool_result_error() {
        let mapped = items(&EventPayload::ToolResult {
            agent: "agent-0".into(),
            tool_use_id: "t1".into(),
            tool_name: "Read".into(),
            output: ToolOutput::error("no such file"),
        });
        assert!(mapped[0].text.contains("Read error: no such file"));
    }

    #[test]
    fn maps_agent_spawned_and_completed_with_names_or_ordinals() {
        let mut names = BTreeMap::new();
        names.insert("agent-1".to_string(), "refactor tests".to_string());

        let spawned = transcript_items(
            &EventPayload::AgentSpawned {
                parent: "agent-0".into(),
                agent: "agent-1".into(),
                name: Some("refactor tests".into()),
                prompt: "explore the\ncodebase".into(),
            },
            &names,
        );
        assert_eq!(spawned[0].kind, ItemKind::AgentNote);
        assert_eq!(
            spawned[0].text,
            "  [subagent refactor tests] spawned: explore the codebase"
        );
        assert_eq!(spawned[0].debug.as_deref(), Some("agent-1, parent agent-0"));

        let completed = transcript_items(
            &EventPayload::AgentCompleted {
                agent: "agent-1".into(),
                result: "done".into(),
                is_error: false,
            },
            &names,
        );
        assert_eq!(
            completed[0].text,
            "  [subagent refactor tests] completed: done"
        );

        // Without a name, the label is a neutral ordinal — never agent-N.
        let unnamed = items(&EventPayload::AgentSpawned {
            parent: "agent-0".into(),
            agent: "agent-2".into(),
            name: None,
            prompt: "look around".into(),
        });
        assert_eq!(unnamed[0].text, "  [subagent 2] spawned: look around");

        let failed = items(&EventPayload::AgentCompleted {
            agent: "agent-2".into(),
            result: "oops".into(),
            is_error: true,
        });
        assert_eq!(failed[0].text, "  [subagent 2] failed: oops");
    }

    #[test]
    fn maps_question_asked_and_answered() {
        let asked = items(&EventPayload::QuestionAsked {
            id: "q1".into(),
            agent: "agent-0".into(),
            question: question(&["yes", "no"], false, false),
        });
        assert_eq!(asked[0].kind, ItemKind::Question);
        assert!(asked[0].text.contains("Pick one"));
        assert!(asked[0].text.contains("[yes / no]"));

        let answered = items(&EventPayload::QuestionAnswered {
            id: "q1".into(),
            client_id: Some("client-9".into()),
            answer: "yes".into(),
        });
        assert_eq!(answered[0].kind, ItemKind::Answer);
        assert_eq!(answered[0].text, "answered: yes");
        assert!(!answered[0].text.contains("client-9"));
        assert_eq!(answered[0].debug.as_deref(), Some("q1 by client-9"));
    }

    #[test]
    fn maps_file_shared_from_client_and_llm() {
        let upload = items(&EventPayload::FileShared {
            name: "a.txt".into(),
            content_b64: b64(b"12345"),
            origin: FileOrigin::Client {
                client_id: "client-1".into(),
            },
        });
        assert_eq!(upload[0].kind, ItemKind::FileNote);
        assert_eq!(upload[0].text, "file a.txt (5 bytes) uploaded");
        assert_eq!(upload[0].debug.as_deref(), Some("client-1"));

        let sent = items(&EventPayload::FileShared {
            name: "b.bin".into(),
            content_b64: b64(b"123"),
            origin: FileOrigin::Llm {
                agent: "agent-0".into(),
            },
        });
        assert_eq!(sent[0].text, "file b.bin (3 bytes) sent by the assistant");

        let sent_by_sub = items(&EventPayload::FileShared {
            name: "c.bin".into(),
            content_b64: b64(b"123"),
            origin: FileOrigin::Llm {
                agent: "agent-3".into(),
            },
        });
        assert_eq!(
            sent_by_sub[0].text,
            "file c.bin (3 bytes) sent by subagent 3"
        );
    }

    #[test]
    fn maps_interrupted_to_a_dim_red_notice() {
        let mapped = items(&EventPayload::Interrupted {
            agent: "agent-0".into(),
        });
        assert_eq!(mapped[0].kind, ItemKind::Interrupted);
        assert_eq!(mapped[0].text, "■ interrupted by the user");
    }

    #[test]
    fn cost_turn_complete_and_awaiting_produce_no_transcript() {
        assert!(items(&EventPayload::CostReport {
            backend: "mock".into(),
            usage: UsageSnapshot::default(),
            quota: QuotaConfig::default(),
        })
        .is_empty());
        assert!(items(&EventPayload::TurnComplete {
            agent: "agent-0".into(),
            stop_reason: silo_core::conversation::StopReason::EndTurn,
        })
        .is_empty());
        assert!(items(&EventPayload::AwaitingInput).is_empty());
    }

    #[test]
    fn maps_access_report_updated_error_and_shutdown() {
        let access = items(&EventPayload::AccessReportUpdated {
            report: AccessReport::default(),
        });
        assert_eq!(access[0].kind, ItemKind::System);

        let error = items(&EventPayload::Error {
            context: "llm".into(),
            message: "boom".into(),
        });
        assert_eq!(error[0].kind, ItemKind::Error);
        assert_eq!(error[0].text, "llm: boom");

        let shutdown = items(&EventPayload::Shutdown {
            message: Some("bye".into()),
        });
        assert_eq!(shutdown[0].kind, ItemKind::Shutdown);
        assert_eq!(shutdown[0].text, "harness shut down: bye");
        let silent = items(&EventPayload::Shutdown { message: None });
        assert_eq!(silent[0].text, "harness shut down");
    }

    #[test]
    fn workspace_folder_names() {
        assert_eq!(workspace_folder_name("/a/b/myproject"), "myproject");
        assert_eq!(workspace_folder_name("/a/b/myproject/"), "myproject");
        assert_eq!(workspace_folder_name("relative/dir"), "dir");
        assert_eq!(workspace_folder_name("plain"), "plain");
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
                client_name: None,
                text: "hi".into(),
            },
        ));
        let before = app.transcript.len();
        app.apply_event(event(
            1,
            EventPayload::UserPrompt {
                client_id: None,
                client_name: None,
                text: "hi".into(),
            },
        ));
        app.apply_event(event(
            0,
            EventPayload::UserPrompt {
                client_id: None,
                client_name: None,
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
                client_name: None,
                text: "go".into(),
            },
        ));
        assert!(!app.awaiting_input);
    }

    #[test]
    fn busy_follows_the_event_stream() {
        let mut app = app();
        assert!(!app.busy);
        app.apply_event(event(
            0,
            EventPayload::UserPrompt {
                client_id: None,
                client_name: None,
                text: "go".into(),
            },
        ));
        assert!(app.busy);
        app.apply_event(event(1, EventPayload::AwaitingInput));
        assert!(!app.busy);
        app.apply_event(event(
            2,
            EventPayload::ToolUse {
                agent: "agent-0".into(),
                call: ToolCall {
                    id: "t1".into(),
                    name: "Bash".into(),
                    input: serde_json::json!({"command": "ls"}),
                },
            },
        ));
        assert!(app.busy);
        app.apply_event(event(
            3,
            EventPayload::Interrupted {
                agent: "agent-0".into(),
            },
        ));
        assert!(!app.busy);
        // Events without busy information leave the state alone.
        app.apply_event(event(
            4,
            EventPayload::CostReport {
                backend: "mock".into(),
                usage: UsageSnapshot::default(),
                quota: QuotaConfig::default(),
            },
        ));
        assert!(!app.busy);
    }

    #[test]
    fn spinner_advances_per_applied_event_only() {
        let mut app = app();
        let before = app.spinner_frame;
        app.apply_event(event(0, EventPayload::AwaitingInput));
        app.apply_event(event(
            1,
            EventPayload::UserPrompt {
                client_id: None,
                client_name: None,
                text: "go".into(),
            },
        ));
        assert_eq!(app.spinner_frame, before + 2);
        // A duplicate event is skipped and does not advance the spinner.
        app.apply_event(event(
            1,
            EventPayload::UserPrompt {
                client_id: None,
                client_name: None,
                text: "go".into(),
            },
        ));
        assert_eq!(app.spinner_frame, before + 2);
        let _ = app.spinner_char();
    }

    #[test]
    fn esc_sends_an_interrupt_only_while_busy() {
        let mut app = app();
        assert!(app.handle_key(key(KeyCode::Esc)).is_empty());
        app.apply_event(event(
            0,
            EventPayload::UserPrompt {
                client_id: None,
                client_name: None,
                text: "go".into(),
            },
        ));
        assert_eq!(
            app.handle_key(key(KeyCode::Esc)),
            vec![ClientMessage::Interrupt]
        );
        app.apply_event(event(1, EventPayload::AwaitingInput));
        assert!(app.handle_key(key(KeyCode::Esc)).is_empty());
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
        app.input = "/stop".into();
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            vec![ClientMessage::Interrupt]
        );
        assert!(app.input.is_empty());
    }

    #[test]
    fn quit_command_sets_the_flag() {
        let mut app = app();
        app.input = "/quit".into();
        assert!(app.handle_key(key(KeyCode::Enter)).is_empty());
        assert!(app.should_quit);
    }

    #[test]
    fn help_command_opens_the_popup_locally() {
        let mut app = app();
        app.input = "/help".into();
        assert!(app.handle_key(key(KeyCode::Enter)).is_empty());
        assert_eq!(app.popup, Some(Popup::Help));
        assert!(app.input.is_empty());
        // Any key closes it, like the other popups.
        app.handle_key(key(KeyCode::Char('x')));
        assert!(app.popup.is_none());
    }

    #[test]
    fn debug_command_toggles_the_per_session_flag() {
        let mut app = app();
        assert!(!app.debug);
        app.input = "/debug".into();
        assert!(app.handle_key(key(KeyCode::Enter)).is_empty());
        assert!(app.debug);
        assert!(matches!(
            app.transcript.last(),
            Some(TranscriptItem {
                kind: ItemKind::System,
                ..
            })
        ));
        app.input = "/debug".into();
        app.handle_key(key(KeyCode::Enter));
        assert!(!app.debug);
    }

    #[test]
    fn unknown_command_error_points_at_help() {
        let mut app = app();
        app.input = "/nope".into();
        app.handle_key(key(KeyCode::Enter));
        let last = app.transcript.last().unwrap();
        assert_eq!(last.kind, ItemKind::Error);
        assert!(last.text.contains("/help"));
    }

    #[test]
    fn harness_started_records_the_workspace_and_id() {
        let mut app = app();
        assert_eq!(app.harness_label(), "127.0.0.1:7777");
        app.apply_event(event(
            0,
            EventPayload::HarnessStarted {
                harness_id: "deadbeef42".into(),
                workspace: "/home/user/myproject".into(),
                sandbox: "mock".into(),
                llm: "mock".into(),
            },
        ));
        assert_eq!(app.harness_label(), "myproject");
        assert_eq!(app.harness_id, "deadbeef42");
    }

    #[test]
    fn run_file_workspace_seeds_the_label() {
        let app = App::new(
            "127.0.0.1:7777".into(),
            "ab".repeat(32),
            Some("/srv/work/thing".into()),
        );
        assert_eq!(app.harness_label(), "thing");
    }

    #[test]
    fn agent_names_feed_later_transcript_lines() {
        let mut app = app();
        app.apply_event(event(
            0,
            EventPayload::AgentSpawned {
                parent: "agent-0".into(),
                agent: "agent-1".into(),
                name: Some("refactor tests".into()),
                prompt: "go".into(),
            },
        ));
        app.apply_event(event(
            1,
            EventPayload::AssistantText {
                agent: "agent-1".into(),
                text: "working".into(),
            },
        ));
        let last = app.transcript.last().unwrap();
        assert_eq!(last.text, "  [subagent refactor tests] working");
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
