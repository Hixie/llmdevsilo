//! TLS material for the interactive WebSocket server.
//!
//! Each harness gets a self-signed certificate persisted under its state
//! directory, so the SHA-256 fingerprint clients pin stays stable across
//! reconnects within a session. A user-supplied certificate chain and key
//! can be loaded instead; the pinned fingerprint is then the SHA-256 of
//! the supplied leaf certificate.

use std::path::Path;
use std::sync::Arc;

use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use sha2::{Digest, Sha256};

use silo_core::error::FrontendError;

use crate::util::write_private_file;

const CERT_FILE: &str = "tls-cert.pem";
const KEY_FILE: &str = "tls-key.pem";

#[derive(Debug)]
pub(crate) struct TlsMaterial {
    /// Certificate chain, leaf first.
    pub cert_chain: Vec<CertificateDer<'static>>,
    pub key_der: PrivateKeyDer<'static>,
    /// SHA-256 of the leaf certificate DER, lowercase hex.
    pub fingerprint_hex: String,
}

/// Loads the persisted certificate and key from `dir`, generating and
/// persisting a fresh self-signed pair when absent. The key file is written
/// with mode 0600.
pub(crate) fn load_or_create(dir: &Path) -> Result<TlsMaterial, FrontendError> {
    let cert_path = dir.join(CERT_FILE);
    let key_path = dir.join(KEY_FILE);
    if cert_path.exists() && key_path.exists() {
        load_pair(&cert_path, &key_path)
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
        cert_chain: vec![cert_der],
        key_der,
        fingerprint_hex,
    })
}

/// Loads a PEM certificate chain (leaf first) and matching PEM private key.
/// The key must parse and its public key must match the leaf certificate.
pub(crate) fn load_pair(cert_path: &Path, key_path: &Path) -> Result<TlsMaterial, FrontendError> {
    let cert_pem = std::fs::read(cert_path).map_err(|e| {
        FrontendError::Setup(format!(
            "unreadable certificate file {}: {e}",
            cert_path.display()
        ))
    })?;
    let mut reader = std::io::BufReader::new(cert_pem.as_slice());
    let cert_chain: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| {
            FrontendError::Setup(format!(
                "unparseable certificate file {}: {e}",
                cert_path.display()
            ))
        })?;
    if cert_chain.is_empty() {
        return Err(FrontendError::Setup(format!(
            "no certificate found in {}",
            cert_path.display()
        )));
    }
    let key_pem = std::fs::read(key_path).map_err(|e| {
        FrontendError::Setup(format!("unreadable key file {}: {e}", key_path.display()))
    })?;
    let mut reader = std::io::BufReader::new(key_pem.as_slice());
    let key_der = rustls_pemfile::private_key(&mut reader)
        .map_err(|e| {
            FrontendError::Setup(format!("unparseable key file {}: {e}", key_path.display()))
        })?
        .ok_or_else(|| {
            FrontendError::Setup(format!("no private key found in {}", key_path.display()))
        })?;
    let provider = rustls::crypto::ring::default_provider();
    rustls::sign::CertifiedKey::from_der(cert_chain.clone(), key_der.clone_key(), &provider)
        .map_err(|e| {
            FrontendError::Setup(format!(
                "certificate {} and key {} do not form a usable pair: {e}",
                cert_path.display(),
                key_path.display()
            ))
        })?;
    let fingerprint_hex = fingerprint(&cert_chain[0]);
    Ok(TlsMaterial {
        cert_chain,
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
        .with_single_cert(material.cert_chain.clone(), material.key_der.clone_key())
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
        assert_eq!(second.cert_chain, first.cert_chain);
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

    #[test]
    fn supplied_material_loads_with_the_leaf_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let certified = rcgen::generate_simple_self_signed(vec!["example.test".into()]).unwrap();
        let cert_path = dir.path().join("supplied-cert.pem");
        let key_path = dir.path().join("supplied-key.pem");
        std::fs::write(&cert_path, certified.cert.pem()).unwrap();
        std::fs::write(&key_path, certified.key_pair.serialize_pem()).unwrap();

        let material = load_pair(&cert_path, &key_path).unwrap();
        assert_eq!(material.cert_chain.len(), 1);
        assert_eq!(material.fingerprint_hex, fingerprint(certified.cert.der()));
        assert!(server_config(&material).is_ok());
    }

    #[test]
    fn mismatched_certificate_and_key_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let a = rcgen::generate_simple_self_signed(vec!["a.test".into()]).unwrap();
        let b = rcgen::generate_simple_self_signed(vec!["b.test".into()]).unwrap();
        let cert_path = dir.path().join("cert.pem");
        let key_path = dir.path().join("key.pem");
        std::fs::write(&cert_path, a.cert.pem()).unwrap();
        std::fs::write(&key_path, b.key_pair.serialize_pem()).unwrap();

        let error = load_pair(&cert_path, &key_path).unwrap_err();
        assert!(matches!(error, FrontendError::Setup(_)));
        assert!(error.to_string().contains("usable pair"));
    }

    #[test]
    fn missing_or_empty_supplied_files_are_setup_errors() {
        let dir = tempfile::tempdir().unwrap();
        let certified = rcgen::generate_simple_self_signed(vec!["c.test".into()]).unwrap();
        let cert_path = dir.path().join("cert.pem");
        let key_path = dir.path().join("key.pem");

        let missing = load_pair(&cert_path, &key_path).unwrap_err();
        assert!(missing.to_string().contains("unreadable certificate file"));

        std::fs::write(&cert_path, "").unwrap();
        std::fs::write(&key_path, certified.key_pair.serialize_pem()).unwrap();
        let empty_cert = load_pair(&cert_path, &key_path).unwrap_err();
        assert!(empty_cert.to_string().contains("no certificate found"));

        std::fs::write(&cert_path, certified.cert.pem()).unwrap();
        std::fs::write(&key_path, "").unwrap();
        let empty_key = load_pair(&cert_path, &key_path).unwrap_err();
        assert!(empty_key.to_string().contains("no private key found"));
    }
}
