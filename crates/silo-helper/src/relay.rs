//! TCP-to-Unix-socket relay.
//!
//! Some sandbox backends only expose a Unix socket as the path out of the
//! sandbox. The relay listens on a loopback TCP port inside the sandbox and
//! pipes each accepted connection, in both directions, to a fresh
//! connection on that Unix socket. Sandboxed programs then reach the
//! egress proxy by connecting to `127.0.0.1:<port>`.

use std::net::SocketAddr;
use std::path::PathBuf;

use tokio::net::{TcpListener, UnixStream};

/// Parses the `SILO_PROXY_RELAY` value: `<unix socket path>:<port>`.
pub fn parse_relay_spec(spec: &str) -> Result<(PathBuf, u16), String> {
    let (path, port) = spec.rsplit_once(':').ok_or_else(|| {
        format!("invalid SILO_PROXY_RELAY value {spec:?} (expected <socket path>:<port>)")
    })?;
    if path.is_empty() {
        return Err(format!(
            "invalid SILO_PROXY_RELAY value {spec:?} (empty socket path)"
        ));
    }
    let port: u16 = port
        .parse()
        .map_err(|_| format!("invalid SILO_PROXY_RELAY value {spec:?} (bad port {port:?})"))?;
    Ok((PathBuf::from(path), port))
}

/// A running relay. Dropping the handle stops accepting new connections;
/// established connections keep flowing until either side closes.
pub struct ProxyRelay {
    local_addr: SocketAddr,
    accept_task: tokio::task::JoinHandle<()>,
}

impl ProxyRelay {
    /// The bound loopback address (useful when port 0 was requested).
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
}

impl Drop for ProxyRelay {
    fn drop(&mut self) {
        self.accept_task.abort();
    }
}

/// Binds `127.0.0.1:<port>` and relays each accepted connection to a fresh
/// connection on `socket_path`.
pub async fn start_proxy_relay(socket_path: PathBuf, port: u16) -> Result<ProxyRelay, String> {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|e| format!("cannot bind relay port 127.0.0.1:{port}: {e}"))?;
    let local_addr = listener
        .local_addr()
        .map_err(|e| format!("cannot read relay address: {e}"))?;
    let accept_task = tokio::spawn(async move {
        loop {
            let Ok((mut inbound, _)) = listener.accept().await else {
                break;
            };
            let socket_path = socket_path.clone();
            tokio::spawn(async move {
                let Ok(mut outbound) = UnixStream::connect(&socket_path).await else {
                    return;
                };
                let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
            });
        }
    });
    Ok(ProxyRelay {
        local_addr,
        accept_task,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn relay_spec_parses_path_and_port() {
        let (path, port) = parse_relay_spec("/tmp/proxy.sock:8123").unwrap();
        assert_eq!(path, PathBuf::from("/tmp/proxy.sock"));
        assert_eq!(port, 8123);
    }

    #[test]
    fn relay_spec_rejects_malformed_values() {
        assert!(parse_relay_spec("no-port-here").is_err());
        assert!(parse_relay_spec(":1234").is_err());
        assert!(parse_relay_spec("/tmp/proxy.sock:notaport").is_err());
        assert!(parse_relay_spec("/tmp/proxy.sock:99999").is_err());
    }

    /// Echo server on a Unix socket: every connection gets its input
    /// copied back.
    fn spawn_unix_echo(socket_path: &std::path::Path) {
        let listener = std::os::unix::net::UnixListener::bind(socket_path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let listener = tokio::net::UnixListener::from_std(listener).unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let (mut reader, mut writer) = stream.split();
                    let _ = tokio::io::copy(&mut reader, &mut writer).await;
                    let _ = writer.shutdown().await;
                });
            }
        });
    }

    #[tokio::test]
    async fn relay_pipes_two_concurrent_connections_both_ways() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("echo.sock");
        spawn_unix_echo(&socket_path);

        let relay = start_proxy_relay(socket_path, 0).await.unwrap();
        let addr = relay.local_addr();

        let mut first = tokio::net::TcpStream::connect(addr).await.unwrap();
        let mut second = tokio::net::TcpStream::connect(addr).await.unwrap();

        // Interleave writes on both connections before reading anything.
        first.write_all(b"first connection payload").await.unwrap();
        second
            .write_all(b"second connection payload")
            .await
            .unwrap();
        first.shutdown().await.unwrap();
        second.shutdown().await.unwrap();

        let mut first_echo = Vec::new();
        let mut second_echo = Vec::new();
        first.read_to_end(&mut first_echo).await.unwrap();
        second.read_to_end(&mut second_echo).await.unwrap();
        assert_eq!(first_echo, b"first connection payload");
        assert_eq!(second_echo, b"second connection payload");
    }
}
