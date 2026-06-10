//! Handlers for the helper operations: shell execution, file access, and
//! directory listing. Each handler returns its failure as a per-request
//! error string, never a panic.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use silo_core::helper::{b64, unb64, DirEntry, HelperOp, HelperPayload};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::sync::oneshot;

use crate::fetch::{FetchConfig, FetchState};

/// Per-stream cap on captured Exec output.
pub(crate) const EXEC_OUTPUT_CAP: usize = 1024 * 1024;

/// Cap on bytes returned by one ReadFile request.
pub(crate) const READ_FILE_CAP: u64 = 5 * 1024 * 1024;

/// Window after process exit during which the output drain tasks may
/// finish reading the pipes (descendants of the shell can hold them open).
const DRAIN_GRACE: Duration = Duration::from_secs(5);

pub(crate) struct ServeState {
    fetch: FetchState,
    /// Cancellation senders for running `Exec` requests, keyed by request
    /// id. Registered by the serve loop before the exec task is spawned,
    /// so a `Cancel` read after the `Exec` always finds the entry.
    execs: Mutex<HashMap<u64, oneshot::Sender<()>>>,
}

impl ServeState {
    pub(crate) fn new(fetch_config: FetchConfig) -> Self {
        ServeState {
            fetch: FetchState::new(fetch_config),
            execs: Mutex::new(HashMap::new()),
        }
    }

    /// Registers a cancellation channel for the `Exec` request `id` and
    /// returns the receiving end for the exec task.
    pub(crate) fn register_exec(&self, id: u64) -> oneshot::Receiver<()> {
        let (tx, rx) = oneshot::channel();
        self.execs
            .lock()
            .expect("exec cancel map poisoned")
            .insert(id, tx);
        rx
    }

    fn unregister_exec(&self, id: u64) {
        self.execs
            .lock()
            .expect("exec cancel map poisoned")
            .remove(&id);
    }
}

pub(crate) async fn handle_op(
    state: &ServeState,
    id: u64,
    op: HelperOp,
    cancel: Option<oneshot::Receiver<()>>,
) -> Result<HelperPayload, String> {
    match op {
        HelperOp::Hello => Ok(HelperPayload::Hello {
            version: env!("CARGO_PKG_VERSION").to_string(),
            pid: std::process::id(),
        }),
        HelperOp::Exec {
            command,
            cwd,
            env,
            timeout_ms,
        } => exec(state, id, cancel, command, cwd, env, timeout_ms).await,
        HelperOp::Cancel { id: target } => {
            let sender = state
                .execs
                .lock()
                .expect("exec cancel map poisoned")
                .remove(&target);
            match sender {
                Some(sender) => {
                    let _ = sender.send(());
                    Ok(HelperPayload::Ack)
                }
                // The target finished (or never existed) before the cancel
                // arrived; the original request's response stands.
                None => Err(format!("no cancellable request with id {target}")),
            }
        }
        HelperOp::ReadFile {
            path,
            offset,
            limit,
        } => read_file(path, offset, limit).await,
        HelperOp::WriteFile {
            path,
            content_b64,
            append,
        } => write_file(path, content_b64, append).await,
        HelperOp::EditFile {
            path,
            old,
            new,
            replace_all,
        } => edit_file(path, old, new, replace_all).await,
        HelperOp::ListDir { path } => list_dir(path).await,
        HelperOp::Fetch {
            url,
            method,
            headers,
            body_b64,
            max_bytes,
        } => {
            state
                .fetch
                .fetch(url, method, headers, body_b64, max_bytes)
                .await
        }
        HelperOp::Shutdown => Ok(HelperPayload::Ack),
    }
}

struct CappedBuffer {
    data: Vec<u8>,
    cap: usize,
    truncated: bool,
}

