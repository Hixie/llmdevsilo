//! Client-side support for the interactive frontend protocol: certificate
//! pinning, connection setup, authentication helpers, Ed25519 key storage,
//! and discovery of local harnesses. Used by the TUI client and by tests.

use std::path::Path;
use std::sync::Arc;

use ed25519_dalek::pkcs8::{DecodePrivateKey, EncodePrivateKey};
use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use futures::{SinkExt, StreamExt};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::WebPkiSupportedAlgorithms;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{
    connect_async_tls_with_config, Connector, MaybeTlsStream, WebSocketStream,
};

use silo_core::error::FrontendError;
use silo_core::helper::{b64, unb64};
use silo_core::protocol::{AuthRequest, ClientMessage, RunInfo, ServerMessage};

use crate::util::write_private_file;

/// A connected client-side WebSocket stream.
pub type ClientStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Successful authentication details.
#[derive(Clone, Debug, PartialEq)]
pub struct AuthOk {
    pub client_id: String,
    /// Key id assigned at pairing time, used for later
    /// challenge-signature logins.
    pub key_id: Option<String>,
    /// Highest event sequence number at authentication time.
    pub next_seq: u64,
}

/// Accepts exactly one server certificate, identified by its SHA-256
/// fingerprint; every other certificate is rejected.
#[derive(Debug)]
pub struct PinnedServerCertVerifier {
    fingerprint: [u8; 32],
    algorithms: WebPkiSupportedAlgorithms,
}

impl PinnedServerCertVerifier {
    pub fn new(fingerprint_hex: &str) -> Result<Self, FrontendError> {
        let bytes = hex::decode(fingerprint_hex.trim())
            .map_err(|e| FrontendError::Setup(format!("invalid certificate fingerprint: {e}")))?;
        let fingerprint: [u8; 32] = bytes.try_into().map_err(|_| {
            FrontendError::Setup(
                "a SHA-256 certificate fingerprint must be 64 hex characters".into(),
            )
        })?;
        Ok(PinnedServerCertVerifier {
            fingerprint,
            algorithms: rustls::crypto::ring::default_provider().signature_verification_algorithms,
        })
    }
}

impl ServerCertVerifier for PinnedServerCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let digest = Sha256::digest(end_entity.as_ref());
        if digest.as_slice() == self.fingerprint {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure,
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

/// Connects to an interactive frontend over TLS with the certificate pinned
/// to `fingerprint_hex`, and consumes the server's `Hello` (returned
/// alongside the stream). `url_or_addr` is either a full `wss://` URL or a
/// bare `host:port`.
pub async fn connect(
    url_or_addr: &str,
    fingerprint_hex: &str,
) -> Result<(ClientStream, ServerMessage), FrontendError> {
    let url = if url_or_addr.contains("://") {
        url_or_addr.to_string()
    } else {
        format!("wss://{url_or_addr}")
    };
    let verifier = Arc::new(PinnedServerCertVerifier::new(fingerprint_hex)?);
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let tls_config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| FrontendError::Setup(format!("tls protocol setup failed: {e}")))?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    let connector = Connector::Rustls(Arc::new(tls_config));
    let (mut stream, _response) =
        connect_async_tls_with_config(url.as_str(), None, false, Some(connector))
            .await
            .map_err(|e| FrontendError::Closed(format!("connection failed: {e}")))?;
    let hello = recv_server(&mut stream).await?;
    if !matches!(hello, ServerMessage::Hello { .. }) {
        return Err(FrontendError::Auth(format!(
            "expected a hello from the server, got {hello:?}"
        )));
    }
    Ok((stream, hello))
}

/// Sends one client message as a JSON text frame.
pub async fn send_client(
    stream: &mut ClientStream,
    message: &ClientMessage,
) -> Result<(), FrontendError> {
    let text = serde_json::to_string(message)
        .map_err(|e| FrontendError::Setup(format!("unserializable client message: {e}")))?;
    stream
        .send(Message::text(text))
        .await
        .map_err(|e| FrontendError::Closed(format!("send failed: {e}")))
}

