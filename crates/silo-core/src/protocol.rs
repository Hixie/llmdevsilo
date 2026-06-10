//! Wire protocol between the interactive frontend (WebSocket server in the
//! harness) and client applications (TUI, Flutter app, web UI).
//!
//! Messages are JSON in WebSocket text frames. The connection starts with
//! the server's `Hello`; the client must authenticate before any other
//! message is accepted.
//!
//! Authentication methods:
//! - `LocalToken`: a key shared via the local filesystem, for clients on
//!   the same machine.
//! - `Pair` + pairing code: a one-time, short-lived code issued by the
//!   harness. The client generates an Ed25519 key pair and registers the
//!   public key during pairing.
//! - `Challenge`/`Signature`: returning clients ask for a challenge and
//!   sign it with their registered key.

use serde::{Deserialize, Serialize};

use crate::cost::{QuotaConfig, UsageSnapshot};
use crate::event::Event;
use crate::sandbox::AccessReport;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum AuthRequest {
    LocalToken {
        token: String,
    },
    /// Redeem a pairing code and register a public key for future
    /// connections.
    Pair {
        code: String,
        /// Ed25519 public key, base64 (32 bytes).
        public_key_b64: String,
        client_name: String,
    },
    /// Ask the server for a challenge to sign.
    Challenge {
        key_id: String,
    },
    /// Ed25519 signature over the server-issued challenge bytes.
    Signature {
        key_id: String,
        signature_b64: String,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    Authenticate {
        #[serde(flatten)]
        auth: AuthRequest,
    },
    /// User prompt. Displayed on all clients; the first prompt received
    /// while the harness is awaiting input starts the next turn.
    Prompt {
        text: String,
    },
    /// Answer to an AskUserQuestion. First answer wins.
    AnswerQuestion {
        question_id: String,
        answer: String,
    },
    /// Upload a file; it is placed in the workspace and announced to all
    /// clients.
    UploadFile {
        name: String,
        content_b64: String,
    },
    /// Request the event backlog starting at `from_seq`.
    RequestEvents {
        from_seq: u64,
    },
    RequestAccessReport,
    RequestCost,
    /// Ask the harness to issue a one-time pairing code for another client.
    RequestPairingCode,
    /// Ask the harness to abort the in-progress turn.
    Interrupt,
    /// Ask the harness to shut down.
    Shutdown,
    Ping {
        nonce: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CostEntry {
    pub backend: String,
    pub usage: UsageSnapshot,
    pub quota: QuotaConfig,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Hello {
        harness_id: String,
        protocol_version: u32,
    },
    /// Challenge bytes (base64) for `AuthRequest::Signature`.
    AuthChallenge {
        challenge_b64: String,
    },
    AuthOk {
        client_id: String,
        /// Key id assigned during pairing, for subsequent
        /// challenge-signature logins.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key_id: Option<String>,
        /// Highest event sequence number so far, so clients know what to
        /// request.
        next_seq: u64,
    },
    AuthError {
        message: String,
    },
    Event {
        event: Event,
    },
    /// Backlog response to `RequestEvents`.
    Events {
        events: Vec<Event>,
    },
    AccessReport {
        report: AccessReport,
    },
    Cost {
        entries: Vec<CostEntry>,
    },
    PairingCode {
        code: String,
        expires_in_secs: u64,
    },
    Pong {
        nonce: u64,
    },
    Error {
        message: String,
    },
    ShuttingDown {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
}

/// Connection details written to the run file for local clients and shown
/// to the user for remote ones.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunInfo {
    pub harness_id: String,
    /// WebSocket address, e.g. "127.0.0.1:7777".
    pub addr: String,
    /// SHA-256 fingerprint of the server's TLS certificate (hex), for
    /// client-side pinning.
    pub cert_fingerprint_sha256: String,
    /// Path to the local-token file, readable only by the user.
    pub local_token_path: String,
    pub pid: u32,
    pub workspace: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_message_wire_format() {
        let message = ClientMessage::Authenticate {
            auth: AuthRequest::LocalToken { token: "t".into() },
        };
        let value = serde_json::to_value(&message).unwrap();
        assert_eq!(value["type"], "authenticate");
        assert_eq!(value["method"], "local_token");
        let parsed: ClientMessage = serde_json::from_value(value).unwrap();
        assert_eq!(parsed, message);
    }

    #[test]
    fn interrupt_wire_format_is_a_bare_type_tag() {
        let message = ClientMessage::Interrupt;
        let value = serde_json::to_value(&message).unwrap();
        assert_eq!(value, serde_json::json!({"type": "interrupt"}));
        let parsed: ClientMessage = serde_json::from_value(value).unwrap();
        assert_eq!(parsed, message);
    }
}