impl CappedBuffer {
    fn new(cap: usize) -> Self {
        CappedBuffer {
            data: Vec::new(),
            cap,
            truncated: false,
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        let room = self.cap.saturating_sub(self.data.len());
        let take = room.min(chunk.len());
        self.data.extend_from_slice(&chunk[..take]);
        if take < chunk.len() {
            self.truncated = true;
        }
    }
}

/// Reads `source` to end-of-stream, keeping at most the buffer's cap and
/// recording whether anything was dropped.
async fn drain<R>(mut source: R, buffer: Arc<Mutex<CappedBuffer>>)
where
    R: AsyncRead + Unpin,
{
    let mut chunk = [0u8; 8192];
    loop {
        match source.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => buffer
                .lock()
                .expect("capped buffer poisoned")
                .push(&chunk[..n]),
        }
    }
}

fn buffer_contents(buffer: &Arc<Mutex<CappedBuffer>>) -> (String, bool) {
    let guard = buffer.lock().expect("capped buffer poisoned");
    (
        String::from_utf8_lossy(&guard.data).into_owned(),
        guard.truncated,
    )
}

/// Kills the child's process group, then the child itself, and reaps it.
/// The child is its own process group leader (the spawn sets
/// `process_group(0)`), so descendants of the shell die with it.
async fn kill_child_group(child: &mut Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
    let _ = child.start_kill();
    let _ = child.wait().await;
}

async fn exec(
    state: &ServeState,
    id: u64,
    cancel: Option<oneshot::Receiver<()>>,
    command: String,
    cwd: Option<String>,
    env: Vec<(String, String)>,
    timeout_ms: u64,
) -> Result<HelperPayload, String> {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(&command);
    if let Some(cwd) = &cwd {
        cmd.current_dir(cwd);
    }
    for (name, value) in &env {
        cmd.env(name, value);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);
    let spawned = cmd.spawn().map_err(|e| format!("cannot spawn shell: {e}"));
    let mut child = match spawned {
        Ok(child) => child,
        Err(message) => {
            state.unregister_exec(id);
            return Err(message);
        }
    };
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let (Some(stdout), Some(stderr)) = (stdout, stderr) else {
        state.unregister_exec(id);
        let _ = child.start_kill();
        return Err("child stdio unavailable".to_string());
    };
    let stdout_buffer = Arc::new(Mutex::new(CappedBuffer::new(EXEC_OUTPUT_CAP)));
    let stderr_buffer = Arc::new(Mutex::new(CappedBuffer::new(EXEC_OUTPUT_CAP)));
    let stdout_task = tokio::spawn(drain(stdout, stdout_buffer.clone()));
    let stderr_task = tokio::spawn(drain(stderr, stderr_buffer.clone()));

    let cancel_signal = async move {
        match cancel {
            Some(receiver) => {
                let _ = receiver.await;
            }
            // No cancellation channel was registered for this execution.
            None => std::future::pending::<()>().await,
        }
    };
    let timeout = Duration::from_millis(timeout_ms);
    let mut cancelled = false;
    let (exit_code, timed_out) = tokio::select! {
        () = cancel_signal => {
            kill_child_group(&mut child).await;
            cancelled = true;
            (-1, false)
        }
        waited = tokio::time::timeout(timeout, child.wait()) => match waited {
            Ok(Ok(status)) => (status.code().unwrap_or(-1), false),
            Ok(Err(e)) => {
                state.unregister_exec(id);
                return Err(format!("wait for child failed: {e}"));
            }
            Err(_) => {
                let _ = child.kill().await;
                (-1, true)
            }
        },
    };
    state.unregister_exec(id);

    let _ = tokio::time::timeout(DRAIN_GRACE, async {
        let _ = stdout_task.await;
        let _ = stderr_task.await;
    })
    .await;

    let (stdout, stdout_truncated) = buffer_contents(&stdout_buffer);
    let (stderr, stderr_truncated) = buffer_contents(&stderr_buffer);
    Ok(HelperPayload::Exec {
        exit_code,
        stdout,
        stderr,
        timed_out,
        truncated: stdout_truncated || stderr_truncated,
        cancelled,
    })
}

