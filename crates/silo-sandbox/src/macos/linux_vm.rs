//! Scaffold of the macOS Linux-VM backend (Virtualization.framework).
//!
//! Design (see `docs/SANDBOX-BACKENDS.md`, section "macos-linux-vm"):
//! a minimal Linux guest is booted through Virtualization.framework; the
//! workspace and the read allowlist are attached with virtio-fs (the
//! workspace read/write, allowlist shares read-only); the scratch space is
//! a guest-local filesystem; the helper runs as the guest init's only
//! child and connects back over vsock; all guest egress is routed to a
//! host-side tap owned by the harness, which forwards port 3128 to the
//! egress proxy and drops everything else, so DNS and direct connections
//! do not leave the guest.
//!
//! The backend is not implemented. `create` returns
//! [`SandboxError::Unavailable`] so callers can fall back or report the
//! missing capability.

use silo_core::config::SandboxConfig;
use silo_core::error::SandboxError;
use silo_core::journal::JournalHandle;
use silo_core::traits::{ProxyHandle, Sandbox};

pub async fn create(
    _config: &SandboxConfig,
    _proxy: ProxyHandle,
    _journal: JournalHandle,
) -> Result<Box<dyn Sandbox>, SandboxError> {
    Err(SandboxError::Unavailable(
        "the Virtualization.framework Linux VM backend is not implemented yet; \
         see docs/SANDBOX-BACKENDS.md (macos-linux-vm) for the design, or use \
         the macos-sandbox-exec backend"
            .into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use silo_core::clock::FakeClock;
    use std::sync::Arc;

    #[tokio::test]
    async fn create_reports_unavailable_with_a_pointer_to_the_docs() {
        let proxy = ProxyHandle {
            http_addr: "127.0.0.1:3128".parse().unwrap(),
            ca_cert_pem: String::new(),
            dns_addr: None,
        };
        let journal = JournalHandle::disabled(Arc::new(FakeClock::default()));
        match create(&SandboxConfig::default(), proxy, journal).await {
            Err(SandboxError::Unavailable(message)) => {
                assert!(message.contains("docs/SANDBOX-BACKENDS.md"));
            }
            Err(other) => panic!("expected Unavailable, got {other:?}"),
            Ok(_) => panic!("expected Unavailable, got a sandbox"),
        }
    }
}