/// Receives the next server message, skipping non-text frames.
pub async fn recv_server(stream: &mut ClientStream) -> Result<ServerMessage, FrontendError> {
    loop {
        match stream.next().await {
            None => return Err(FrontendError::Closed("connection closed".into())),
            Some(Err(e)) => return Err(FrontendError::Closed(format!("receive failed: {e}"))),
            Some(Ok(Message::Text(text))) => {
                return serde_json::from_str(text.as_str()).map_err(|e| {
                    FrontendError::Closed(format!("unparseable server message: {e}"))
                });
            }
            Some(Ok(Message::Close(_))) => {
                return Err(FrontendError::Closed("server closed the connection".into()));
            }
            Some(Ok(_)) => continue,
        }
    }
}

async fn expect_auth_ok(stream: &mut ClientStream) -> Result<AuthOk, FrontendError> {
    match recv_server(stream).await? {
        ServerMessage::AuthOk {
            client_id,
            key_id,
            next_seq,
        } => Ok(AuthOk {
            client_id,
            key_id,
            next_seq,
        }),
        ServerMessage::AuthError { message } => Err(FrontendError::Auth(message)),
        other => Err(FrontendError::Auth(format!(
            "unexpected reply to authentication: {other:?}"
        ))),
    }
}

/// Authenticates with the filesystem-shared local token.
pub async fn authenticate_local(
    stream: &mut ClientStream,
    token: &str,
) -> Result<AuthOk, FrontendError> {
    send_client(
        stream,
        &ClientMessage::Authenticate {
            auth: AuthRequest::LocalToken {
                token: token.to_string(),
            },
        },
    )
    .await?;
    expect_auth_ok(stream).await
}

/// Redeems a one-time pairing code, registering `public_key` for later
/// challenge-signature logins. The returned `key_id` is set.
pub async fn pair(
    stream: &mut ClientStream,
    code: &str,
    client_name: &str,
    public_key: &VerifyingKey,
) -> Result<AuthOk, FrontendError> {
    send_client(
        stream,
        &ClientMessage::Authenticate {
            auth: AuthRequest::Pair {
                code: code.to_string(),
                public_key_b64: b64(public_key.as_bytes()),
                client_name: client_name.to_string(),
            },
        },
    )
    .await?;
    expect_auth_ok(stream).await
}

/// Authenticates a previously paired client: requests a challenge, signs
/// it, and submits the signature.
pub async fn login_with_key(
    stream: &mut ClientStream,
    key_id: &str,
    signing_key: &SigningKey,
) -> Result<AuthOk, FrontendError> {
    send_client(
        stream,
        &ClientMessage::Authenticate {
            auth: AuthRequest::Challenge {
                key_id: key_id.to_string(),
            },
        },
    )
    .await?;
    let challenge = match recv_server(stream).await? {
        ServerMessage::AuthChallenge { challenge_b64 } => {
            unb64(&challenge_b64).map_err(FrontendError::Auth)?
        }
        ServerMessage::AuthError { message } => return Err(FrontendError::Auth(message)),
        other => {
            return Err(FrontendError::Auth(format!(
                "unexpected reply to the challenge request: {other:?}"
            )))
        }
    };
    let signature = signing_key.sign(&challenge);
    send_client(
        stream,
        &ClientMessage::Authenticate {
            auth: AuthRequest::Signature {
                key_id: key_id.to_string(),
                signature_b64: b64(&signature.to_bytes()),
            },
        },
    )
    .await?;
    expect_auth_ok(stream).await
}

/// Generates a fresh Ed25519 signing key.
pub fn generate_signing_key() -> SigningKey {
    SigningKey::generate(&mut rand::rngs::OsRng)
}

