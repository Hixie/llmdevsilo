//! Per-connection handling for the interactive frontend: TLS and WebSocket
//! handshakes, authentication, the outgoing event pump, and dispatch of
//! client messages.

use std::sync::Arc;
use std::time::Duration;

use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use rand::RngCore;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc};
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

use silo_core::event::{Event, EventPayload, FileOrigin};
use silo_core::helper::{b64, unb64};
use silo_core::protocol::{AuthRequest, ClientMessage, ServerMessage};
use silo_core::traits::FrontendCommand;

use super::auth;
use super::http;
use super::{QuestionOutcome, Shared};

type WsStream = WebSocketStream<http::PrefixedStream<tokio_rustls::server::TlsStream<TcpStream>>>;

/// Each message of the authentication phase must arrive within this window.
/// The HTTP request head after the TLS handshake shares the same limit.
const AUTH_TIMEOUT: Duration = Duration::from_secs(30);

/// Accepts connections until shutdown, spawning a handler task per client.
pub(super) async fn accept_loop(listener: TcpListener, acceptor: TlsAcceptor, shared: Arc<Shared>) {
    let mut shutdown_rx = shared.shutdown_tx.subscribe();
    if *shutdown_rx.borrow_and_update() {
        return;
    }
    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => break,
            accepted = listener.accept() => match accepted {
                Ok((tcp, _peer)) => {
                    let task = tokio::spawn(handle_connection(tcp, acceptor.clone(), shared.clone()));
                    shared
                        .conn_tasks
                        .lock()
                        .expect("connection task list poisoned")
                        .push(task);
                }
                Err(_) => continue,
            },
        }
    }
}

async fn handle_connection(tcp: TcpStream, acceptor: TlsAcceptor, shared: Arc<Shared>) {
    let mut tls = match acceptor.accept(tcp).await {
        Ok(tls) => tls,
        Err(_) => return,
    };
    // Peek the HTTP request head so plain browser navigations get a
    // landing page instead of a WebSocket protocol error. The bytes are
    // replayed into the WebSocket handshake through PrefixedStream.
    let head = match tokio::time::timeout(AUTH_TIMEOUT, http::read_request_head(&mut tls)).await {
        Ok(Some(head)) => head,
        Ok(None) | Err(_) => return,
    };
    if !http::is_websocket_upgrade(&head) {
        let response = http::landing_page_response(&shared.harness_id, &head, shared.listen_addr);
        let _ = tls.write_all(response.as_bytes()).await;
        let _ = tls.shutdown().await;
        return;
    }
    let prefixed = http::PrefixedStream::new(head, tls);
    let mut ws = match tokio_tungstenite::accept_async(prefixed).await {
        Ok(ws) => ws,
        Err(_) => return,
    };
    let hello = ServerMessage::Hello {
        harness_id: shared.harness_id.clone(),
        protocol_version: silo_core::PROTOCOL_VERSION,
    };
    if send_message(&mut ws, &hello).await.is_err() {
        return;
    }
    let Some(outcome) = authenticate(&mut ws, &shared).await else {
        return;
    };
    let client_id = uuid::Uuid::new_v4().to_string();
    // Subscribing before reading next_seq guarantees that every event with
    // seq >= next_seq reaches this client through the pump.
    let events_rx = shared.bus.subscribe();
    let next_seq = shared.bus.next_seq();
    let auth_ok = ServerMessage::AuthOk {
        client_id: client_id.clone(),
        key_id: outcome.key_id,
        next_seq,
    };
    if send_message(&mut ws, &auth_ok).await.is_err() {
        return;
    }

    let (sink, stream) = ws.split();
    let (out_tx, out_rx) = mpsc::unbounded_channel();
    let writer = tokio::spawn(write_outgoing(sink, out_rx, shared.clone()));
    let pump = tokio::spawn(pump_events(
        events_rx,
        next_seq,
        out_tx.clone(),
        shared.clone(),
    ));
    read_incoming(
        stream,
        &out_tx,
        &shared,
        &client_id,
        outcome.client_name.as_deref(),
    )
    .await;
    pump.abort();
    // Dropping the last sender lets the writer drain and close the socket.
    drop(out_tx);
    let _ = writer.await;
}

struct AuthOutcome {
    key_id: Option<String>,
    /// Display name registered at pairing; `None` for local-token clients.
    client_name: Option<String>,
}

