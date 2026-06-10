//! Harness configuration. Loadable from a TOML file; every field can also
//! be set from command-line flags by the `silo` binary.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::cost::{Pricing, QuotaConfig};
use crate::error::HarnessError;
use crate::secrets::CredentialInjection;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LlmBackendKind {
    Anthropic,
    OpenaiResponses,
    OpenaiWebsocket,
    Local,
    Mock,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxKind {
    Mock,
    /// macOS sandbox-exec (Seatbelt) profile around native processes.
    MacosSandboxExec,
    /// Linux guest VM on macOS via Virtualization.framework.
    MacosLinuxVm,
    /// gVisor (runsc) on Linux.
    LinuxGvisor,
    /// Firecracker-style microVM on Linux.
    LinuxMicrovm,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrontendKind {
    Interactive,
    Headless,
    Mock,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LlmConfig {
    pub backend: LlmBackendKind,
    #[serde(default = "default_model")]
    pub model: String,
    /// Environment variable holding the API key (cloud backends).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// Override the service base URL (or the local server URL for the
    /// local backend).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Command used to start a local inference server, if the local backend
    /// should manage one (e.g. a llama.cpp `llama-server` invocation).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_server_command: Option<String>,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing: Option<Pricing>,
    #[serde(default)]
    pub quota: QuotaConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
}

fn default_model() -> String {
    "claude-sonnet-4-6".to_string()
}

fn default_max_tokens() -> u32 {
    8192
}

impl Default for LlmConfig {
    fn default() -> Self {
        LlmConfig {
            backend: LlmBackendKind::Mock,
            model: default_model(),
            api_key_env: None,
            base_url: None,
            local_server_command: None,
            max_tokens: default_max_tokens(),
            pricing: None,
            quota: QuotaConfig::default(),
            system_prompt: None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ProxySettings {
    /// Domains the sandbox may reach. Exact names; a leading "*." allows
    /// the domain and all subdomains.
    #[serde(default)]
    pub allowed_domains: Vec<String>,
    #[serde(default)]
    pub credentials: Vec<CredentialInjection>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SandboxConfig {
    pub kind: SandboxKind,
    /// Host paths the sandbox may read and execute but not write.
    #[serde(default)]
    pub read_allowlist: Vec<PathBuf>,
    #[serde(default)]
    pub proxy: ProxySettings,
    /// Filled in by the harness at startup from the attached workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_mount: Option<PathBuf>,
    /// Parent directory for the per-sandbox scratch space. Defaults to a
    /// platform temporary directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scratch_root: Option<PathBuf>,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        SandboxConfig {
            kind: SandboxKind::Mock,
            read_allowlist: Vec::new(),
            proxy: ProxySettings::default(),
            workspace_mount: None,
            scratch_root: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FrontendConfig {
    pub kind: FrontendKind,
    /// Listen address for the interactive WebSocket server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listen_addr: Option<SocketAddr>,
    /// Initial prompt for the headless frontend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headless_prompt: Option<String>,
    /// Print a pairing code on startup (interactive frontend).
    #[serde(default)]
    pub issue_pairing_code: bool,
    /// PEM certificate chain for the interactive server. Set together with
    /// `tls_key_path`; used instead of the generated self-signed
    /// certificate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_cert_path: Option<PathBuf>,
    /// PEM private key matching `tls_cert_path`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_key_path: Option<PathBuf>,
}

impl Default for FrontendConfig {
    fn default() -> Self {
        FrontendConfig {
            kind: FrontendKind::Interactive,
            listen_addr: None,
            headless_prompt: None,
            issue_pairing_code: false,
            tls_cert_path: None,
            tls_key_path: None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LoggingConfig {
    /// Journal file path. Defaults to a file under the state directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub journal_path: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HarnessConfig {
    #[serde(default = "crate::short_id")]
    pub harness_id: String,
    /// Path to the locked workspace (directory registered with
    /// `silo workspace`).
    pub workspace: PathBuf,
    #[serde(default)]
    pub llm: LlmConfig,
    #[serde(default)]
    pub sandbox: SandboxConfig,
    #[serde(default)]
    pub frontend: FrontendConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
}

impl HarnessConfig {
    pub fn load(path: &Path) -> Result<Self, HarnessError> {
        let text = std::fs::read_to_string(path)?;
        toml::from_str(&text).map_err(|e| HarnessError::Config(e.to_string()))
    }

    pub fn save(&self, path: &Path) -> Result<(), HarnessError> {
        let text = toml::to_string_pretty(self).map_err(|e| HarnessError::Config(e.to_string()))?;
        std::fs::write(path, text)?;
        Ok(())
    }

    /// One-line description safe for journaling (no secrets are stored in
    /// the config in the first place; keys are env-var names).
    pub fn summary(&self) -> String {
        format!(
            "workspace={} llm={:?}/{} sandbox={:?} frontend={:?}",
            self.workspace.display(),
            self.llm.backend,
            self.llm.model,
            self.sandbox.kind,
            self.frontend.kind
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_toml_roundtrip() {
        let config = HarnessConfig {
            harness_id: "abc123".into(),
            workspace: PathBuf::from("/tmp/ws"),
            llm: LlmConfig::default(),
            sandbox: SandboxConfig::default(),
            frontend: FrontendConfig::default(),
            logging: LoggingConfig::default(),
        };
        let text = toml::to_string_pretty(&config).unwrap();
        let parsed: HarnessConfig = toml::from_str(&text).unwrap();
        assert_eq!(parsed, config);

        let with_tls = HarnessConfig {
            frontend: FrontendConfig {
                tls_cert_path: Some(PathBuf::from("/tmp/cert.pem")),
                tls_key_path: Some(PathBuf::from("/tmp/key.pem")),
                ..FrontendConfig::default()
            },
            ..config
        };
        let text = toml::to_string_pretty(&with_tls).unwrap();
        let parsed: HarnessConfig = toml::from_str(&text).unwrap();
        assert_eq!(parsed, with_tls);
    }
}
