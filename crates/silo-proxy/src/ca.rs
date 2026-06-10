//! Per-session certificate authority and leaf-certificate minting.
//!
//! The CA is generated when the proxy starts. Its private key lives only in
//! memory and is never written to disk. Only the CA's public certificate
//! (PEM) is exposed, via [`SessionCa::ca_cert_pem`], for the sandbox to
//! trust. Leaf certificates are minted on demand for the host the sandbox is
//! connecting to and cached, so repeated connections to the same host reuse a
//! certificate.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::sign::CertifiedKey;
use silo_core::error::ProxyError;

/// A generated leaf certificate and its key, ready to hand to rustls.
struct Leaf {
    cert_der: CertificateDer<'static>,
    key_der: PrivatePkcs8KeyDer<'static>,
}

/// Holds the session CA and a cache of minted leaf certificates.
pub struct SessionCa {
    ca_key: KeyPair,
    ca_cert: rcgen::Certificate,
    ca_cert_pem: String,
    leaves: Mutex<HashMap<String, Arc<CertifiedKey>>>,
}

impl SessionCa {
    /// Generates a fresh CA. The private key exists only in this instance.
    pub fn generate() -> Result<Self, ProxyError> {
        let ca_key =
            KeyPair::generate().map_err(|e| ProxyError::Tls(format!("ca key generation: {e}")))?;
        let mut params = CertificateParams::new(Vec::new())
            .map_err(|e| ProxyError::Tls(format!("ca params: {e}")))?;
        let mut name = DistinguishedName::new();
        name.push(DnType::CommonName, "llmdevsilo session CA");
        name.push(DnType::OrganizationName, "llmdevsilo");
        params.distinguished_name = name;
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        let ca_cert = params
            .self_signed(&ca_key)
            .map_err(|e| ProxyError::Tls(format!("ca self-sign: {e}")))?;
        let ca_cert_pem = ca_cert.pem();
        Ok(SessionCa {
            ca_key,
            ca_cert,
            ca_cert_pem,
            leaves: Mutex::new(HashMap::new()),
        })
    }

    /// PEM of the CA's public certificate. Never contains the private key.
    pub fn ca_cert_pem(&self) -> &str {
        &self.ca_cert_pem
    }

    /// Returns a rustls signing key for `host`, minting and caching a leaf
    /// certificate signed by the session CA on first use.
    pub fn leaf_for(&self, host: &str) -> Result<Arc<CertifiedKey>, ProxyError> {
        let key = host.to_ascii_lowercase();
        {
            let cache = self.leaves.lock().expect("leaf cache poisoned");
            if let Some(existing) = cache.get(&key) {
                return Ok(existing.clone());
            }
        }
        let leaf = self.mint_leaf(host)?;
        let signing_key = rustls::crypto::ring::sign::any_supported_type(&PrivateKeyDer::Pkcs8(
            leaf.key_der.clone_key(),
        ))
        .map_err(|e| ProxyError::Tls(format!("leaf signing key: {e}")))?;
        let certified = Arc::new(CertifiedKey::new(vec![leaf.cert_der.clone()], signing_key));
        let mut cache = self.leaves.lock().expect("leaf cache poisoned");
        let entry = cache.entry(key).or_insert(certified);
        Ok(entry.clone())
    }

    fn mint_leaf(&self, host: &str) -> Result<Leaf, ProxyError> {
        let leaf_key =
            KeyPair::generate().map_err(|e| ProxyError::Tls(format!("leaf key: {e}")))?;
        let mut params = CertificateParams::new(vec![host.to_string()])
            .map_err(|e| ProxyError::Tls(format!("leaf params for {host}: {e}")))?;
        let mut name = DistinguishedName::new();
        name.push(DnType::CommonName, host);
        params.distinguished_name = name;
        params.is_ca = IsCa::NoCa;
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.use_authority_key_identifier_extension = true;
        let cert = params
            .signed_by(&leaf_key, &self.ca_cert, &self.ca_key)
            .map_err(|e| ProxyError::Tls(format!("leaf sign for {host}: {e}")))?;
        let cert_der = cert.der().clone();
        let key_der = PrivatePkcs8KeyDer::from(leaf_key.serialize_der());
        Ok(Leaf { cert_der, key_der })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ca_pem_has_no_private_key() {
        let ca = SessionCa::generate().unwrap();
        let pem = ca.ca_cert_pem();
        assert!(pem.contains("BEGIN CERTIFICATE"));
        assert!(!pem.contains("PRIVATE KEY"));
    }

    #[test]
    fn two_sessions_have_distinct_cas() {
        let a = SessionCa::generate().unwrap();
        let b = SessionCa::generate().unwrap();
        assert_ne!(a.ca_cert_pem(), b.ca_cert_pem());
    }

    #[test]
    fn leaves_are_cached_per_host() {
        let ca = SessionCa::generate().unwrap();
        let first = ca.leaf_for("example.com").unwrap();
        let second = ca.leaf_for("example.com").unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        let other = ca.leaf_for("other.com").unwrap();
        assert!(!Arc::ptr_eq(&first, &other));
    }
}
