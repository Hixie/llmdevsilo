//! Integration tests for the intercepting proxy using real loopback sockets
//! and reqwest as the sandbox-side client.

mod common;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use silo_core::clock::{FakeClock, SharedClock};
use silo_core::config::ProxySettings;
use silo_core::journal::{parse_journal, JournalEntry, JournalHandle, JournalWriter};
use silo_core::secrets::CredentialInjection;
use silo_core::traits::ProxyHandle;
use silo_proxy::ProxyBuilder;

fn journal() -> (JournalHandle, Arc<Mutex<Vec<u8>>>) {
    let clock: SharedClock = Arc::new(FakeClock::default());
    let (writer, buf) = JournalWriter::in_memory(clock);
    (JournalHandle::new(writer), buf)
}

fn network_records(bytes: &[u8]) -> Vec<silo_core::journal::NetworkRecord> {
    parse_journal(bytes)
        .unwrap()
        .into_iter()
        .filter_map(|r| match r.entry {
            JournalEntry::Network { record } => Some(record),
            _ => None,
        })
        .collect()
}

/// Builds a reqwest client that routes through the proxy and trusts the
/// session CA.
fn client_through(handle: &ProxyHandle) -> reqwest::Client {
    let proxy = reqwest::Proxy::all(format!("http://{}", handle.http_addr)).unwrap();
    let ca = reqwest::Certificate::from_pem(handle.ca_cert_pem.as_bytes()).unwrap();
    reqwest::Client::builder()
        .proxy(proxy)
        .add_root_certificate(ca)
        .build()
        .unwrap()
}

#[tokio::test]
async fn https_request_reaches_origin_and_is_journaled() {
    let origin = common::start_tls_origin(&["test.example"]).await;
    let origin_addr: SocketAddr = format!("127.0.0.1:{}", origin.addr.port()).parse().unwrap();
    let (journal, buf) = journal();

    let settings = ProxySettings {
        allowed_domains: vec!["test.example".into()],
        credentials: vec![],
    };
    let mut proxy = ProxyBuilder::new(settings, journal)
        .enable_dns(false)
        .allow_loopback_upstream_for_tests(true)
        .with_resolver_override("test.example", origin_addr)
        .with_extra_upstream_root_ca(origin.cert_pem.clone())
        .build();
    let handle = proxy.start().await.unwrap();

    let client = client_through(&handle);
    let url = format!("https://test.example:{}/hello", origin.addr.port());
    let response = client.get(&url).send().await.unwrap();
    assert_eq!(response.status(), 200);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["method"], "GET");
    assert_eq!(body["path"], "/hello");

    proxy.shutdown().await.unwrap();

    let records = network_records(&buf.lock().unwrap());
    let allowed = records
        .iter()
        .find(|r| r.allowed && r.host == "test.example" && r.path.as_deref() == Some("/hello"));
    let allowed = allowed.expect("expected an allowed network record for the request");
    assert_eq!(allowed.method.as_deref(), Some("GET"));
    assert_eq!(allowed.status, Some(200));
    assert!(!allowed.credential_injected);
}

#[tokio::test]
async fn credential_is_injected_and_never_journaled() {
    let origin = common::start_tls_origin(&["test.example"]).await;
    let origin_addr: SocketAddr = format!("127.0.0.1:{}", origin.addr.port()).parse().unwrap();
    let (journal, buf) = journal();

    let var = "SILO_TEST_INJECTION_SECRET";
    let secret = "supersecrettoken-xyz";
    std::env::set_var(var, secret);

    let settings = ProxySettings {
        allowed_domains: vec!["test.example".into()],
        credentials: vec![CredentialInjection {
            host: "test.example".into(),
            header: "Authorization".into(),
            value_env: var.into(),
            format: "Bearer {secret}".into(),
        }],
    };
    let mut proxy = ProxyBuilder::new(settings, journal)
        .enable_dns(false)
        .allow_loopback_upstream_for_tests(true)
        .with_resolver_override("test.example", origin_addr)
        .with_extra_upstream_root_ca(origin.cert_pem.clone())
        .build();
    let handle = proxy.start().await.unwrap();
    std::env::remove_var(var);

    let client = client_through(&handle);
    let url = format!("https://test.example:{}/secure", origin.addr.port());
    // The client supplies no Authorization header.
    let response = client.get(&url).send().await.unwrap();
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(
        body["headers"]["authorization"],
        "Bearer supersecrettoken-xyz"
    );

    proxy.shutdown().await.unwrap();

    let bytes = buf.lock().unwrap().clone();
    let text = String::from_utf8_lossy(&bytes);
    assert!(!text.contains(secret), "secret leaked into journal");
    let records = network_records(&bytes);
    let injected = records
        .iter()
        .find(|r| r.host == "test.example" && r.credential_injected);
    assert!(injected.is_some(), "expected a credential_injected record");
}

