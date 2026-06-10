//! Credential injection.
//!
//! For requests whose host exactly matches a configured entry, the proxy
//! strips any client-supplied header of the configured name and sets it to
//! the configured template with `{secret}` replaced by the secret value. The
//! secret is read once from its environment variable when the proxy starts
//! and held as a [`SecretString`], so it never reaches journals, events, or
//! the sandbox.

use std::collections::HashMap;

use silo_core::config::ProxySettings;
use silo_core::error::ProxyError;
use silo_core::secrets::{CredentialInjection, SecretString};

use crate::allowlist::normalize_host;

/// One resolved credential: which header to set and the value to set it to.
struct Resolved {
    header: String,
    format: String,
    secret: SecretString,
}

/// All configured credential injections, keyed by exact host name.
#[derive(Default)]
pub struct CredentialStore {
    by_host: HashMap<String, Resolved>,
}

impl CredentialStore {
    /// Reads every credential's secret from its environment variable. A
    /// missing variable is a setup error.
    pub fn from_settings(settings: &ProxySettings) -> Result<Self, ProxyError> {
        let mut by_host = HashMap::new();
        for injection in &settings.credentials {
            let value = read_secret(injection)?;
            by_host.insert(
                normalize_host(&injection.host),
                Resolved {
                    header: injection.header.clone(),
                    format: injection.format.clone(),
                    secret: value,
                },
            );
        }
        Ok(CredentialStore { by_host })
    }

    /// Whether a credential is configured for `host` (exact match).
    pub fn has_host(&self, host: &str) -> bool {
        self.by_host.contains_key(&normalize_host(host))
    }

    /// Host names with credentials configured, for the access report.
    pub fn hosts(&self) -> Vec<String> {
        let mut hosts: Vec<String> = self.by_host.keys().cloned().collect();
        hosts.sort();
        hosts
    }

    /// Rewrites the request headers for `host`: removes any existing header
    /// of the configured name, then inserts the injected value. Returns true
    /// when a credential was injected.
    pub fn apply(&self, host: &str, headers: &mut http::HeaderMap) -> bool {
        let Some(resolved) = self.by_host.get(&normalize_host(host)) else {
            return false;
        };
        let Ok(name) = http::header::HeaderName::from_bytes(resolved.header.as_bytes()) else {
            return false;
        };
        headers.remove(&name);
        let value = resolved
            .format
            .replace("{secret}", resolved.secret.expose());
        match http::header::HeaderValue::from_str(&value) {
            Ok(header_value) => {
                headers.insert(name, header_value);
                true
            }
            Err(_) => false,
        }
    }
}

fn read_secret(injection: &CredentialInjection) -> Result<SecretString, ProxyError> {
    match std::env::var(&injection.value_env) {
        Ok(value) => Ok(SecretString::new(value)),
        Err(_) => Err(ProxyError::Setup(format!(
            "credential for host {} requires environment variable {} which is not set",
            injection.host, injection.value_env
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn injection(host: &str, header: &str, env: &str, format: &str) -> CredentialInjection {
        CredentialInjection {
            host: host.to_string(),
            header: header.to_string(),
            value_env: env.to_string(),
            format: format.to_string(),
        }
    }

    #[test]
    fn missing_env_var_is_a_setup_error() {
        let settings = ProxySettings {
            allowed_domains: vec![],
            credentials: vec![injection(
                "api.example.com",
                "Authorization",
                "SILO_TEST_DEFINITELY_MISSING_VAR",
                "Bearer {secret}",
            )],
        };
        let result = CredentialStore::from_settings(&settings);
        assert!(matches!(result, Err(ProxyError::Setup(_))));
    }

    #[test]
    fn formats_and_replaces_existing_header() {
        let var = "SILO_TEST_CRED_FORMAT";
        std::env::set_var(var, "tok123");
        let settings = ProxySettings {
            allowed_domains: vec![],
            credentials: vec![injection(
                "api.example.com",
                "Authorization",
                var,
                "Bearer {secret}",
            )],
        };
        let store = CredentialStore::from_settings(&settings).unwrap();
        std::env::remove_var(var);

        let mut headers = http::HeaderMap::new();
        headers.insert(
            http::header::AUTHORIZATION,
            http::HeaderValue::from_static("client-supplied"),
        );
        assert!(store.apply("api.example.com", &mut headers));
        let values: Vec<_> = headers
            .get_all(http::header::AUTHORIZATION)
            .iter()
            .collect();
        assert_eq!(values.len(), 1);
        assert_eq!(values[0], "Bearer tok123");
    }

    #[test]
    fn exact_host_only() {
        let var = "SILO_TEST_CRED_HOST";
        std::env::set_var(var, "tok");
        let settings = ProxySettings {
            allowed_domains: vec![],
            credentials: vec![injection(
                "api.example.com",
                "Authorization",
                var,
                "{secret}",
            )],
        };
        let store = CredentialStore::from_settings(&settings).unwrap();
        std::env::remove_var(var);

        assert!(store.has_host("api.example.com"));
        assert!(store.has_host("API.example.com"));
        assert!(!store.has_host("sub.api.example.com"));
        let mut headers = http::HeaderMap::new();
        assert!(!store.apply("other.example.com", &mut headers));
    }
}
