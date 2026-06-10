//! The connection task: connects to the interactive frontend with a pinned
//! certificate, authenticates, requests the event backlog, pumps server
//! messages to the app, forwards app commands to the server, and reconnects
//! with backoff, resuming from the last seen event sequence number.
//!
//! Also holds the client-side persistence for remote connections: the
//! known-hosts fingerprint store and the per-host Ed25519 key id.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ed25519_dalek::SigningKey;
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use silo_core::error::FrontendError;
use silo_core::protocol::{ClientMessage, ServerMessage};
use silo_frontend::client::{self, ClientStream};

/// Messages from the connection task to the app.
#[derive(Debug)]
pub enum NetEvent {
    Connecting {
        attempt: u32,
    },
    Connected {
        harness_id: String,
    },
    Server(ServerMessage),
    Disconnected {
        reason: String,
        retry_in_secs: u64,
    },
    /// Unrecoverable failure (authentication rejected); the task has ended.
    Fatal {
        message: String,
    },
}

/// How to authenticate once the TLS connection is up.
pub enum AuthSpec {
    /// Filesystem-shared token, for local harnesses.
    LocalToken { token: String },
    /// One-time pairing code; the key is registered with the server and the
    /// spec becomes [`AuthSpec::Key`] for reconnects.
    Pair {
        code: String,
        key: SigningKey,
        client_name: String,
    },
    /// Challenge-signature login with a previously registered key.
    Key { key: SigningKey, key_id: String },
}

pub struct ConnectTarget {
    /// `host:port` or a full `wss://` URL.
    pub addr: String,
    /// SHA-256 fingerprint of the server certificate, hex.
    pub fingerprint: String,
    pub auth: AuthSpec,
    /// State directory for persisting the key id and the host fingerprint
    /// after a successful remote authentication. `None` for local targets.
    pub persist_state_dir: Option<PathBuf>,
}

// --- known-hosts and key persistence ---

/// Fingerprints of previously seen servers, keyed by address.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct KnownHosts {
    #[serde(default)]
    pub hosts: BTreeMap<String, String>,
}

pub fn known_hosts_path(state_dir: &Path) -> PathBuf {
    silo_core::paths::client_keys_dir(state_dir).join("known-hosts.json")
}

pub fn load_known_hosts(state_dir: &Path) -> Result<KnownHosts, FrontendError> {
    let path = known_hosts_path(state_dir);
    if !path.exists() {
        return Ok(KnownHosts::default());
    }
    let text = std::fs::read_to_string(&path)?;
    serde_json::from_str(&text).map_err(|e| {
        FrontendError::Setup(format!(
            "unreadable known-hosts file {}: {e}",
            path.display()
        ))
    })
}

pub fn save_known_hosts(state_dir: &Path, hosts: &KnownHosts) -> Result<(), FrontendError> {
    let path = known_hosts_path(state_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(hosts)
        .map_err(|e| FrontendError::Setup(format!("unserializable known-hosts: {e}")))?;
    std::fs::write(&path, text)?;
    Ok(())
}

/// Records (or refreshes) the fingerprint for one address.
pub fn remember_host(state_dir: &Path, addr: &str, fingerprint: &str) -> Result<(), FrontendError> {
    let mut hosts = load_known_hosts(state_dir)?;
    hosts
        .hosts
        .insert(addr.to_string(), fingerprint.to_string());
    save_known_hosts(state_dir, &hosts)
}

/// The stored fingerprint for an address, if any.
pub fn lookup_host(state_dir: &Path, addr: &str) -> Result<Option<String>, FrontendError> {
    Ok(load_known_hosts(state_dir)?.hosts.get(addr).cloned())
}

/// Filename-safe stem derived from a `host:port` address.
pub fn host_file_stem(addr: &str) -> String {
    addr.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Path of the client's private key for one server address.
pub fn key_path(state_dir: &Path, addr: &str) -> PathBuf {
    silo_core::paths::client_keys_dir(state_dir).join(format!("{}.pem", host_file_stem(addr)))
}

/// Path of the stored key id for one server address.
pub fn key_id_path(state_dir: &Path, addr: &str) -> PathBuf {
    silo_core::paths::client_keys_dir(state_dir).join(format!("{}.key-id", host_file_stem(addr)))
}

pub fn save_key_id(state_dir: &Path, addr: &str, key_id: &str) -> Result<(), FrontendError> {
    let path = key_id_path(state_dir, addr);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, key_id)?;
    Ok(())
}

pub fn load_key_id(state_dir: &Path, addr: &str) -> Result<Option<String>, FrontendError> {
    let path = key_id_path(state_dir, addr);
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)?;
    Ok(Some(text.trim().to_string()))
}

// --- the connection task ---

/// Reconnect delay for the given attempt number: 1s, 2s, 4s, ... capped at
/// 30 seconds.
pub fn backoff_delay(attempt: u32) -> Duration {
    let secs = 1u64.checked_shl(attempt).unwrap_or(u64::MAX).min(30);
    Duration::from_secs(secs)
}