/// Runs the authentication phase. On success returns the outcome; on
/// failure sends `AuthError`, closes the socket, and returns `None`.
async fn authenticate(ws: &mut WsStream, shared: &Arc<Shared>) -> Option<AuthOutcome> {
    let mut shutdown_rx = shared.shutdown_tx.subscribe();
    if *shutdown_rx.borrow_and_update() {
        return None;
    }
    let mut pending_challenge: Option<(String, [u8; 32])> = None;
    loop {
        let frame = tokio::select! {
            _ = shutdown_rx.changed() => return None,
            timed = tokio::time::timeout(AUTH_TIMEOUT, ws.next()) => match timed {
                Err(_) => return deny(ws, "authentication timed out").await,
                Ok(None) | Ok(Some(Err(_))) => return None,
                Ok(Some(Ok(frame))) => frame,
            },
        };
        let text = match frame {
            Message::Text(text) => text,
            Message::Close(_) => return None,
            _ => continue,
        };
        let auth = match serde_json::from_str::<ClientMessage>(text.as_str()) {
            Ok(ClientMessage::Authenticate { auth }) => auth,
            Ok(_) => return deny(ws, "authentication required").await,
            Err(_) => return deny(ws, "malformed message").await,
        };
        match auth {
            AuthRequest::LocalToken { token } => {
                let supplied = auth::sha256_digest(token.as_bytes());
                return if auth::constant_time_eq(&supplied, &shared.token_digest) {
                    Some(AuthOutcome {
                        key_id: None,
                        client_name: None,
                    })
                } else {
                    deny(ws, "invalid token").await
                };
            }
            AuthRequest::Pair {
                code,
                public_key_b64,
                client_name,
            } => {
                let redeemed = shared
                    .pairing
                    .lock()
                    .expect("pairing codes poisoned")
                    .redeem(&code);
                if !redeemed {
                    return deny(ws, "invalid or expired pairing code").await;
                }
                if auth::parse_public_key(&public_key_b64).is_none() {
                    return deny(ws, "invalid public key").await;
                }
                let added = shared
                    .keys
                    .lock()
                    .expect("authorized keys poisoned")
                    .add(&public_key_b64, &client_name);
                return match added {
                    Ok(key_id) => Some(AuthOutcome {
                        key_id: Some(key_id),
                        client_name: Some(client_name).filter(|name| !name.is_empty()),
                    }),
                    Err(_) => deny(ws, "could not persist the public key").await,
                };
            }
            AuthRequest::Challenge { key_id } => {
                let known = shared
                    .keys
                    .lock()
                    .expect("authorized keys poisoned")
                    .verifying_key(&key_id)
                    .is_some();
                if !known {
                    return deny(ws, "unknown key").await;
                }
                let mut challenge = [0u8; 32];
                rand::rngs::OsRng.fill_bytes(&mut challenge);
                let reply = ServerMessage::AuthChallenge {
                    challenge_b64: b64(&challenge),
                };
                pending_challenge = Some((key_id, challenge));
                if send_message(ws, &reply).await.is_err() {
                    return None;
                }
            }
            AuthRequest::Signature {
                key_id,
                signature_b64,
            } => {
                let Some((pending_id, challenge)) = pending_challenge.take() else {
                    return deny(ws, "no pending challenge").await;
                };
                if pending_id != key_id {
                    return deny(ws, "challenge does not match this key").await;
                }
                let verifying_key = shared
                    .keys
                    .lock()
                    .expect("authorized keys poisoned")
                    .verifying_key(&key_id);
                let Some(verifying_key) = verifying_key else {
                    return deny(ws, "unknown key").await;
                };
                let valid = unb64(&signature_b64)
                    .ok()
                    .and_then(|bytes| ed25519_dalek::Signature::from_slice(&bytes).ok())
                    .is_some_and(|signature| {
                        use ed25519_dalek::Verifier;
                        verifying_key.verify(&challenge, &signature).is_ok()
                    });
                return if valid {
                    let client_name = shared
                        .keys
                        .lock()
                        .expect("authorized keys poisoned")
                        .client_name(&key_id);
                    Some(AuthOutcome {
                        key_id: Some(key_id),
                        client_name,
                    })
                } else {
                    deny(ws, "invalid signature").await
                };
            }
        }
    }
}

async fn deny(ws: &mut WsStream, message: &str) -> Option<AuthOutcome> {
    let reply = ServerMessage::AuthError {
        message: message.into(),
    };
    let _ = send_message(ws, &reply).await;
    let _ = ws.close(None).await;
    None
}

/// Owns the write half: serializes queued messages, and on shutdown drains
/// the queue, announces `ShuttingDown`, and closes the socket.
async fn write_outgoing(
    mut sink: SplitSink<WsStream, Message>,
    mut out_rx: mpsc::UnboundedReceiver<ServerMessage>,
    shared: Arc<Shared>,
) {
    let mut shutdown_rx = shared.shutdown_tx.subscribe();
    let announce = if *shutdown_rx.borrow_and_update() {
        true
    } else {
        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => break true,
                item = out_rx.recv() => match item {
                    Some(message) => {
                        if send_to_sink(&mut sink, &message).await.is_err() {
                            break false;
                        }
                    }
                    None => break *shutdown_rx.borrow(),
                },
            }
        }
    };
    if announce {
        while let Ok(message) = out_rx.try_recv() {
            if send_to_sink(&mut sink, &message).await.is_err() {
                break;
            }
        }
        let message = shared
            .shutdown_message
            .lock()
            .expect("shutdown message poisoned")
            .clone();
        let _ = send_to_sink(&mut sink, &ServerMessage::ShuttingDown { message }).await;
    }
    let _ = sink.close().await;
}

