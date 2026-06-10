//! Harness-side connection to the helper process inside a sandbox.
//!
//! A [`HelperSession`] speaks the JSON-lines protocol from
//! `silo_core::helper` over any byte stream. Requests get sequential ids;
//! a writer task serializes outgoing frames and a reader task completes
//! the matching pending request as each response arrives, so any number
//! of requests can be in flight at once and responses may arrive out of
//! order.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use silo_core::error::SandboxError;
use silo_core::helper::{
    read_json_line, write_json_line, HelperOp, HelperPayload, HelperRequest, HelperResponse,
};
use tokio::io::{AsyncRead, AsyncWrite, BufReader};
use tokio::sync::{mpsc, oneshot};

/// Pending requests waiting for a response, keyed by request id. `None`
/// marks the connection as closed: no further requests are accepted, and
/// every waiter has been failed.
type Pending =
    Arc<Mutex<Option<HashMap<u64, oneshot::Sender<Result<HelperPayload, SandboxError>>>>>>;

fn closed_error() -> SandboxError {
    SandboxError::Unavailable("helper connection closed".into())
}

/// One live helper connection. Cheap to share by reference; all methods
/// take `&self` and concurrent requests are supported.
pub struct HelperSession {
    requests: mpsc::Sender<HelperRequest>,
    pending: Pending,
    next_id: AtomicU64,
    helper_version: String,
    helper_pid: u32,
    reader_task: tokio::task::JoinHandle<()>,
    writer_task: tokio::task::JoinHandle<()>,
}

impl HelperSession {
    /// Wraps `stream` and performs the handshake: a `Hello` request is
    /// sent and the helper's `Hello` reply (version and pid) is awaited.
    pub async fn from_stream<S>(stream: S) -> Result<HelperSession, SandboxError>
    where
        S: AsyncRead + AsyncWrite + Send + 'static,
    {
        let (read_half, write_half) = tokio::io::split(stream);
        let pending: Pending = Arc::new(Mutex::new(Some(HashMap::new())));
        let (requests, mut request_rx) = mpsc::channel::<HelperRequest>(64);

        let writer_task = tokio::spawn(async move {
            let mut writer = write_half;
            while let Some(request) = request_rx.recv().await {
                if write_json_line(&mut writer, &request).await.is_err() {
                    break;
                }
            }
        });

        let reader_pending = pending.clone();
        let reader_task = tokio::spawn(async move {
            let mut reader = BufReader::new(read_half);
            while let Ok(Some(response)) = read_json_line::<_, HelperResponse>(&mut reader).await {
                let waiter = reader_pending
                    .lock()
                    .expect("pending request map poisoned")
                    .as_mut()
                    .and_then(|map| map.remove(&response.id));
                if let Some(waiter) = waiter {
                    let result = response.result.map_err(SandboxError::Helper);
                    let _ = waiter.send(result);
                }
            }
            // Mark the session closed and fail every outstanding request.
            let drained = reader_pending
                .lock()
                .expect("pending request map poisoned")
                .take();
            if let Some(mut map) = drained {
                for (_, waiter) in map.drain() {
                    let _ = waiter.send(Err(closed_error()));
                }
            }
        });

        let mut session = HelperSession {
            requests,
            pending,
            next_id: AtomicU64::new(0),
            helper_version: String::new(),
            helper_pid: 0,
            reader_task,
            writer_task,
        };
        match session.request(HelperOp::Hello).await? {
            HelperPayload::Hello { version, pid } => {
                session.helper_version = version;
                session.helper_pid = pid;
                Ok(session)
            }
            other => Err(SandboxError::Setup(format!(
                "unexpected handshake reply from helper: {other:?}"
            ))),
        }
    }

