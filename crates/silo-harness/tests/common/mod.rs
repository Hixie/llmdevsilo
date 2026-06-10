//! Shared fixtures for the harness integration tests. Every test runs with
//! the mock LLM, mock sandbox, mock frontend, and mock proxy, under the
//! fake clock, with a tempdir state directory, a plain-directory
//! workspace, and an in-memory journal.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::Value;

use silo_core::clock::FakeClock;
use silo_core::config::{
    FrontendConfig, FrontendKind, HarnessConfig, LlmConfig, LoggingConfig, SandboxConfig,
};
use silo_core::conversation::{CompletionResponse, ContentBlock, StopReason, TokenDelta};
use silo_core::event::Event;
use silo_core::journal::{
    parse_journal, JournalEntry, JournalHandle, JournalRecord, JournalWriter,
};
use silo_core::replay::{ScriptedLlmTurn, SharedScript, TestScript};
use silo_harness::RunOptions;
use silo_workspace::{ContainerStrategy, WorkspaceManager};

/// One prepared test session: a state directory, a locked workspace, and an
/// in-memory journal.
pub struct Fixture {
    pub state: tempfile::TempDir,
    pub workspace_root: tempfile::TempDir,
    pub workspace: PathBuf,
    pub journal: JournalHandle,
    pub journal_buffer: Arc<Mutex<Vec<u8>>>,
}

impl Fixture {
    pub fn new() -> Fixture {
        let state = tempfile::tempdir().expect("state tempdir");
        let workspace_root = tempfile::tempdir().expect("workspace tempdir");
        let workspace = workspace_root.path().join("ws");
        lock_workspace(state.path(), &workspace);
        let (journal, journal_buffer) = in_memory_journal();
        Fixture {
            state,
            workspace_root,
            workspace,
            journal,
            journal_buffer,
        }
    }

    pub fn config(&self) -> HarnessConfig {
        mock_config(&self.workspace)
    }

    pub fn options(&self, script: SharedScript) -> RunOptions {
        RunOptions {
            script: Some(script),
            deterministic: true,
            mock_proxy: true,
            journal: Some(self.journal.clone()),
            state_dir: Some(self.state.path().to_path_buf()),
            ..RunOptions::default()
        }
    }

    pub fn records(&self) -> Vec<JournalRecord> {
        let bytes = self.journal_buffer.lock().expect("journal buffer").clone();
        parse_journal(&bytes).expect("journal parses")
    }

    pub fn events(&self) -> Vec<Event> {
        events_of(&self.records())
    }

    /// Fresh in-memory journal for a second run against the same fixture.
    pub fn reset_journal(&mut self) {
        let (journal, buffer) = in_memory_journal();
        self.journal = journal;
        self.journal_buffer = buffer;
    }
}

pub fn in_memory_journal() -> (JournalHandle, Arc<Mutex<Vec<u8>>>) {
    let clock = Arc::new(FakeClock::default());
    let (writer, buffer) = JournalWriter::in_memory(clock);
    (JournalHandle::new(writer), buffer)
}

pub fn lock_workspace(state: &Path, workspace: &Path) {
    WorkspaceManager::with_strategy(state.to_path_buf(), ContainerStrategy::PlainDir)
        .lock(workspace)
        .expect("workspace locks");
}

pub fn mock_config(workspace: &Path) -> HarnessConfig {
    HarnessConfig {
        harness_id: "test-harness".into(),
        workspace: workspace.to_path_buf(),
        llm: LlmConfig::default(),
        sandbox: SandboxConfig::default(),
        frontend: FrontendConfig {
            kind: FrontendKind::Mock,
            ..FrontendConfig::default()
        },
        logging: LoggingConfig::default(),
    }
}

pub fn events_of(records: &[JournalRecord]) -> Vec<Event> {
    records
        .iter()
        .filter_map(|record| match &record.entry {
            JournalEntry::Event { event } => Some(event.clone()),
            _ => None,
        })
        .collect()
}

pub fn event_kinds(events: &[Event]) -> Vec<&'static str> {
    events.iter().map(|event| event.payload.kind()).collect()
}

pub fn assert_strictly_increasing_from_zero(events: &[Event]) {
    for (index, event) in events.iter().enumerate() {
        assert_eq!(
            event.seq, index as u64,
            "event {index} has seq {}",
            event.seq
        );
    }
}

/// A scripted LLM turn whose response contains text and tool-use blocks.
pub fn llm_turn(
    expect_user_contains: Option<&str>,
    text: Option<&str>,
    tools: &[(&str, &str, Value)],
    stop_reason: StopReason,
    usage: TokenDelta,
) -> ScriptedLlmTurn {
    let mut content = Vec::new();
    if let Some(text) = text {
        content.push(ContentBlock::Text { text: text.into() });
    }
    for (id, name, input) in tools {
        content.push(ContentBlock::ToolUse {
            id: (*id).to_string(),
            name: (*name).to_string(),
            input: input.clone(),
        });
    }
    ScriptedLlmTurn {
        expect_user_contains: expect_user_contains.map(str::to_string),
        response: CompletionResponse {
            content,
            stop_reason,
            usage,
        },
    }
}

pub fn shared(script: TestScript) -> SharedScript {
    SharedScript::new(script)
}
