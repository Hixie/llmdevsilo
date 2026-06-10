//! TLS material for the interactive WebSocket server.
//!
//! Each harness gets a self-signed certificate persisted under its state
//! directory, so the SHA-256 fingerprint clients pin stays stable across
//! reconnects within a session.

use std::path::Path;
use std::sync::Arc;

use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use sha2::{Digest, Sha256};

use silo_core::error::FrontendError;

use crate::util::write_private_file;

const CERT_FILE: &str = "tls-cert.pem";
const KEY_FILE: &str = "tls-key.pem";

pub(crate) struct TlsMaterial {
    pub cert_der: CertificateDer<'static>,
    pub key_der: PrivateKeyDer<'static>,
    /// SHA-256 of the certificate DER, lowercase hex.
    pub fingerprint_hex: String,
}

/// Loads the persisted certificate and key from `dir`, generating and
/// persisting a fresh self-signed pair when absent. The key file is written
/// with mode 0600.
pub(crate) fn load_or_create(dir: &Path) -> Result<TlsMaterial, FrontendError> {
    let cert_path = dir.join(CERT_FILE);
    let key_path = dir.join(KEY_FILE);
    if cert_path.exists() && key_path.exists() {
        load(&cert_path, &key_path)
    } else {
        create(&cert_path, &key_path)
    }
}

fn create(cert_path: &Path, key_path: &Path) -> Result<TlsMaterial, FrontendError> {
    let certified =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string(), "llmdevsilo".to_string()])
            .map_err(|e| FrontendError::Setup(format!("certificate generation failed: {e}")))?;
    std::fs::write(cert_path, certified.cert.pem())?;
    write_private_file(key_path, certified.key_pair.serialize_pem().as_bytes())?;
    let cert_der = certified.cert.der().clone();
    let key_der =
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(certified.key_pair.serialize_der()));
    let fingerprint_hex = fingerprint(&cert_der);
    Ok(TlsMaterial {
        cert_der,
        key_der,
        fingerprint_hex,
    })
}

fn load(cert_path: &Path, key_path: &Path) -> Result<TlsMaterial, FrontendError> {
    let cert_pem = std::fs::read(cert_path)?;
    let mut reader = std::io::BufReader::new(cert_pem.as_slice());
    let cert_der = rustls_pemfile::certs(&mut reader)
        .next()
        .transpose()?
        .ok_or_else(|| {
            FrontendError::Setup(format!("no certificate found in {}", cert_path.display()))
        })?;
    let key_pem = std::fs::read(key_path)?;
    let mut reader = std::io::BufReader::new(key_pem.as_slice());
    let key_der = rustls_pemfile::private_key(&mut reader)?.ok_or_else(|| {
        FrontendError::Setup(format!("no private key found in {}", key_path.display()))
    })?;
    let fingerprint_hex = fingerprint(&cert_der);
    Ok(TlsMaterial {
        cert_der,
        key_der,
        fingerprint_hex,
    })
}

pub(crate) fn fingerprint(cert_der: &CertificateDer<'_>) -> String {
    hex::encode(Sha256::digest(cert_der.as_ref()))
}

/// Builds the rustls server configuration for the interactive listener.
pub(crate) fn server_config(material: &TlsMaterial) -> Result<rustls::ServerConfig, FrontendError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| FrontendError::Setup(format!("tls protocol setup failed: {e}")))?
        .with_no_client_auth()
        .with_single_cert(
            vec![material.cert_der.clone()],
            material.key_der.clone_key(),
        )
        .map_err(|e| FrontendError::Setup(format!("tls certificate setup failed: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn material_is_created_then_reloaded_with_a_stable_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let first = load_or_create(dir.path()).unwrap();
        assert_eq!(first.fingerprint_hex.len(), 64);
        assert!(dir.path().join(CERT_FILE).exists());
        assert!(dir.path().join(KEY_FILE).exists());

        let second = load_or_create(dir.path()).unwrap();
        assert_eq!(second.fingerprint_hex, first.fingerprint_hex);
        assert_eq!(second.cert_der, first.cert_der);
    }

    #[test]
    fn distinct_directories_get_distinct_certificates() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let material_a = load_or_create(a.path()).unwrap();
        let material_b = load_or_create(b.path()).unwrap();
        assert_ne!(material_a.fingerprint_hex, material_b.fingerprint_hex);
    }

    #[cfg(unix)]
    #[test]
    fn key_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        load_or_create(dir.path()).unwrap();
        let mode = std::fs::metadata(dir.path().join(KEY_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn server_config_accepts_the_generated_material() {
        let dir = tempfile::tempdir().unwrap();
        let material = load_or_create(dir.path()).unwrap();
        assert!(server_config(&material).is_ok());
    }
}