/// Saves a signing key as PKCS#8 PEM at `path` with file mode 0600.
pub fn save_signing_key(path: &Path, key: &SigningKey) -> Result<(), FrontendError> {
    let pem = key
        .to_pkcs8_pem(ed25519_dalek::pkcs8::spki::der::pem::LineEnding::LF)
        .map_err(|e| FrontendError::Setup(format!("key serialization failed: {e}")))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    write_private_file(path, pem.as_bytes())?;
    Ok(())
}

/// Loads a signing key saved by [`save_signing_key`].
pub fn load_signing_key(path: &Path) -> Result<SigningKey, FrontendError> {
    let text = std::fs::read_to_string(path)?;
    SigningKey::from_pkcs8_pem(&text)
        .map_err(|e| FrontendError::Setup(format!("unreadable key file {}: {e}", path.display())))
}

/// Lists the live local harnesses recorded under `<state>/run/*.json`,
/// skipping files that do not parse.
pub fn list_local_harnesses(state_dir: &Path) -> Vec<RunInfo> {
    let mut harnesses = Vec::new();
    let runs_dir = silo_core::paths::runs_dir(state_dir);
    let Ok(entries) = std::fs::read_dir(runs_dir) else {
        return harnesses;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        if let Ok(info) = serde_json::from_str::<RunInfo>(&text) {
            harnesses.push(info);
        }
    }
    harnesses.sort_by(|a, b| a.harness_id.cmp(&b.harness_id));
    harnesses
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprints_must_be_sha256_hex() {
        assert!(PinnedServerCertVerifier::new("zz").is_err());
        assert!(PinnedServerCertVerifier::new("abcd").is_err());
        let valid = "a".repeat(64);
        assert!(PinnedServerCertVerifier::new(&valid).is_ok());
    }

    #[test]
    fn verifier_accepts_only_the_pinned_certificate() {
        let cert = CertificateDer::from(b"fake certificate der".to_vec());
        let fingerprint = hex::encode(Sha256::digest(cert.as_ref()));
        let verifier = PinnedServerCertVerifier::new(&fingerprint).unwrap();
        let name = ServerName::try_from("localhost").unwrap();
        let now = UnixTime::now();

        assert!(verifier
            .verify_server_cert(&cert, &[], &name, &[], now)
            .is_ok());
        let other = CertificateDer::from(b"another certificate".to_vec());
        assert!(verifier
            .verify_server_cert(&other, &[], &name, &[], now)
            .is_err());
        assert!(!verifier.supported_verify_schemes().is_empty());
    }

    #[test]
    fn signing_keys_roundtrip_through_pem_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keys").join("client.pem");
        let key = generate_signing_key();
        save_signing_key(&path, &key).unwrap();
        let loaded = load_signing_key(&path).unwrap();
        assert_eq!(loaded.to_bytes(), key.to_bytes());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn harness_listing_skips_unparseable_files() {
        let state = tempfile::tempdir().unwrap();
        let runs = silo_core::paths::runs_dir(state.path());
        std::fs::create_dir_all(&runs).unwrap();
        let info = RunInfo {
            harness_id: "abc".into(),
            addr: "127.0.0.1:1".into(),
            cert_fingerprint_sha256: "00".repeat(32),
            local_token_path: "/tmp/token".into(),
            pid: 1,
            workspace: "/tmp/ws".into(),
        };
        std::fs::write(runs.join("abc.json"), serde_json::to_string(&info).unwrap()).unwrap();
        std::fs::write(runs.join("broken.json"), "{nope").unwrap();
        std::fs::write(runs.join("ignored.txt"), "not a run file").unwrap();

        let listed = list_local_harnesses(state.path());
        assert_eq!(listed, vec![info]);
    }

    #[test]
    fn harness_listing_is_empty_without_a_runs_directory() {
        let state = tempfile::tempdir().unwrap();
        assert!(list_local_harnesses(state.path()).is_empty());
    }
}
