//! Secret handling. Secrets are kept out of journals and logs by
//! construction: [`SecretString`] serializes and displays as a redaction
//! marker, and configuration refers to secrets by environment variable name
//! rather than by value.

use std::fmt;

use serde::{Deserialize, Serialize};

/// A string that never appears in `Debug`, `Display`, or serialized output.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretString(String);

impl SecretString {
    pub fn new(value: impl Into<String>) -> Self {
        SecretString(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

impl fmt::Display for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

impl Serialize for SecretString {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str("[redacted]")
    }
}

impl<'de> Deserialize<'de> for SecretString {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(SecretString(String::deserialize(deserializer)?))
    }
}

/// Configures the egress proxy to attach a credential to requests for one
/// host. The secret value is read from the named environment variable when
/// the proxy starts; it is never stored in configuration files or journals,
/// and is never readable from inside the sandbox.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CredentialInjection {
    /// Exact host name the credential applies to, e.g. "api.github.com".
    pub host: String,
    /// Header to set, e.g. "Authorization".
    pub header: String,
    /// Environment variable holding the secret value.
    pub value_env: String,
    /// Header value template; `{secret}` is replaced with the secret.
    #[serde(default = "default_format")]
    pub format: String,
}

fn default_format() -> String {
    "{secret}".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_never_leaks_via_debug_display_or_serde() {
        let secret = SecretString::new("hunter2");
        assert_eq!(format!("{secret}"), "[redacted]");
        assert_eq!(format!("{secret:?}"), "[redacted]");
        assert_eq!(serde_json::to_string(&secret).unwrap(), "\"[redacted]\"");
        assert_eq!(secret.expose(), "hunter2");
    }
}