/// Forwards every bus event at or after `next` to the client, refilling
/// from bus history when the broadcast channel lags.
async fn pump_events(
    mut events_rx: broadcast::Receiver<Event>,
    mut next: u64,
    out_tx: mpsc::UnboundedSender<ServerMessage>,
    shared: Arc<Shared>,
) {
    let mut shutdown_rx = shared.shutdown_tx.subscribe();
    if *shutdown_rx.borrow_and_update() {
        return;
    }
    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => break,
            received = events_rx.recv() => match received {
                Ok(event) => {
                    if event.seq >= next {
                        next = event.seq + 1;
                        if out_tx.send(ServerMessage::Event { event }).is_err() {
                            break;
                        }
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    for event in shared.bus.since(next) {
                        next = event.seq + 1;
                        if out_tx.send(ServerMessage::Event { event }).is_err() {
                            return;
                        }
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
        }
    }
}

async fn read_incoming(
    mut stream: SplitStream<WsStream>,
    out_tx: &mpsc::UnboundedSender<ServerMessage>,
    shared: &Arc<Shared>,
    client_id: &str,
    client_name: Option<&str>,
) {
    let mut shutdown_rx = shared.shutdown_tx.subscribe();
    if *shutdown_rx.borrow_and_update() {
        return;
    }
    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => break,
            frame = stream.next() => {
                let message = match frame {
                    None | Some(Err(_)) => break,
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<ClientMessage>(text.as_str()) {
                            Ok(message) => message,
                            Err(_) => {
                                let _ = out_tx.send(ServerMessage::Error {
                                    message: "unrecognized message".into(),
                                });
                                continue;
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) => break,
                    Some(Ok(_)) => continue,
                };
                handle_message(message, out_tx, shared, client_id, client_name).await;
            },
        }
    }
}

async fn handle_message(
    message: ClientMessage,
    out_tx: &mpsc::UnboundedSender<ServerMessage>,
    shared: &Arc<Shared>,
    client_id: &str,
    client_name: Option<&str>,
) {
    match message {
        ClientMessage::Authenticate { .. } => {
            let _ = out_tx.send(ServerMessage::Error {
                message: "already authenticated".into(),
            });
        }
        ClientMessage::Prompt { text } => {
            shared.bus.emit(EventPayload::UserPrompt {
                client_id: Some(client_id.to_string()),
                client_name: client_name.map(str::to_string),
                text: text.clone(),
            });
            let _ = shared.prompt_tx.send(text);
        }
        ClientMessage::AnswerQuestion {
            question_id,
            answer,
        } => {
            let sender = shared
                .questions
                .lock()
                .expect("questions poisoned")
                .remove(&question_id);
            // The first answer takes the sender; later answers find the map
            // empty and are ignored.
            if let Some(sender) = sender {
                if sender
                    .send(QuestionOutcome::Answered(answer.clone()))
                    .is_ok()
                {
                    shared.bus.emit(EventPayload::QuestionAnswered {
                        id: question_id,
                        client_id: Some(client_id.to_string()),
                        answer,
                    });
                }
            }
        }
        ClientMessage::Interrupt => {
            let _ = shared.commands.send(FrontendCommand::Interrupt).await;
        }
        ClientMessage::UploadFile { name, content_b64 } => {
            shared.bus.emit(EventPayload::FileShared {
                name,
                content_b64,
                origin: FileOrigin::Client {
                    client_id: client_id.to_string(),
                },
            });
        }
        ClientMessage::RequestEvents { from_seq } => {
            let _ = out_tx.send(ServerMessage::Events {
                events: shared.bus.since(from_seq),
            });
        }
        ClientMessage::RequestAccessReport => {
            let _ = out_tx.send(ServerMessage::AccessReport {
                report: shared.access.clone(),
            });
        }
        ClientMessage::RequestCost => {
            let entries = shared
                .cost
                .lock()
                .expect("cost cache poisoned")
                .values()
                .cloned()
                .collect();
            let _ = out_tx.send(ServerMessage::Cost { entries });
        }
        ClientMessage::RequestPairingCode => {
            let code = shared
                .pairing
                .lock()
                .expect("pairing codes poisoned")
                .mint();
            let _ = out_tx.send(ServerMessage::PairingCode {
                code,
                expires_in_secs: auth::PAIRING_CODE_TTL.as_secs(),
            });
        }
        ClientMessage::Shutdown => {
            let _ = shared
                .commands
                .send(FrontendCommand::Shutdown { message: None })
                .await;
        }
        ClientMessage::Ping { nonce } => {
            let _ = out_tx.send(ServerMessage::Pong { nonce });
        }
    }
}

async fn send_message(ws: &mut WsStream, message: &ServerMessage) -> Result<(), ()> {
    let text = serde_json::to_string(message).map_err(|_| ())?;
    ws.send(Message::text(text)).await.map_err(|_| ())
}

async fn send_to_sink(
    sink: &mut SplitSink<WsStream, Message>,
    message: &ServerMessage,
) -> Result<(), ()> {
    let text = serde_json::to_string(message).map_err(|_| ())?;
    sink.send(Message::text(text)).await.map_err(|_| ())
}
