//! Client file uploads.
//!
//! When an interactive client uploads a file, the frontend emits
//! `FileShared` with a client origin. This listener writes the bytes into
//! `_uploads/<sanitized name>` in the workspace through the sandbox Write
//! tool, so the model can read the file under the same sandbox policy as
//! everything else.

use serde_json::json;

use silo_core::event::{EventBus, EventPayload, FileOrigin};
use silo_core::helper::unb64;
use silo_core::journal::{JournalEntry, JournalHandle};
use silo_core::tool::ToolCall;
use silo_core::traits::Sandbox;
use tokio::sync::broadcast;

/// Watches the event bus for client uploads and stores each one in the
/// workspace. Catches up from the bus history first, then follows live
/// events; on subscriber lag it resynchronizes from the history, so no
/// upload is skipped. Returns only when the event bus closes.
pub(crate) async fn listen(bus: &EventBus, sandbox: &dyn Sandbox, journal: &JournalHandle) {
    let mut events = bus.subscribe();
    let mut counter: u64 = 0;
    let mut next_seq: u64 = 0;
    for event in bus.since(0) {
        next_seq = event.seq + 1;
        handle_event(sandbox, journal, event.payload, &mut counter).await;
    }
    loop {
        match events.recv().await {
            Ok(event) => {
                if event.seq < next_seq {
                    continue;
                }
                next_seq = event.seq + 1;
                handle_event(sandbox, journal, event.payload, &mut counter).await;
            }
            Err(broadcast::error::RecvError::Lagged(_)) => {
                for event in bus.since(next_seq) {
                    next_seq = event.seq + 1;
                    handle_event(sandbox, journal, event.payload, &mut counter).await;
                }
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

async fn handle_event(
    sandbox: &dyn Sandbox,
    journal: &JournalHandle,
    payload: EventPayload,
    counter: &mut u64,
) {
    if let EventPayload::FileShared {
        name,
        content_b64,
        origin: FileOrigin::Client { .. },
    } = payload
    {
        *counter += 1;
        store_upload(sandbox, journal, &name, &content_b64, *counter).await;
    }
}

async fn store_upload(
    sandbox: &dyn Sandbox,
    journal: &JournalHandle,
    name: &str,
    content_b64: &str,
    counter: u64,
) {
    let file_name = sanitize_file_name(name);
    if let Err(error) = unb64(content_b64) {
        journal.append(JournalEntry::Lifecycle {
            message: format!("rejected upload {name:?}: {error}"),
        });
        return;
    }
    // The base64 is passed through so binary uploads reach the workspace
    // byte-for-byte; the Write tool decodes it in the helper.
    let call = ToolCall {
        id: format!("upload-{counter}"),
        name: "Write".into(),
        input: json!({
            "path": format!("_uploads/{file_name}"),
            "content_b64": content_b64,
        }),
    };
    match sandbox.run_tool(&"harness".to_string(), &call).await {
        Ok(output) => {
            let failed = output.is_error;
            // The Write executes in the sandbox; the entry carries owner
            // "sandbox" with agent "harness".
            journal.append(JournalEntry::ToolExec {
                agent: "harness".into(),
                owner: "sandbox".into(),
                call,
                output,
            });
            journal.append(JournalEntry::Lifecycle {
                message: if failed {
                    format!("failed to store client upload _uploads/{file_name}")
                } else {
                    format!("stored client upload as _uploads/{file_name}")
                },
            });
        }
        Err(error) => {
            journal.append(JournalEntry::Lifecycle {
                message: format!("failed to store client upload {file_name}: {error}"),
            });
        }
    }
}

/// Reduces an uploaded file name to a single safe path component: the part
/// after the last path separator, with control characters removed. Empty
/// and dot-only results become "upload".
pub(crate) fn sanitize_file_name(name: &str) -> String {
    let last = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let cleaned: String = last.chars().filter(|c| !c.is_control()).collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        "upload".to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use serde_json::json;
    use silo_core::clock::FakeClock;
    use silo_core::config::SandboxConfig;
    use silo_core::helper::b64;
    use silo_core::journal::{parse_journal, JournalWriter};
    use silo_core::replay::{ScriptedToolExec, SharedScript, TestScript};
    use silo_core::tool::ToolOutput;

    use super::*;

    fn journal_pair() -> (JournalHandle, Arc<Mutex<Vec<u8>>>) {
        let (writer, buffer) = JournalWriter::in_memory(Arc::new(FakeClock::default()));
        (JournalHandle::new(writer), buffer)
    }

    #[tokio::test]
    async fn upload_is_written_via_the_sandbox_write_tool() {
        let script = SharedScript::new(TestScript {
            tools: vec![ScriptedToolExec {
                expect_name: "Write".into(),
                expect_input: Some(json!({
                    "path": "_uploads/notes.txt",
                    "content_b64": b64(b"hello"),
                })),
                output: ToolOutput::ok("Wrote 5 bytes to _uploads/notes.txt"),
            }],
            ..TestScript::default()
        });
        let (journal, buffer) = journal_pair();
        let sandbox = silo_sandbox::create_sandbox(
            &SandboxConfig::default(),
            None,
            Some(script.clone()),
            journal.clone(),
        )
        .await
        .unwrap();

        store_upload(
            sandbox.as_ref(),
            &journal,
            "dir/notes.txt",
            &b64(b"hello"),
            1,
        )
        .await;

        assert!(
            script.finished(),
            "remaining: {}",
            script.remaining_summary()
        );
        let records = parse_journal(&buffer.lock().unwrap()).unwrap();
        let exec = records
            .iter()
            .find_map(|record| match &record.entry {
                JournalEntry::ToolExec { owner, call, .. } => Some((owner.clone(), call.clone())),
                _ => None,
            })
            .expect("a ToolExec entry");
        assert_eq!(exec.0, "sandbox");
        assert_eq!(exec.1.name, "Write");
    }

    #[tokio::test]
    async fn invalid_base64_is_rejected_without_a_tool_call() {
        let script = SharedScript::new(TestScript::default());
        let (journal, buffer) = journal_pair();
        let sandbox = silo_sandbox::create_sandbox(
            &SandboxConfig::default(),
            None,
            Some(script),
            journal.clone(),
        )
        .await
        .unwrap();

        store_upload(sandbox.as_ref(), &journal, "x.bin", "%%%not-base64%%%", 1).await;

        let records = parse_journal(&buffer.lock().unwrap()).unwrap();
        assert!(records
            .iter()
            .all(|record| !matches!(record.entry, JournalEntry::ToolExec { .. })));
        assert!(records.iter().any(|record| matches!(
            &record.entry,
            JournalEntry::Lifecycle { message } if message.contains("rejected upload")
        )));
    }

    #[test]
    fn sanitization_strips_directories_and_separators() {
        assert_eq!(sanitize_file_name("report.pdf"), "report.pdf");
        assert_eq!(sanitize_file_name("a/b/c.txt"), "c.txt");
        assert_eq!(sanitize_file_name("..\\..\\evil.sh"), "evil.sh");
        assert_eq!(sanitize_file_name("/etc/passwd"), "passwd");
        assert_eq!(sanitize_file_name("../../"), "upload");
        assert_eq!(sanitize_file_name(".."), "upload");
        assert_eq!(sanitize_file_name(""), "upload");
        assert_eq!(sanitize_file_name("  "), "upload");
        assert_eq!(sanitize_file_name("a\nb.txt"), "ab.txt");
    }
}