#[tokio::test]
async fn client_supplied_auth_header_is_replaced() {
    let origin = common::start_tls_origin(&["test.example"]).await;
    let origin_addr: SocketAddr = format!("127.0.0.1:{}", origin.addr.port()).parse().unwrap();
    let (journal, _buf) = journal();

    let var = "SILO_TEST_INJECTION_REPLACE";
    std::env::set_var(var, "realtoken");

    let settings = ProxySettings {
        allowed_domains: vec!["test.example".into()],
        credentials: vec![CredentialInjection {
            host: "test.example".into(),
            header: "Authorization".into(),
            value_env: var.into(),
            format: "Bearer {secret}".into(),
        }],
    };
    let mut proxy = ProxyBuilder::new(settings, journal)
        .enable_dns(false)
        .allow_loopback_upstream_for_tests(true)
        .with_resolver_override("test.example", origin_addr)
        .with_extra_upstream_root_ca(origin.cert_pem.clone())
        .build();
    let handle = proxy.start().await.unwrap();
    std::env::remove_var(var);

    let client = client_through(&handle);
    let url = format!("https://test.example:{}/secure", origin.addr.port());
    let response = client
        .get(&url)
        .header("Authorization", "Bearer client-attempt")
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["headers"]["authorization"], "Bearer realtoken");

    proxy.shutdown().await.unwrap();
}

#[tokio::test]
async fn non_allowlisted_domain_gets_403() {
    let (journal, buf) = journal();
    let settings = ProxySettings {
        allowed_domains: vec!["allowed.example".into()],
        credentials: vec![],
    };
    let mut proxy = ProxyBuilder::new(settings, journal)
        .enable_dns(false)
        .build();
    let handle = proxy.start().await.unwrap();

    let client = client_through(&handle);
    let result = client.get("https://blocked.example/path").send().await;
    // A 403 to CONNECT surfaces as a connection error in reqwest.
    assert!(result.is_err());

    proxy.shutdown().await.unwrap();

    let records = network_records(&buf.lock().unwrap());
    let blocked = records
        .iter()
        .find(|r| r.host == "blocked.example" && !r.allowed);
    let blocked = blocked.expect("expected a blocked record");
    assert_eq!(blocked.note.as_deref(), Some("domain not allowlisted"));
}

#[tokio::test]
async fn ip_literal_loopback_is_blocked() {
    let (journal, buf) = journal();
    // The IP literal is not on the allowlist, so it is refused at the
    // allowlist stage with a 403.
    let settings = ProxySettings {
        allowed_domains: vec!["allowed.example".into()],
        credentials: vec![],
    };
    let mut proxy = ProxyBuilder::new(settings, journal)
        .enable_dns(false)
        .build();
    let handle = proxy.start().await.unwrap();

    let client = client_through(&handle);
    let result = client.get("https://127.0.0.1/").send().await;
    assert!(result.is_err());

    proxy.shutdown().await.unwrap();

    let records = network_records(&buf.lock().unwrap());
    assert!(records.iter().any(|r| r.host == "127.0.0.1" && !r.allowed));
}