/// Spawns the connection task. Returns the command sender (messages for the
/// server), the event receiver (messages for the app), and the task handle.
pub fn spawn(
    target: ConnectTarget,
) -> (
    mpsc::UnboundedSender<ClientMessage>,
    mpsc::UnboundedReceiver<NetEvent>,
    tokio::task::JoinHandle<()>,
) {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (evt_tx, evt_rx) = mpsc::unbounded_channel();
    let handle = tokio::spawn(run(target, cmd_rx, evt_tx));
    (cmd_tx, evt_rx, handle)
}

enum PumpEnd {
    /// The server announced shutdown; no reconnect.
    ServerShutdown,
    /// The app side dropped the command channel; the client is exiting.
    AppClosed,
    /// The connection dropped; reconnect.
    Lost(String),
}

async fn run(
    mut target: ConnectTarget,
    mut cmd_rx: mpsc::UnboundedReceiver<ClientMessage>,
    evt_tx: mpsc::UnboundedSender<NetEvent>,
) {
    let mut attempt: u32 = 0;
    let mut last_seq: Option<u64> = None;
    loop {
        if evt_tx.send(NetEvent::Connecting { attempt }).is_err() {
            return;
        }
        match connect_and_auth(&mut target).await {
            Ok((mut stream, harness_id)) => {
                attempt = 0;
                if evt_tx.send(NetEvent::Connected { harness_id }).is_err() {
                    return;
                }
                let from_seq = last_seq.map(|s| s + 1).unwrap_or(0);
                if let Err(e) =
                    client::send_client(&mut stream, &ClientMessage::RequestEvents { from_seq })
                        .await
                {
                    let _ = evt_tx.send(NetEvent::Disconnected {
                        reason: e.to_string(),
                        retry_in_secs: backoff_delay(attempt).as_secs(),
                    });
                } else {
                    match pump(stream, &mut last_seq, &mut cmd_rx, &evt_tx).await {
                        PumpEnd::ServerShutdown | PumpEnd::AppClosed => return,
                        PumpEnd::Lost(reason) => {
                            let _ = evt_tx.send(NetEvent::Disconnected {
                                reason,
                                retry_in_secs: backoff_delay(attempt).as_secs(),
                            });
                        }
                    }
                }
            }
            Err(FrontendError::Auth(message)) => {
                let _ = evt_tx.send(NetEvent::Fatal {
                    message: format!("authentication failed: {message}"),
                });
                return;
            }
            Err(e) => {
                let _ = evt_tx.send(NetEvent::Disconnected {
                    reason: e.to_string(),
                    retry_in_secs: backoff_delay(attempt).as_secs(),
                });
            }
        }
        tokio::time::sleep(backoff_delay(attempt)).await;
        attempt = attempt.saturating_add(1);
    }
}

/// Connects with the pinned fingerprint and authenticates according to the
/// target's auth spec. After a successful pairing the spec is rewritten to
/// challenge-signature login and the key id and host fingerprint are
/// persisted.
async fn connect_and_auth(
    target: &mut ConnectTarget,
) -> Result<(ClientStream, String), FrontendError> {
    let (mut stream, hello) = client::connect(&target.addr, &target.fingerprint).await?;
    let harness_id = match hello {
        ServerMessage::Hello { harness_id, .. } => harness_id,
        _ => String::new(),
    };
    match &target.auth {
        AuthSpec::LocalToken { token } => {
            client::authenticate_local(&mut stream, token).await?;
        }
        AuthSpec::Pair {
            code,
            key,
            client_name,
        } => {
            let ok = client::pair(&mut stream, code, client_name, &key.verifying_key()).await?;
            let key_id = ok.key_id.ok_or_else(|| {
                FrontendError::Auth("the server did not assign a key id at pairing".into())
            })?;
            let key = key.clone();
            if let Some(state_dir) = &target.persist_state_dir {
                save_key_id(state_dir, &target.addr, &key_id)?;
                remember_host(state_dir, &target.addr, &target.fingerprint)?;
            }
            target.auth = AuthSpec::Key { key, key_id };
        }
        AuthSpec::Key { key, key_id } => {
            client::login_with_key(&mut stream, key_id, key).await?;
            if let Some(state_dir) = &target.persist_state_dir {
                remember_host(state_dir, &target.addr, &target.fingerprint)?;
            }
        }
    }
    Ok((stream, harness_id))
}