    /// Sends one operation and awaits its response. Safe to call from
    /// multiple tasks at once; responses are matched by request id. A
    /// per-request failure reported by the helper becomes
    /// [`SandboxError::Helper`]; a dead connection becomes
    /// [`SandboxError::Unavailable`].
    pub async fn request(&self, op: HelperOp) -> Result<HelperPayload, SandboxError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (waiter_tx, waiter_rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().expect("pending request map poisoned");
            match pending.as_mut() {
                Some(map) => {
                    map.insert(id, waiter_tx);
                }
                None => return Err(closed_error()),
            }
        }
        if self.requests.send(HelperRequest { id, op }).await.is_err() {
            if let Some(map) = self
                .pending
                .lock()
                .expect("pending request map poisoned")
                .as_mut()
            {
                map.remove(&id);
            }
            return Err(closed_error());
        }
        match waiter_rx.await {
            Ok(result) => result,
            Err(_) => Err(closed_error()),
        }
    }

    /// Asks the helper to exit. The helper acknowledges and then
    /// terminates.
    pub async fn shutdown(&self) -> Result<(), SandboxError> {
        match self.request(HelperOp::Shutdown).await? {
            HelperPayload::Ack => Ok(()),
            other => Err(SandboxError::Helper(format!(
                "unexpected reply to shutdown: {other:?}"
            ))),
        }
    }

    /// Version the helper reported in its handshake.
    pub fn helper_version(&self) -> &str {
        &self.helper_version
    }

    /// Process id the helper reported in its handshake.
    pub fn helper_pid(&self) -> u32 {
        self.helper_pid
    }
}

impl Drop for HelperSession {
    fn drop(&mut self) {
        self.reader_task.abort();
        self.writer_task.abort();
    }
}

impl std::fmt::Debug for HelperSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HelperSession")
            .field("helper_version", &self.helper_version)
            .field("helper_pid", &self.helper_pid)
            .finish_non_exhaustive()
    }
}

/// A bound Unix socket waiting for the helper to connect. Created by
/// [`listen_unix`]; sandbox backends pass the socket path to the helper
/// (`unix:<path>`) and then call [`HelperListener::accept`].
#[cfg(unix)]
pub struct HelperListener {
    listener: tokio::net::UnixListener,
    accept_timeout: Duration,
    socket_path: PathBuf,
}

#[cfg(unix)]
impl HelperListener {
    /// The path the listener is bound to.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Waits for the helper to connect and completes the handshake. The
    /// timeout covers both the connection and the handshake.
    pub async fn accept(self) -> Result<HelperSession, SandboxError> {
        let path = self.socket_path.clone();
        let session = tokio::time::timeout(self.accept_timeout, async move {
            let (stream, _addr) = self.listener.accept().await.map_err(|e| {
                SandboxError::Setup(format!(
                    "accept on helper socket {} failed: {e}",
                    self.socket_path.display()
                ))
            })?;
            HelperSession::from_stream(stream).await
        })
        .await
        .map_err(|_| {
            SandboxError::Setup(format!(
                "helper did not connect to {} within {:?}",
                path.display(),
                self.accept_timeout
            ))
        })??;
        Ok(session)
    }
}