async fn read_file(
    path: String,
    offset: Option<u64>,
    limit: Option<u64>,
) -> Result<HelperPayload, String> {
    let mut file = tokio::fs::File::open(&path)
        .await
        .map_err(|e| format!("cannot open {path}: {e}"))?;
    if let Some(offset) = offset {
        if offset > 0 {
            file.seek(std::io::SeekFrom::Start(offset))
                .await
                .map_err(|e| format!("cannot seek in {path}: {e}"))?;
        }
    }
    let cap = limit.map_or(READ_FILE_CAP, |limit| limit.min(READ_FILE_CAP));
    let mut content = Vec::new();
    // One extra byte distinguishes "exactly cap bytes" from "more remains".
    file.take(cap + 1)
        .read_to_end(&mut content)
        .await
        .map_err(|e| format!("cannot read {path}: {e}"))?;
    let truncated = content.len() as u64 > cap;
    if truncated {
        content.truncate(cap as usize);
    }
    Ok(HelperPayload::File {
        content_b64: b64(&content),
        truncated,
    })
}

async fn write_file(
    path: String,
    content_b64: String,
    append: bool,
) -> Result<HelperPayload, String> {
    let content = unb64(&content_b64)?;
    let path_buf = PathBuf::from(&path);
    if let Some(parent) = path_buf.parent() {
        if !parent.as_os_str().is_empty() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| format!("cannot create directory {}: {e}", parent.display()))?;
        }
    }
    if append {
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path_buf)
            .await
            .map_err(|e| format!("cannot open {path} for appending: {e}"))?;
        file.write_all(&content)
            .await
            .map_err(|e| format!("cannot write {path}: {e}"))?;
        file.flush()
            .await
            .map_err(|e| format!("cannot write {path}: {e}"))?;
    } else {
        tokio::fs::write(&path_buf, &content)
            .await
            .map_err(|e| format!("cannot write {path}: {e}"))?;
    }
    Ok(HelperPayload::Written {
        bytes: content.len() as u64,
    })
}

async fn edit_file(
    path: String,
    old: String,
    new: String,
    replace_all: bool,
) -> Result<HelperPayload, String> {
    if old.is_empty() {
        return Err("old string is empty".into());
    }
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|e| format!("cannot read {path}: {e}"))?;
    let text = String::from_utf8(bytes).map_err(|_| format!("{path} is not valid UTF-8 text"))?;
    let count = text.matches(old.as_str()).count() as u64;
    if count == 0 {
        return Err("old string not found".into());
    }
    if count > 1 && !replace_all {
        return Err(format!(
            "old string matches {count} times; set replace_all to change every occurrence"
        ));
    }
    let (updated, replacements) = if replace_all {
        (text.replace(old.as_str(), new.as_str()), count)
    } else {
        (text.replacen(old.as_str(), new.as_str(), 1), 1)
    };
    tokio::fs::write(&path, updated)
        .await
        .map_err(|e| format!("cannot write {path}: {e}"))?;
    Ok(HelperPayload::Edited { replacements })
}

