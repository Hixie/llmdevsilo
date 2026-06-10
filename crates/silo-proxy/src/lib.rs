//! Harness-controlled egress proxy.
//!
//! All sandbox network traffic funnels through this proxy. It enforces a
//! domain allowlist; blocks localhost, intranet (RFC 1918 and similar),
//! and link-local destinations both by name and after DNS resolution (IPv4,
//! IPv6, and IPv4-mapped IPv6); terminates TLS from the sandbox with a
//! per-session ephemeral certificate authority (private key never persisted,
//! never readable from the sandbox); injects credentials for configured
//! hosts; and journals a summary of every exchange.

pub mod allowlist;
pub mod ca;
pub mod credentials;
pub mod dns;
pub mod ipguard;
pub mod mock;
pub mod proxy;
pub mod upstream;

use std::collections::HashMap;
use std::net::SocketAddr;

use silo_core::config::ProxySettings;
use silo_core::journal::JournalHandle;
use silo_core::traits::EgressProxy;

pub use proxy::HttpProxy;

/// Creates the real intercepting proxy with default options: an ephemeral
/// loopback TCP port for HTTP and an ephemeral loopback UDP port for the
/// DNS filter.
pub fn create_proxy(settings: ProxySettings, journal: JournalHandle) -> Box<dyn EgressProxy> {
    ProxyBuilder::new(settings, journal).build()
}

/// Creates the mock proxy for tests: a real loopback listener that closes
/// every accepted connection immediately, with a generated CA certificate
/// in its handle. It journals nothing.
pub fn create_mock_proxy(settings: ProxySettings, journal: JournalHandle) -> Box<dyn EgressProxy> {
    Box::new(mock::MockProxy::new(settings, journal))
}

/// Configures and constructs an [`HttpProxy`]. [`create_proxy`] uses the
/// defaults; tests use the builder to pin ports and install test hooks.
pub struct ProxyBuilder {
    pub(crate) settings: ProxySettings,
    pub(crate) journal: JournalHandle,
    pub(crate) port: u16,
    pub(crate) dns_enabled: bool,
    pub(crate) dns_port: u16,
    pub(crate) resolver_overrides: HashMap<String, SocketAddr>,
    pub(crate) extra_upstream_root_cas: Vec<String>,
    pub(crate) allow_loopback_upstream: bool,
}

impl ProxyBuilder {
    pub fn new(settings: ProxySettings, journal: JournalHandle) -> Self {
        ProxyBuilder {
            settings,
            journal,
            port: 0,
            dns_enabled: true,
            dns_port: 0,
            resolver_overrides: HashMap::new(),
            extra_upstream_root_cas: Vec::new(),
            allow_loopback_upstream: false,
        }
    }

    /// TCP port for the HTTP proxy listener. Zero picks an ephemeral port.
    pub fn port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    /// Whether to serve the UDP DNS filter alongside the HTTP proxy.
    pub fn enable_dns(mut self, enabled: bool) -> Self {
        self.dns_enabled = enabled;
        self
    }

    /// UDP port for the DNS filter. Zero picks an ephemeral port.
    pub fn dns_port(mut self, port: u16) -> Self {
        self.dns_port = port;
        self
    }

    /// Test hook only; not reachable from [`ProxySettings`] or configuration
    /// files. Resolves `host` to `addr` instead of using DNS. The IP guard
    /// still applies to the overridden address.
    pub fn with_resolver_override(mut self, host: impl Into<String>, addr: SocketAddr) -> Self {
        self.resolver_overrides
            .insert(allowlist::normalize_host(&host.into()), addr);
        self
    }

    /// Test hook only; not reachable from [`ProxySettings`] or configuration
    /// files. Adds a PEM certificate to the trust roots used for upstream
    /// TLS connections, so tests can run a local TLS origin.
    pub fn with_extra_upstream_root_ca(mut self, pem: impl Into<String>) -> Self {
        self.extra_upstream_root_cas.push(pem.into());
        self
    }

    /// Test hook only; not reachable from [`ProxySettings`] or configuration
    /// files. Permits loopback upstream addresses so tests can connect to
    /// local origin servers. All other blocked ranges stay blocked.
    pub fn allow_loopback_upstream_for_tests(mut self, allow: bool) -> Self {
        self.allow_loopback_upstream = allow;
        self
    }

    pub fn build(self) -> Box<dyn EgressProxy> {
        Box::new(HttpProxy::new(self))
    }
}