/// Binds a Unix socket for the helper to connect back to. Any stale file
/// at `socket_path` is removed first.
#[cfg(unix)]
pub async fn listen_unix(
    socket_path: &Path,
    accept_timeout: Duration,
) -> Result<HelperListener, SandboxError> {
    if socket_path.exists() {
        std::fs::remove_file(socket_path)?;
    }
    let listener = tokio::net::UnixListener::bind(socket_path).map_err(|e| {
        SandboxError::Setup(format!(
            "cannot bind helper socket {}: {e}",
            socket_path.display()
        ))
    })?;
    Ok(HelperListener {
        listener,
        accept_timeout,
        socket_path: socket_path.to_path_buf(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use silo_core::helper::b64;

    /// Starts a fake helper peer that answers `Hello`, echoes `ReadFile`
    /// paths back as file content, and answers the path "first" only
    /// after "second" has been answered (forcing out-of-order replies).
    fn spawn_reordering_peer() -> tokio::io::DuplexStream {
        let (near, far) = tokio::io::duplex(1 << 16);
        tokio::spawn(async move {
            let (read_half, mut write_half) = tokio::io::split(far);
            let mut reader = BufReader::new(read_half);
            let mut held: Option<u64> = None;
            while let Ok(Some(request)) = read_json_line::<_, HelperRequest>(&mut reader).await {
                match request.op {
                    HelperOp::Hello => {
                        let response = HelperResponse {
                            id: request.id,
                            result: Ok(HelperPayload::Hello {
                                version: "0.0.0-test".into(),
                                pid: 42,
                            }),
                        };
                        let _ = write_json_line(&mut write_half, &response).await;
                    }
                    HelperOp::ReadFile { path, .. } if path == "first" => {
                        held = Some(request.id);
                    }
                    HelperOp::ReadFile { path, .. } => {
                        let response = HelperResponse {
                            id: request.id,
                            result: Ok(HelperPayload::File {
                                content_b64: b64(path.as_bytes()),
                                truncated: false,
                            }),
                        };
                        let _ = write_json_line(&mut write_half, &response).await;
                        if let Some(held_id) = held.take() {
                            let response = HelperResponse {
                                id: held_id,
                                result: Ok(HelperPayload::File {
                                    content_b64: b64(b"first"),
                                    truncated: false,
                                }),
                            };
                            let _ = write_json_line(&mut write_half, &response).await;
                        }
                    }
                    HelperOp::Shutdown => {
                        let response = HelperResponse {
                            id: request.id,
                            result: Ok(HelperPayload::Ack),
                        };
                        let _ = write_json_line(&mut write_half, &response).await;
                        break;
                    }
                    _ => {
                        let response = HelperResponse {
                            id: request.id,
                            result: Err("unsupported in fake peer".into()),
                        };
                        let _ = write_json_line(&mut write_half, &response).await;
                    }
                }
            }
        });
        near
    }

    fn file_content(payload: HelperPayload) -> Vec<u8> {
        match payload {
            HelperPayload::File { content_b64, .. } => {
                silo_core::helper::unb64(&content_b64).unwrap()
            }
            other => panic!("expected File payload, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn handshake_records_version_and_pid() {
        let session = HelperSession::from_stream(spawn_reordering_peer())
            .await
            .unwrap();
        assert_eq!(session.helper_version(), "0.0.0-test");
        assert_eq!(session.helper_pid(), 42);
        session.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn out_of_order_responses_are_correlated_by_id() {
        let session = HelperSession::from_stream(spawn_reordering_peer())
            .await
            .unwrap();
        let read = |path: &str| {
            let path = path.to_string();
            let session = &session;
            async move {
                session
                    .request(HelperOp::ReadFile {
                        path,
                        offset: None,
                        limit: None,
                    })
                    .await
            }
        };
        // "first" is sent first but answered second by the peer.
        let (first, second) = tokio::join!(read("first"), read("second"));
        assert_eq!(file_content(first.unwrap()), b"first");
        assert_eq!(file_content(second.unwrap()), b"second");
    }

    #[tokio::test]
    async fn helper_error_becomes_sandbox_helper_error() {
        let session = HelperSession::from_stream(spawn_reordering_peer())
            .await
            .unwrap();
        let err = session
            .request(HelperOp::ListDir { path: "x".into() })
            .await
            .unwrap_err();
        match err {
            SandboxError::Helper(message) => assert!(message.contains("unsupported")),
            other => panic!("expected Helper error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn closed_connection_fails_pending_and_new_requests() {
        let (near, far) = tokio::io::duplex(1 << 16);
        // Answer the handshake, then close the connection.
        tokio::spawn(async move {
            let (read_half, mut write_half) = tokio::io::split(far);
            let mut reader = BufReader::new(read_half);
            if let Ok(Some(request)) = read_json_line::<_, HelperRequest>(&mut reader).await {
                let response = HelperResponse {
                    id: request.id,
                    result: Ok(HelperPayload::Hello {
                        version: "0".into(),
                        pid: 1,
                    }),
                };
                let _ = write_json_line(&mut write_half, &response).await;
            }
        });
        let session = HelperSession::from_stream(near).await.unwrap();
        let err = session
            .request(HelperOp::ListDir { path: "x".into() })
            .await
            .unwrap_err();
        assert!(matches!(err, SandboxError::Unavailable(_)), "got {err:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn listen_unix_accepts_a_helper_connection() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("helper.sock");
        let listener = listen_unix(&socket_path, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(listener.socket_path(), socket_path);

        let connect_path = socket_path.clone();
        tokio::spawn(async move {
            let stream = tokio::net::UnixStream::connect(&connect_path)
                .await
                .unwrap();
            let _ =
                silo_helper::serve_stream_with_config(stream, silo_helper::FetchConfig::default())
                    .await;
        });

        let session = listener.accept().await.unwrap();
        assert!(!session.helper_version().is_empty());
        session.shutdown().await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn listen_unix_times_out_when_nothing_connects() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("helper.sock");
        let listener = listen_unix(&socket_path, Duration::from_millis(50))
            .await
            .unwrap();
        let err = listener.accept().await.unwrap_err();
        match err {
            SandboxError::Setup(message) => assert!(message.contains("did not connect")),
            other => panic!("expected Setup error, got {other:?}"),
        }
    }
}
