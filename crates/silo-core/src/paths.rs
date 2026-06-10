//! Layout of the harness state directory.
//!
//! Everything the harness persists outside workspaces lives under one state
//! directory (`~/.llmdevsilo` by default, overridable with the
//! `LLMDEVSILO_STATE_DIR` environment variable). The state directory holds
//! journals, frontend keys, and workspace metadata; it is never part of the
//! sandbox read allowlist, and [`crate::risk`] blocks attempts to add it.

use std::path::{Path, PathBuf};

pub fn state_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("LLMDEVSILO_STATE_DIR") {
        return PathBuf::from(dir);
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".llmdevsilo")
}

/// Per-harness run files (`<id>.json`): connection details for local
/// clients (address, certificate fingerprint, local token path).
pub fn runs_dir(state: &Path) -> PathBuf {
    state.join("run")
}

pub fn journals_dir(state: &Path) -> PathBuf {
    state.join("journals")
}

/// Client-side private keys for the TUI and other local clients.
pub fn client_keys_dir(state: &Path) -> PathBuf {
    state.join("client-keys")
}

/// Harness-side data per harness id: local auth token, authorized client
/// public keys, TLS certificate.
pub fn harness_dir(state: &Path, harness_id: &str) -> PathBuf {
    state.join("harness").join(harness_id)
}

/// Registry of workspaces managed by `silo workspace` (snapshot manifests,
/// lock state, image paths).
pub fn workspaces_dir(state: &Path) -> PathBuf {
    state.join("workspaces")
}