async fn list_dir(path: String) -> Result<HelperPayload, String> {
    let mut read_dir = tokio::fs::read_dir(&path)
        .await
        .map_err(|e| format!("cannot list {path}: {e}"))?;
    let mut entries = Vec::new();
    loop {
        let entry = read_dir
            .next_entry()
            .await
            .map_err(|e| format!("cannot list {path}: {e}"))?;
        let Some(entry) = entry else { break };
        let name = entry.file_name().to_string_lossy().into_owned();
        let (is_dir, size) = match entry.metadata().await {
            Ok(metadata) => (metadata.is_dir(), metadata.len()),
            Err(_) => (false, 0),
        };
        entries.push(DirEntry { name, is_dir, size });
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(HelperPayload::Dir { entries })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload_file(payload: HelperPayload) -> (Vec<u8>, bool) {
        match payload {
            HelperPayload::File {
                content_b64,
                truncated,
            } => (unb64(&content_b64).unwrap(), truncated),
            other => panic!("expected File payload, got {other:?}"),
        }
    }

    fn payload_exec(payload: HelperPayload) -> (i32, String, String, bool, bool) {
        match payload {
            HelperPayload::Exec {
                exit_code,
                stdout,
                stderr,
                timed_out,
                truncated,
                cancelled: _,
            } => (exit_code, stdout, stderr, timed_out, truncated),
            other => panic!("expected Exec payload, got {other:?}"),
        }
    }

    fn state() -> ServeState {
        ServeState::new(FetchConfig::default())
    }

    async fn exec_plain(
        command: &str,
        cwd: Option<String>,
        env: Vec<(String, String)>,
        timeout_ms: u64,
    ) -> Result<HelperPayload, String> {
        exec(&state(), 0, None, command.into(), cwd, env, timeout_ms).await
    }

    #[tokio::test]
    async fn read_file_honors_offset_and_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.txt");
        std::fs::write(&path, b"0123456789").unwrap();
        let path = path.display().to_string();

        let (content, truncated) = payload_file(read_file(path.clone(), None, None).await.unwrap());
        assert_eq!(content, b"0123456789");
        assert!(!truncated);

        let (content, truncated) =
            payload_file(read_file(path.clone(), Some(3), Some(4)).await.unwrap());
        assert_eq!(content, b"3456");
        assert!(truncated);

        let (content, truncated) =
            payload_file(read_file(path.clone(), Some(6), Some(100)).await.unwrap());
        assert_eq!(content, b"6789");
        assert!(!truncated);

        let (content, truncated) = payload_file(read_file(path, Some(20), None).await.unwrap());
        assert!(content.is_empty());
        assert!(!truncated);
    }

    #[tokio::test]
    async fn read_file_caps_at_five_mebibytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.bin");
        std::fs::write(&path, vec![7u8; READ_FILE_CAP as usize + 100]).unwrap();
        let (content, truncated) = payload_file(
            read_file(path.display().to_string(), None, None)
                .await
                .unwrap(),
        );
        assert_eq!(content.len() as u64, READ_FILE_CAP);
        assert!(truncated);
    }

    #[tokio::test]
    async fn read_file_reports_missing_file() {
        let err = read_file("/nonexistent/definitely/missing".into(), None, None)
            .await
            .unwrap_err();
        assert!(err.contains("cannot open"));
    }

    #[tokio::test]
    async fn write_file_creates_parents_and_appends() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a/b/c.txt");
        let path_str = path.display().to_string();
        let payload = write_file(path_str.clone(), b64(b"one"), false)
            .await
            .unwrap();
        assert_eq!(payload, HelperPayload::Written { bytes: 3 });
        let payload = write_file(path_str, b64(b" two"), true).await.unwrap();
        assert_eq!(payload, HelperPayload::Written { bytes: 4 });
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one two");
    }

    #[tokio::test]
    async fn write_file_rejects_invalid_base64() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.txt").display().to_string();
        let err = write_file(path, "not base64!!!".into(), false)
            .await
            .unwrap_err();
        assert!(err.contains("base64"));
    }

    #[tokio::test]
    async fn edit_file_replaces_counts_and_rejects() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("text.txt");
        std::fs::write(&path, "alpha beta alpha").unwrap();
        let path_str = path.display().to_string();

        let err = edit_file(path_str.clone(), "zeta".into(), "x".into(), false)
            .await
            .unwrap_err();
        assert!(err.contains("not found"));

        let err = edit_file(path_str.clone(), "alpha".into(), "x".into(), false)
            .await
            .unwrap_err();
        assert!(err.contains("matches 2 times"));

        let payload = edit_file(path_str.clone(), "beta".into(), "gamma".into(), false)
            .await
            .unwrap();
        assert_eq!(payload, HelperPayload::Edited { replacements: 1 });
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "alpha gamma alpha");

        let payload = edit_file(path_str.clone(), "alpha".into(), "delta".into(), true)
            .await
            .unwrap();
        assert_eq!(payload, HelperPayload::Edited { replacements: 2 });
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "delta gamma delta");

        let err = edit_file(path_str, "".into(), "x".into(), false)
            .await
            .unwrap_err();
        assert!(err.contains("empty"));
    }

    #[tokio::test]
    async fn edit_file_rejects_binary_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bin.dat");
        std::fs::write(&path, [0xff, 0xfe, 0x00, 0x41]).unwrap();
        let err = edit_file(path.display().to_string(), "A".into(), "B".into(), false)
            .await
            .unwrap_err();
        assert!(err.contains("not valid UTF-8"));
    }

    #[tokio::test]
    async fn list_dir_sorts_entries_by_name() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("zebra.txt"), "12345").unwrap();
        std::fs::write(dir.path().join("apple.txt"), "1").unwrap();
        std::fs::create_dir(dir.path().join("middle")).unwrap();
        let payload = list_dir(dir.path().display().to_string()).await.unwrap();
        let HelperPayload::Dir { entries } = payload else {
            panic!("expected Dir payload");
        };
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["apple.txt", "middle", "zebra.txt"]);
        assert!(!entries[0].is_dir);
        assert_eq!(entries[0].size, 1);
        assert!(entries[1].is_dir);
        assert_eq!(entries[2].size, 5);
    }

    #[tokio::test]
    async fn exec_captures_streams_and_exit_code() {
        let payload = exec_plain("echo out; echo err >&2; exit 3", None, vec![], 30_000)
            .await
            .unwrap();
        let (exit_code, stdout, stderr, timed_out, truncated) = payload_exec(payload);
        assert_eq!(exit_code, 3);
        assert_eq!(stdout, "out\n");
        assert_eq!(stderr, "err\n");
        assert!(!timed_out);
        assert!(!truncated);
    }

    #[tokio::test]
    async fn exec_honors_cwd_and_env() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        let payload = exec_plain(
            "printf '%s|%s' \"$PWD\" \"$SILO_TEST_VAR\"",
            Some(cwd.display().to_string()),
            vec![("SILO_TEST_VAR".into(), "marker".into())],
            30_000,
        )
        .await
        .unwrap();
        let (exit_code, stdout, _, _, _) = payload_exec(payload);
        assert_eq!(exit_code, 0);
        assert_eq!(stdout, format!("{}|marker", cwd.display()));
    }

    #[tokio::test]
    async fn exec_kills_on_timeout() {
        let payload = exec_plain("sleep 5", None, vec![], 200).await.unwrap();
        let (exit_code, _, _, timed_out, _) = payload_exec(payload);
        assert_eq!(exit_code, -1);
        assert!(timed_out);
    }

    #[tokio::test]
    async fn exec_caps_output_at_one_mebibyte() {
        let command = "dd if=/dev/zero bs=1024 count=2048 2>/dev/null | tr '\\0' 'x'";
        let payload = exec_plain(command, None, vec![], 60_000).await.unwrap();
        let (exit_code, stdout, _, timed_out, truncated) = payload_exec(payload);
        assert_eq!(exit_code, 0);
        assert!(!timed_out);
        assert!(truncated);
        assert_eq!(stdout.len(), EXEC_OUTPUT_CAP);
    }

    #[tokio::test]
    async fn cancel_ends_a_running_exec_with_partial_output() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("marker");
        let state = state();
        let cancel_rx = state.register_exec(7);
        let exec_future = exec(
            &state,
            7,
            Some(cancel_rx),
            format!(
                "echo started; touch '{}'; sleep 30; echo finished",
                marker.display()
            ),
            None,
            vec![],
            120_000,
        );
        tokio::pin!(exec_future);
        // Drive the exec until the marker proves the echo ran, then cancel.
        let payload = loop {
            tokio::select! {
                payload = &mut exec_future => break payload.unwrap(),
                () = tokio::time::sleep(Duration::from_millis(20)) => {
                    if marker.exists() {
                        let sender = state
                            .execs
                            .lock()
                            .unwrap()
                            .remove(&7);
                        if let Some(sender) = sender {
                            let _ = sender.send(());
                        }
                    }
                }
            }
        };
        match payload {
            HelperPayload::Exec {
                exit_code,
                stdout,
                timed_out,
                cancelled,
                ..
            } => {
                assert_eq!(exit_code, -1);
                assert!(cancelled);
                assert!(!timed_out);
                assert_eq!(stdout, "started\n");
            }
            other => panic!("expected Exec payload, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cancel_of_an_unknown_id_is_a_per_request_error() {
        let state = state();
        let err = handle_op(&state, 1, HelperOp::Cancel { id: 42 }, None)
            .await
            .unwrap_err();
        assert!(err.contains("42"), "unexpected error: {err}");
    }
}