async fn pump(
    mut stream: ClientStream,
    last_seq: &mut Option<u64>,
    cmd_rx: &mut mpsc::UnboundedReceiver<ClientMessage>,
    evt_tx: &mpsc::UnboundedSender<NetEvent>,
) -> PumpEnd {
    loop {
        tokio::select! {
            incoming = stream.next() => match incoming {
                None => return PumpEnd::Lost("connection closed".into()),
                Some(Err(e)) => return PumpEnd::Lost(format!("receive failed: {e}")),
                Some(Ok(Message::Close(_))) => {
                    return PumpEnd::Lost("server closed the connection".into());
                }
                Some(Ok(Message::Text(text))) => {
                    let message: ServerMessage = match serde_json::from_str(text.as_str()) {
                        Ok(m) => m,
                        Err(e) => return PumpEnd::Lost(format!("unparseable server message: {e}")),
                    };
                    track_seq(&message, last_seq);
                    let shutting_down = matches!(message, ServerMessage::ShuttingDown { .. });
                    if evt_tx.send(NetEvent::Server(message)).is_err() {
                        return PumpEnd::AppClosed;
                    }
                    if shutting_down {
                        return PumpEnd::ServerShutdown;
                    }
                }
                Some(Ok(_)) => {}
            },
            command = cmd_rx.recv() => match command {
                None => return PumpEnd::AppClosed,
                Some(message) => {
                    let text = match serde_json::to_string(&message) {
                        Ok(t) => t,
                        Err(e) => return PumpEnd::Lost(format!("unserializable client message: {e}")),
                    };
                    if let Err(e) = stream.send(Message::text(text)).await {
                        return PumpEnd::Lost(format!("send failed: {e}"));
                    }
                }
            },
        }
    }
}

/// Tracks the highest event sequence number seen, so a reconnect can resume
/// the backlog from the right place.
fn track_seq(message: &ServerMessage, last_seq: &mut Option<u64>) {
    let observe = |last: &mut Option<u64>, seq: u64| {
        if last.map(|l| seq > l).unwrap_or(true) {
            *last = Some(seq);
        }
    };
    match message {
        ServerMessage::Event { event } => observe(last_seq, event.seq),
        ServerMessage::Events { events } => {
            for event in events {
                observe(last_seq, event.seq);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use silo_core::clock::Timestamp;
    use silo_core::event::{Event, EventPayload};

    #[test]
    fn known_hosts_roundtrip_through_the_state_dir() {
        let state = tempfile::tempdir().unwrap();
        assert_eq!(
            load_known_hosts(state.path()).unwrap(),
            KnownHosts::default()
        );

        remember_host(state.path(), "example.com:7777", &"ab".repeat(32)).unwrap();
        remember_host(state.path(), "other.example:1234", &"cd".repeat(32)).unwrap();

        let loaded = load_known_hosts(state.path()).unwrap();
        assert_eq!(loaded.hosts.len(), 2);
        assert_eq!(
            lookup_host(state.path(), "example.com:7777").unwrap(),
            Some("ab".repeat(32))
        );
        assert_eq!(lookup_host(state.path(), "unknown:1").unwrap(), None);

        // Re-recording an address replaces its fingerprint.
        remember_host(state.path(), "example.com:7777", &"ef".repeat(32)).unwrap();
        assert_eq!(
            lookup_host(state.path(), "example.com:7777").unwrap(),
            Some("ef".repeat(32))
        );
    }

    #[test]
    fn corrupt_known_hosts_is_an_error_not_a_panic() {
        let state = tempfile::tempdir().unwrap();
        let path = known_hosts_path(state.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{not json").unwrap();
        assert!(load_known_hosts(state.path()).is_err());
    }

    #[test]
    fn host_file_stems_are_filename_safe() {
        assert_eq!(host_file_stem("example.com:7777"), "example.com_7777");
        assert_eq!(host_file_stem("10.0.0.1:80"), "10.0.0.1_80");
        assert_eq!(host_file_stem("a/b\\c"), "a_b_c");
    }

    #[test]
    fn key_id_roundtrips() {
        let state = tempfile::tempdir().unwrap();
        assert_eq!(load_key_id(state.path(), "h:1").unwrap(), None);
        save_key_id(state.path(), "h:1", "key-42").unwrap();
        assert_eq!(
            load_key_id(state.path(), "h:1").unwrap(),
            Some("key-42".into())
        );
    }

    #[test]
    fn backoff_grows_and_caps() {
        assert_eq!(backoff_delay(0), Duration::from_secs(1));
        assert_eq!(backoff_delay(1), Duration::from_secs(2));
        assert_eq!(backoff_delay(2), Duration::from_secs(4));
        assert_eq!(backoff_delay(4), Duration::from_secs(16));
        assert_eq!(backoff_delay(5), Duration::from_secs(30));
        assert_eq!(backoff_delay(63), Duration::from_secs(30));
        assert_eq!(backoff_delay(64), Duration::from_secs(30));
    }

    #[test]
    fn seq_tracking_uses_the_highest_seen() {
        let event = |seq| Event {
            seq,
            time: Timestamp {
                logical: seq,
                wall_ms: None,
            },
            payload: EventPayload::AwaitingInput,
        };
        let mut last = None;
        track_seq(
            &ServerMessage::Events {
                events: vec![event(0), event(1), event(2)],
            },
            &mut last,
        );
        assert_eq!(last, Some(2));
        track_seq(&ServerMessage::Event { event: event(7) }, &mut last);
        assert_eq!(last, Some(7));
        // Stale duplicates do not move the cursor backwards.
        track_seq(&ServerMessage::Event { event: event(3) }, &mut last);
        assert_eq!(last, Some(7));
        track_seq(&ServerMessage::Pong { nonce: 1 }, &mut last);
        assert_eq!(last, Some(7));
    }
}
