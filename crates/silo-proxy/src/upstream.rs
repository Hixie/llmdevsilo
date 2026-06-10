//! Upstream connection establishment.
//!
//! Resolves a host to addresses (or uses a test resolver override), checks
//! every candidate address against the IP guard, and connects to the first
//! permitted address. For TLS connections the upstream side verifies the real
//! server certificate against the webpki trust roots (plus any test-supplied
//! extra roots) and presents the requested server name; the session CA is
//! never used on the upstream side.

use std::net::SocketAddr;
use std::sync::Arc;

use rustls::pki_types::ServerName;
use rustls::{ClientConfig, RootCertStore};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;
use tokio_rustls::TlsConnector;

use crate::ipguard::IpGuard;

/// Why an upstream connection could not be made.
#[derive(Debug)]
pub enum UpstreamError {
    /// No resolved address was permitted by the IP guard.
    Blocked(String),
    /// DNS resolution failed or returned no addresses.
    Resolve(String),
    /// The TCP connection failed.
    Connect(String),
    /// The TLS handshake failed.
    Tls(String),
}

impl std::fmt::Display for UpstreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UpstreamError::Blocked(m) => write!(f, "blocked address: {m}"),
            UpstreamError::Resolve(m) => write!(f, "resolve failed: {m}"),
            UpstreamError::Connect(m) => write!(f, "connect failed: {m}"),
            UpstreamError::Tls(m) => write!(f, "upstream tls failed: {m}"),
        }
    }
}

/// Resolves and policy-checks upstream destinations, and opens connections.
pub struct UpstreamConnector {
    guard: IpGuard,
    tls_config: Arc<ClientConfig>,
    resolver_overrides: std::collections::HashMap<String, SocketAddr>,
}

impl UpstreamConnector {
    pub fn new(
        guard: IpGuard,
        extra_root_cas: &[String],
        resolver_overrides: std::collections::HashMap<String, SocketAddr>,
    ) -> Result<Self, String> {
        let mut roots = RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        for pem in extra_root_cas {
            let mut reader = std::io::BufReader::new(pem.as_bytes());
            for cert in rustls_pemfile::certs(&mut reader) {
                let cert = cert.map_err(|e| format!("extra root parse: {e}"))?;
                roots
                    .add(cert)
                    .map_err(|e| format!("extra root add: {e}"))?;
            }
        }
        let config =
            ClientConfig::builder_with_provider(rustls::crypto::ring::default_provider().into())
                .with_safe_default_protocol_versions()
                .map_err(|e| format!("client config: {e}"))?
                .with_root_certificates(roots)
                .with_no_client_auth();
        Ok(UpstreamConnector {
            guard,
            tls_config: Arc::new(config),
            resolver_overrides,
        })
    }

    /// The address policy this connector enforces.
    pub fn guard(&self) -> &IpGuard {
        &self.guard
    }

    /// Resolves `host:port` to candidate addresses, applying the resolver
    /// override if one is configured for the host.
    pub async fn resolve(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>, UpstreamError> {
        let normalized = crate::allowlist::normalize_host(host);
        if let Some(addr) = self.resolver_overrides.get(&normalized) {
            return Ok(vec![*addr]);
        }
        let lookup = format!("{host}:{port}");
        match tokio::net::lookup_host(lookup).await {
            Ok(addrs) => {
                let collected: Vec<SocketAddr> = addrs.collect();
                if collected.is_empty() {
                    Err(UpstreamError::Resolve(format!("no addresses for {host}")))
                } else {
                    Ok(collected)
                }
            }
            Err(e) => Err(UpstreamError::Resolve(format!("{host}: {e}"))),
        }
    }

    /// Checks every resolved address; rejects the whole connection if any
    /// address is blocked, then connects to a permitted address.
    pub async fn connect_tcp(&self, host: &str, port: u16) -> Result<TcpStream, UpstreamError> {
        let addrs = self.resolve(host, port).await?;
        for addr in &addrs {
            if let Some(reason) = self.guard.check(addr.ip()) {
                return Err(UpstreamError::Blocked(format!(
                    "{} resolves to {} ({})",
                    host,
                    addr.ip(),
                    reason.as_str()
                )));
            }
        }
        let mut last_err = None;
        for addr in addrs {
            match TcpStream::connect(addr).await {
                Ok(stream) => return Ok(stream),
                Err(e) => last_err = Some(e),
            }
        }
        Err(UpstreamError::Connect(
            last_err
                .map(|e| e.to_string())
                .unwrap_or_else(|| "no addresses".into()),
        ))
    }

    /// Opens a TCP connection and wraps it in TLS, verifying the upstream
    /// certificate and presenting `host` as the server name.
    pub async fn connect_tls(
        &self,
        host: &str,
        port: u16,
    ) -> Result<TlsStream<TcpStream>, UpstreamError> {
        let tcp = self.connect_tcp(host, port).await?;
        let server_name = ServerName::try_from(host.to_string())
            .map_err(|e| UpstreamError::Tls(format!("invalid server name {host}: {e}")))?;
        let connector = TlsConnector::from(self.tls_config.clone());
        connector
            .connect(server_name, tcp)
            .await
            .map_err(|e| UpstreamError::Tls(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connector(guard: IpGuard) -> UpstreamConnector {
        UpstreamConnector::new(guard, &[], std::collections::HashMap::new()).unwrap()
    }

    #[tokio::test]
    async fn resolver_override_is_used() {
        let addr: SocketAddr = "203.0.113.7:443".parse().unwrap();
        let mut overrides = std::collections::HashMap::new();
        overrides.insert("test.example".to_string(), addr);
        let conn = UpstreamConnector::new(IpGuard::new(), &[], overrides).unwrap();
        let resolved = conn.resolve("test.example", 443).await.unwrap();
        assert_eq!(resolved, vec![addr]);
    }

    #[tokio::test]
    async fn override_to_blocked_address_is_rejected() {
        let addr: SocketAddr = "10.0.0.1:443".parse().unwrap();
        let mut overrides = std::collections::HashMap::new();
        overrides.insert("evil.example".to_string(), addr);
        let conn =
            UpstreamConnector::new(IpGuard::with_loopback_allowed(true), &[], overrides).unwrap();
        let err = conn.connect_tcp("evil.example", 443).await.unwrap_err();
        assert!(matches!(err, UpstreamError::Blocked(_)));
    }

    #[tokio::test]
    async fn loopback_blocked_by_default() {
        let addr: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let mut overrides = std::collections::HashMap::new();
        overrides.insert("local.example".to_string(), addr);
        let conn = UpstreamConnector::new(IpGuard::new(), &[], overrides).unwrap();
        let err = conn.connect_tcp("local.example", 9).await.unwrap_err();
        assert!(matches!(err, UpstreamError::Blocked(_)));
        let _ = connector(IpGuard::new());
    }
}
