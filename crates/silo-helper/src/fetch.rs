//! HTTP fetching from inside the sandbox.
//!
//! All requests go through the egress proxy: the client is built with an
//! explicit proxy address and the session CA certificate, both read once
//! from the environment (`HTTPS_PROXY`/`HTTP_PROXY` and `SILO_PROXY_CA`).
//! Certificate validation is never disabled.

use std::path::PathBuf;

use silo_core::helper::{b64, unb64, HelperPayload};
use tokio::sync::OnceCell;

/// Connection parameters for the `Fetch` operation. Built from the
/// environment in production; tests construct it directly.
#[derive(Clone, Debug, Default)]
pub struct FetchConfig {
    /// Proxy URL for all requests. `None` disables proxying entirely
    /// (including any proxy configuration in the process environment).
    pub proxy_url: Option<String>,
    /// Path to a PEM file with an additional trusted root certificate (the
    /// per-session proxy CA).
    pub ca_cert_path: Option<PathBuf>,
}

impl FetchConfig {
    /// Reads `HTTPS_PROXY` (falling back to `HTTP_PROXY`) and
    /// `SILO_PROXY_CA` from the process environment.
    pub fn from_env() -> Self {
        fn non_empty(name: &str) -> Option<String> {
            std::env::var(name)
                .ok()
                .filter(|value| !value.trim().is_empty())
        }
        FetchConfig {
            proxy_url: non_empty("HTTPS_PROXY").or_else(|| non_empty("HTTP_PROXY")),
            ca_cert_path: non_empty("SILO_PROXY_CA").map(PathBuf::from),
        }
    }
}

/// Holds the lazily built HTTP client; the client is constructed at most
/// once per serve loop.
pub(crate) struct FetchState {
    config: FetchConfig,
    client: OnceCell<reqwest::Client>,
}

impl FetchState {
    pub(crate) fn new(config: FetchConfig) -> Self {
        FetchState {
            config,
            client: OnceCell::new(),
        }
    }

    async fn client(&self) -> Result<&reqwest::Client, String> {
        self.client
            .get_or_try_init(|| async { build_client(&self.config) })
            .await
    }

    pub(crate) async fn fetch(
        &self,
        url: String,
        method: String,
        headers: Vec<(String, String)>,
        body_b64: Option<String>,
        max_bytes: u64,
    ) -> Result<HelperPayload, String> {
        let client = self.client().await?;
        let method = reqwest::Method::from_bytes(method.as_bytes())
            .map_err(|_| format!("invalid HTTP method {method:?}"))?;
        let mut request = client.request(method, &url);
        for (name, value) in &headers {
            let header_name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| format!("invalid header name {name:?}"))?;
            let header_value = reqwest::header::HeaderValue::from_str(value)
                .map_err(|_| format!("invalid value for header {name:?}"))?;
            request = request.header(header_name, header_value);
        }
        if let Some(body_b64) = body_b64 {
            request = request.body(unb64(&body_b64)?);
        }
        let mut response = request
            .send()
            .await
            .map_err(|e| format!("fetch failed: {e}"))?;
        let status = response.status().as_u16();
        let response_headers: Vec<(String, String)> = response
            .headers()
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_string(),
                    String::from_utf8_lossy(value.as_bytes()).into_owned(),
                )
            })
            .collect();
        let cap = usize::try_from(max_bytes).unwrap_or(usize::MAX);
        let mut body = Vec::new();
        let mut truncated = false;
        loop {
            let chunk = response
                .chunk()
                .await
                .map_err(|e| format!("fetch body failed: {e}"))?;
            let Some(chunk) = chunk else { break };
            let room = cap.saturating_sub(body.len());
            if chunk.len() > room {
                body.extend_from_slice(&chunk[..room]);
                truncated = true;
                break;
            }
            body.extend_from_slice(&chunk);
        }
        Ok(HelperPayload::Fetched {
            status,
            headers: response_headers,
            body_b64: b64(&body),
            truncated,
        })
    }
}

fn build_client(config: &FetchConfig) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder();
    builder = match &config.proxy_url {
        Some(url) => builder.proxy(
            reqwest::Proxy::all(url).map_err(|e| format!("invalid proxy URL {url:?}: {e}"))?,
        ),
        None => builder.no_proxy(),
    };
    if let Some(path) = &config.ca_cert_path {
        let pem = std::fs::read(path)
            .map_err(|e| format!("cannot read CA certificate {}: {e}", path.display()))?;
        let certificate = reqwest::Certificate::from_pem(&pem)
            .map_err(|e| format!("invalid CA certificate {}: {e}", path.display()))?;
        builder = builder.add_root_certificate(certificate);
    }
    builder
        .build()
        .map_err(|e| format!("cannot build HTTP client: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_client_without_proxy_or_ca() {
        assert!(build_client(&FetchConfig::default()).is_ok());
    }

    #[test]
    fn build_client_reports_missing_ca_file() {
        let config = FetchConfig {
            proxy_url: None,
            ca_cert_path: Some(PathBuf::from("/nonexistent/ca.pem")),
        };
        let err = build_client(&config).unwrap_err();
        assert!(err.contains("cannot read CA certificate"));
    }

    #[test]
    fn build_client_reports_invalid_proxy_url() {
        let config = FetchConfig {
            proxy_url: Some("::not a url::".into()),
            ca_cert_path: None,
        };
        let err = build_client(&config).unwrap_err();
        assert!(err.contains("invalid proxy URL"));
    }

    #[tokio::test]
    async fn fetch_rejects_bad_method_and_headers() {
        let state = FetchState::new(FetchConfig::default());
        let err = state
            .fetch(
                "http://127.0.0.1:1/".into(),
                "BAD METHOD".into(),
                vec![],
                None,
                100,
            )
            .await
            .unwrap_err();
        assert!(err.contains("invalid HTTP method"));

        let err = state
            .fetch(
                "http://127.0.0.1:1/".into(),
                "GET".into(),
                vec![("bad header name".into(), "v".into())],
                None,
                100,
            )
            .await
            .unwrap_err();
        assert!(err.contains("invalid header name"));
    }
}