#[tokio::test]
async fn allowlisted_ip_literal_still_blocked_by_guard() {
    let (journal, buf) = journal();
    // The IP literal is explicitly allowlisted, but it is a private address,
    // so the IP guard refuses it before any connection.
    let settings = ProxySettings {
        allowed_domains: vec!["10.1.2.3".into()],
        credentials: vec![],
    };
    let mut proxy = ProxyBuilder::new(settings, journal)
        .enable_dns(false)
        .build();
    let handle = proxy.start().await.unwrap();

    let client = client_through(&handle);
    let result = client.get("https://10.1.2.3/").send().await;
    assert!(result.is_err());

    proxy.shutdown().await.unwrap();

    let records = network_records(&buf.lock().unwrap());
    let blocked = records.iter().find(|r| r.host == "10.1.2.3" && !r.allowed);
    let blocked = blocked.expect("expected a blocked-address record");
    assert_eq!(blocked.note.as_deref(), Some("blocked address"));
}

#[tokio::test]
async fn post_dns_private_address_is_blocked() {
    let (journal, buf) = journal();
    let settings = ProxySettings {
        allowed_domains: vec!["evil.example".into()],
        credentials: vec![],
    };
    // evil.example is allowlisted, but resolves (via override) to a private
    // address, so the upstream connection must be refused after resolution.
    let private: SocketAddr = "10.0.0.1:443".parse().unwrap();
    let mut proxy = ProxyBuilder::new(settings, journal)
        .enable_dns(false)
        .allow_loopback_upstream_for_tests(true)
        .with_resolver_override("evil.example", private)
        .build();
    let handle = proxy.start().await.unwrap();

    let client = client_through(&handle);
    // CONNECT succeeds (allowlisted), TLS is set up, the inner request then
    // fails to reach the blocked upstream.
    let result = client
        .get("https://evil.example/data")
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await;
    // The inner request returns a 502 from the proxy.
    if let Ok(response) = result {
        assert_eq!(response.status(), 502);
    }

    proxy.shutdown().await.unwrap();

    let records = network_records(&buf.lock().unwrap());
    let blocked = records
        .iter()
        .find(|r| r.host == "evil.example" && !r.allowed);
    let blocked = blocked.expect("expected a blocked-address record");
    assert_eq!(blocked.note.as_deref(), Some("blocked address"));
}

#[tokio::test]
async fn plain_http_absolute_form_reaches_origin() {
    let origin = common::start_plain_origin().await;
    let origin_addr: SocketAddr = format!("127.0.0.1:{}", origin.addr.port()).parse().unwrap();
    let (journal, buf) = journal();

    let settings = ProxySettings {
        allowed_domains: vec!["plain.example".into()],
        credentials: vec![],
    };
    let mut proxy = ProxyBuilder::new(settings, journal)
        .enable_dns(false)
        .allow_loopback_upstream_for_tests(true)
        .with_resolver_override("plain.example", origin_addr)
        .build();
    let handle = proxy.start().await.unwrap();

    let client = client_through(&handle);
    let url = format!("http://plain.example:{}/plain-path", origin.addr.port());
    let response = client.get(&url).send().await.unwrap();
    assert_eq!(response.status(), 200);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["path"], "/plain-path");

    proxy.shutdown().await.unwrap();

    let records = network_records(&buf.lock().unwrap());
    let allowed = records
        .iter()
        .find(|r| r.allowed && r.host == "plain.example");
    let allowed = allowed.expect("expected an allowed plain-http record");
    assert_eq!(allowed.path.as_deref(), Some("/plain-path"));
}

#[tokio::test]
async fn ca_pem_carries_no_private_key_and_differs_per_proxy() {
    let (journal_a, _) = journal();
    let (journal_b, _) = journal();
    let mut a = ProxyBuilder::new(ProxySettings::default(), journal_a)
        .enable_dns(false)
        .build();
    let mut b = ProxyBuilder::new(ProxySettings::default(), journal_b)
        .enable_dns(false)
        .build();
    let ha = a.start().await.unwrap();
    let hb = b.start().await.unwrap();
    assert!(!ha.ca_cert_pem.contains("PRIVATE KEY"));
    assert!(!hb.ca_cert_pem.contains("PRIVATE KEY"));
    assert_ne!(ha.ca_cert_pem, hb.ca_cert_pem);
    a.shutdown().await.unwrap();
    b.shutdown().await.unwrap();
}
