//! Mock egress proxy for tests.
//!
//! Binds a real loopback TCP listener whose accept loop closes every accepted
//! connection immediately, so the address in the returned handle is reachable
//! but unusable. The handle carries a fixed generated CA certificate PEM. The
//! mock journals nothing.

use async_trait::async_trait;
use silo_core::config::ProxySettings;
use silo_core::error::ProxyError;
use silo_core::journal::JournalHandle;
use silo_core::traits::{EgressProxy, ProxyHandle};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use crate::ca::SessionCa;

/// Mock proxy. Opens a listener that drops connections; journals nothing.
pub struct MockProxy {
    #[allow(dead_code)]
    settings: ProxySettings,
    #[allow(dead_code)]
    journal: JournalHandle,
    handle: Option<ProxyHandle>,
    task: Option<JoinHandle<()>>,
}

impl MockProxy {
    pub fn new(settings: ProxySettings, journal: JournalHandle) -> Self {
        MockProxy {
            settings,
            journal,
            handle: None,
            task: None,
        }
    }
}

#[async_trait]
impl EgressProxy for MockProxy {
    async fn start(&mut self) -> Result<ProxyHandle, ProxyError> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let http_addr = listener.local_addr()?;
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                drop(stream);
            }
        });
        self.task = Some(task);
        let ca = SessionCa::generate()?;
        let handle = ProxyHandle {
            http_addr,
            ca_cert_pem: ca.ca_cert_pem().to_string(),
            dns_addr: None,
        };
        self.handle = Some(handle.clone());
        Ok(handle)
    }

    fn handle(&self) -> Option<ProxyHandle> {
        self.handle.clone()
    }

    async fn shutdown(&mut self) -> Result<(), ProxyError> {
        if let Some(task) = self.task.take() {
            task.abort();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use silo_core::clock::{FakeClock, SharedClock};
    use silo_core::journal::JournalWriter;
    use std::sync::Arc;

    fn journal() -> (JournalHandle, Arc<std::sync::Mutex<Vec<u8>>>) {
        let clock: SharedClock = Arc::new(FakeClock::default());
        let (writer, buf) = JournalWriter::in_memory(clock);
        (JournalHandle::new(writer), buf)
    }

    #[tokio::test]
    async fn mock_binds_and_journals_nothing() {
        let (journal, buf) = journal();
        let mut proxy = MockProxy::new(ProxySettings::default(), journal);
        let handle = proxy.start().await.unwrap();
        assert!(handle.http_addr.ip().is_loopback());
        assert!(handle.ca_cert_pem.contains("BEGIN CERTIFICATE"));
        assert!(!handle.ca_cert_pem.contains("PRIVATE KEY"));
        // Accepted connections are dropped immediately.
        let stream = tokio::net::TcpStream::connect(handle.http_addr)
            .await
            .unwrap();
        drop(stream);
        proxy.shutdown().await.unwrap();
        assert!(buf.lock().unwrap().is_empty());
    }
}
