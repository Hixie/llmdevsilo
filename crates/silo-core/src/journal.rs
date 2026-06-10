//! Persistent structured logging.
//!
//! Every interaction between harness modules is appended to a journal as a
//! typed record: frontend commands, LLM requests and responses, tool
//! executions, network operations, and lifecycle notes. The record types
//! carry no secrets by construction (credentials are referenced by
//! environment variable name, see [`crate::secrets`]), so a journal can be
//! shared and replayed safely. [`crate::replay::script_from_journal`] turns
//! a journal into a [`crate::replay::TestScript`] that reproduces the
//! session against mock components.
//!
//! Journals are written under the harness state directory, which is never
//! part of the sandbox read allowlist (enforced by [`crate::risk`]).

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::clock::{SharedClock, Timestamp};
use crate::conversation::{AgentId, CompletionRequest, CompletionResponse};
use crate::event::Event;
use crate::tool::{ToolCall, ToolOutput};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JournalRecord {
    pub seq: u64,
    pub time: Timestamp,
    #[serde(flatten)]
    pub entry: JournalEntry,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "entry", rename_all = "snake_case")]
pub enum JournalEntry {
    Meta {
        harness_id: String,
        harness_version: String,
        config_summary: String,
    },
    Event {
        event: Event,
    },
    LlmRequest {
        agent: AgentId,
        backend: String,
        request: CompletionRequest,
    },
    LlmResponse {
        agent: AgentId,
        backend: String,
        response: CompletionResponse,
    },
    ToolExec {
        agent: AgentId,
        /// "sandbox", "frontend", or "harness" — which component ran it.
        owner: String,
        call: ToolCall,
        output: ToolOutput,
    },
    FrontendCommand {
        command: serde_json::Value,
    },
    Network {
        record: NetworkRecord,
    },
    Lifecycle {
        message: String,
    },
}

/// Summary of one proxied network exchange. Carries metadata only — never
/// request or response bodies of credentialed calls, and never credential
/// values.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NetworkRecord {
    pub host: String,
    pub port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub allowed: bool,
    pub credential_injected: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

enum Sink {
    File(File),
    Memory(Arc<Mutex<Vec<u8>>>),
    Disabled,
}

/// Appends records as JSON Lines. Wrap in a [`JournalHandle`] for shared use.
pub struct JournalWriter {
    sink: Sink,
    clock: SharedClock,
    next_seq: u64,
}

impl JournalWriter {
    pub fn to_file(path: &Path, clock: SharedClock) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(JournalWriter {
            sink: Sink::File(file),
            clock,
            next_seq: 0,
        })
    }

    pub fn in_memory(clock: SharedClock) -> (Self, Arc<Mutex<Vec<u8>>>) {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        (
            JournalWriter {
                sink: Sink::Memory(buffer.clone()),
                clock,
                next_seq: 0,
            },
            buffer,
        )
    }

    pub fn disabled(clock: SharedClock) -> Self {
        JournalWriter {
            sink: Sink::Disabled,
            clock,
            next_seq: 0,
        }
    }

    pub fn append(&mut self, entry: JournalEntry) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        let record = JournalRecord {
            seq,
            time: self.clock.now(),
            entry,
        };
        let mut line = serde_json::to_string(&record).expect("journal records always serialize");
        line.push('\n');
        match &mut self.sink {
            Sink::File(file) => {
                let _ = file.write_all(line.as_bytes());
                let _ = file.flush();
            }
            Sink::Memory(buffer) => {
                buffer
                    .lock()
                    .expect("journal buffer poisoned")
                    .extend_from_slice(line.as_bytes());
            }
            Sink::Disabled => {}
        }
        seq
    }
}

/// Cloneable, thread-safe handle to a journal writer.
#[derive(Clone)]
pub struct JournalHandle(Arc<Mutex<JournalWriter>>);

impl JournalHandle {
    pub fn new(writer: JournalWriter) -> Self {
        JournalHandle(Arc::new(Mutex::new(writer)))
    }

    pub fn disabled(clock: SharedClock) -> Self {
        JournalHandle::new(JournalWriter::disabled(clock))
    }

    pub fn append(&self, entry: JournalEntry) -> u64 {
        self.0
            .lock()
            .expect("journal writer poisoned")
            .append(entry)
    }
}

impl std::fmt::Debug for JournalHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("JournalHandle")
    }
}

pub fn read_journal(path: &Path) -> std::io::Result<Vec<JournalRecord>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut records = Vec::new();
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let record: JournalRecord = serde_json::from_str(&line)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        records.push(record);
    }
    Ok(records)
}

pub fn parse_journal(bytes: &[u8]) -> std::io::Result<Vec<JournalRecord>> {
    let mut records = Vec::new();
    for line in bytes.split(|b| *b == b'\n') {
        if line.iter().all(|b| b.is_ascii_whitespace()) {
            continue;
        }
        let record: JournalRecord = serde_json::from_slice(line)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        records.push(record);
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::FakeClock;
    use std::sync::Arc as StdArc;

    #[test]
    fn journal_roundtrips_records() {
        let clock: SharedClock = StdArc::new(FakeClock::default());
        let (writer, buffer) = JournalWriter::in_memory(clock);
        let handle = JournalHandle::new(writer);
        handle.append(JournalEntry::Lifecycle {
            message: "started".into(),
        });
        handle.append(JournalEntry::Network {
            record: NetworkRecord {
                host: "api.example.com".into(),
                port: 443,
                allowed: true,
                ..NetworkRecord::default()
            },
        });

        let bytes = buffer.lock().unwrap().clone();
        let records = parse_journal(&bytes).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].seq, 0);
        assert_eq!(records[1].seq, 1);
        assert!(matches!(records[0].entry, JournalEntry::Lifecycle { .. }));
        assert!(records[0].time.wall_ms.is_none());
    }
}
