//! Unix-socket-to-TCP forwarder.
//!
//! The gVisor sandbox runs with `--network=none`, so the only path out is
//! a Unix socket bind-mounted into the scratch space. The harness side
//! listens on that socket and pipes each accepted connection, in both
//! directions, to a fresh TCP connection on the egress proxy address.
//! This is the mirror of the helper-side relay
//! (`silo_helper::start_proxy_relay`), which turns in-sandbox loopback TCP
//! connections back into Unix-socket connections.

use std::net::SocketAddr;

use tokio::net::UnixListener;

/// A running forwarder. Dropping the handle stops accepting new
/// connections; established connections keep flowing until either side
/// closes.
pub struct UnixToTcpForwarder {
    accept_task: tokio::task::JoinHandle<()>,
}

impl Drop for UnixToTcpForwarder {
    fn drop(&mut self) {
        self.accept_task.abort();
    }
}

/// Accepts connections on `listener` and pipes each to a fresh TCP
/// connection on `target`.
pub fn start_unix_to_tcp_forwarder(
    listener: UnixListener,
    target: SocketAddr,
) -> UnixToTcpForwarder {
    let accept_task = tokio::spawn(async move {
        loop {
            let Ok((mut inbound, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let Ok(mut outbound) = tokio::net::TcpStream::connect(target).await else {
                    return;
                };
                let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
            });
        }
    });
    UnixToTcpForwarder { accept_task }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// TCP echo server on loopback; every connection gets its input copied
    /// back.
    async fn spawn_tcp_echo() -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
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
        addr
    }

    #[tokio::test]
    async fn forwarder_pipes_concurrent_connections_both_ways() {
        let target = spawn_tcp_echo().await;
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("proxy.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let _forwarder = start_unix_to_tcp_forwarder(listener, target);

        let mut first = tokio::net::UnixStream::connect(&socket_path).await.unwrap();
        let mut second = tokio::net::UnixStream::connect(&socket_path).await.unwrap();

        first.write_all(b"first payload").await.unwrap();
        second.write_all(b"second payload").await.unwrap();
        first.shutdown().await.unwrap();
        second.shutdown().await.unwrap();

        let mut first_echo = Vec::new();
        let mut second_echo = Vec::new();
        first.read_to_end(&mut first_echo).await.unwrap();
        second.read_to_end(&mut second_echo).await.unwrap();
        assert_eq!(first_echo, b"first payload");
        assert_eq!(second_echo, b"second payload");
    }

    #[tokio::test]
    async fn unreachable_target_closes_the_connection() {
        // Bind and drop a listener to get a port with nothing behind it.
        let unused = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = unused.local_addr().unwrap();
        drop(unused);

        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("proxy.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let _forwarder = start_unix_to_tcp_forwarder(listener, target);

        let mut stream = tokio::net::UnixStream::connect(&socket_path).await.unwrap();
        let mut buffer = Vec::new();
        // The forwarder fails to reach the target and drops its end; the
        // read terminates rather than hanging.
        stream.read_to_end(&mut buffer).await.unwrap();
        assert!(buffer.is_empty());
    }
}
