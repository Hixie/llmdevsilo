//! Types describing what a sandbox exposes to the LLM and to the user.

use serde::{Deserialize, Serialize};

/// Human-readable description of everything the sandboxed LLM can reach.
/// Surfaced to interactive clients so the user can audit access at a glance.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AccessReport {
    /// Sandbox backend in use, e.g. "mock" or "macos-sandbox-exec".
    pub sandbox_kind: String,
    /// Path of the read/write workspace as seen inside the sandbox.
    pub workspace_mount: String,
    /// Path of the per-sandbox writable scratch space inside the sandbox.
    pub scratch_dir: String,
    /// Host paths the sandbox can read (and execute) but not write.
    pub readable_paths: Vec<String>,
    /// Domains the egress proxy will allow.
    pub allowed_domains: Vec<String>,
    /// Domains for which the proxy injects credentials. Only domain names
    /// are listed; the credentials themselves are never exposed.
    pub credential_domains: Vec<String>,
    /// Free-form caveats, e.g. platform-specific lock limitations.
    pub notes: Vec<String>,
}
