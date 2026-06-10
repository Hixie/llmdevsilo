//! Runtime of the in-sandbox helper process.
//!
//! The helper is the untrusted component that runs inside every sandbox.
//! It connects back to the harness (Unix socket or TCP loopback, per the
//! `SILO_HELPER_CONNECT` environment variable), then serves
//! `silo_core::helper::HelperRequest` messages: executing shell commands,
//! reading/writing/editing files, listing directories, and making HTTP
//! requests through the egress proxy. Everything it does is subject to the
//! sandbox policy — it has no privileges beyond any other sandboxed
//! process.
//!
//! Requests are handled concurrently: the read loop spawns one task per
//! request and a single writer task serializes the responses, so responses
//! can arrive in any order and are correlated by request id.

mod fetch;
mod ops;
mod relay;

pub use fetch::FetchConfig;
pub use relay::{parse_relay_spec, start_proxy_relay, ProxyRelay};

use std::sync::Arc;

use silo_core::helper::{write_json_line, HelperOp, HelperPayload, HelperRequest, HelperResponse};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, BufReader};
use tokio::sync::{mpsc, oneshot};

/// Entry point used by the `silo-helper` binary. `connect` is the address
/// to reach the harness: `unix:/path/to.sock` or `tcp:127.0.0.1:port`.
///
/// When the `SILO_PROXY_RELAY` environment variable is set to
/// `<unix socket path>:<port>`, a TCP-to-Unix-socket relay is started on
/// `127.0.0.1:<port>` before serving (used by backends where the only path
/// out of the sandbox is a Unix socket).
pub async fn run(connect: &str) -> Result<(), String> {
    let _relay = match std::env::var("SILO_PROXY_RELAY") {
        Ok(spec) if !spec.trim().is_empty() => {
            let (socket_path, port) = relay::parse_relay_spec(&spec)?;
            Some(relay::start_proxy_relay(socket_path, port).await?)
        }
        _ => None,
    };
    let fetch_config = FetchConfig::from_env();
    if let Some(path) = connect.strip_prefix("unix:") {
        let stream = tokio::net::UnixStream::connect(path)
            .await
            .map_err(|e| format!("cannot connect to unix socket {path}: {e}"))?;
        return serve_stream_with_config(stream, fetch_config).await;
    }
    if let Some(addr) = connect.strip_prefix("tcp:") {
        let stream = tokio::net::TcpStream::connect(addr)
            .await
            .map_err(|e| format!("cannot connect to {addr}: {e}"))?;
        return serve_stream_with_config(stream, fetch_config).await;
    }
    Err(format!(
        "invalid connect string {connect:?} (expected unix:<path> or tcp:<host:port>)"
    ))
}

/// Serves the helper protocol on `stream`, reading the `Fetch`
/// configuration (proxy address and CA certificate path) from the
/// environment. Returns when the harness sends `Shutdown` or closes the
/// stream.
pub async fn serve_stream<S>(stream: S) -> Result<(), String>
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    serve_stream_with_config(stream, FetchConfig::from_env()).await
}

/// Serves the helper protocol on `stream` with an explicit `Fetch`
/// configuration. Tests use this to control proxying without touching
/// process environment variables.
pub async fn serve_stream_with_config<S>(stream: S, fetch_config: FetchConfig) -> Result<(), String>
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let (read_half, write_half) = tokio::io::split(stream);
    let (tx, rx) = mpsc::channel::<Outgoing>(64);
    let _writer_task = tokio::spawn(write_loop(write_half, rx));
    let state = Arc::new(ops::ServeState::new(fetch_config));
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .await
            .map_err(|e| format!("read error: {e}"))?;
        if n == 0 {
            break;
        }
        if line.trim().is_empty() {
            continue;
        }
        // A line that is not valid JSON is skipped; a JSON line that is not
        // a valid request gets a per-request error when it carries an id.
        // Bad input never terminates the serve loop.
        let value: serde_json::Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let request: HelperRequest = match serde_json::from_value(value.clone()) {
            Ok(request) => request,
            Err(e) => {
                if let Some(id) = value.get("id").and_then(|v| v.as_u64()) {
                    let response = HelperResponse {
                        id,
                        result: Err(format!("malformed request: {e}")),
                    };
                    let _ = tx
                        .send(Outgoing {
                            response,
                            done: None,
                        })
                        .await;
                }
                continue;
            }
        };
        match request.op {
            HelperOp::Shutdown => {
                let (done_tx, done_rx) = oneshot::channel();
                let response = HelperResponse {
                    id: request.id,
                    result: Ok(HelperPayload::Ack),
                };
                let _ = tx
                    .send(Outgoing {
                        response,
                        done: Some(done_tx),
                    })
                    .await;
                let _ = done_rx.await;
                break;
            }
            op => {
                let tx = tx.clone();
                let state = state.clone();
                let id = request.id;
                tokio::spawn(async move {
                    let result = ops::handle_op(&state, op).await;
                    let response = HelperResponse { id, result };
                    let _ = tx
                        .send(Outgoing {
                            response,
                            done: None,
                        })
                        .await;
                });
            }
        }
    }
    Ok(())
}

struct Outgoing {
    response: HelperResponse,
    /// Signalled after the response has been written, so `Shutdown` can
    /// acknowledge before the serve loop exits.
    done: Option<oneshot::Sender<()>>,
}

async fn write_loop<W>(mut writer: W, mut rx: mpsc::Receiver<Outgoing>)
where
    W: AsyncWrite + Unpin,
{
    while let Some(outgoing) = rx.recv().await {
        let ok = write_json_line(&mut writer, &outgoing.response)
            .await
            .is_ok();
        if let Some(done) = outgoing.done {
            let _ = done.send(());
        }
        if !ok {
            break;
        }
    }
}
