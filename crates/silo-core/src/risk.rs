//! Hardcoded list of known-sensitive paths, and scanning of read-allowlist
//! entries against it.
//!
//! The user configures which host paths the sandbox can read. Adding, say,
//! the home directory would expose SSH keys; the harness refuses such
//! entries. This is best-effort defense-in-depth, not a core part of the
//! security model.

use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq)]
pub struct RiskHit {
    /// The sensitive path that would become readable.
    pub path: PathBuf,
    pub reason: &'static str,
}

/// Paths relative to the home directory that are known to hold credentials
/// or other sensitive user data.
const HOME_RELATIVE_RISKS: &[(&str, &str)] = &[
    (".ssh", "SSH private keys"),
    (".gnupg", "GPG private keys"),
    (".aws", "AWS credentials"),
    (".azure", "Azure credentials"),
    (".config/gcloud", "Google Cloud credentials"),
    (".kube", "Kubernetes credentials"),
    (".docker/config.json", "Docker registry credentials"),
    (".netrc", "netrc passwords"),
    (".npmrc", "npm tokens"),
    (".pypirc", "PyPI tokens"),
    (".cargo/credentials", "crates.io tokens"),
    (".cargo/credentials.toml", "crates.io tokens"),
    (".git-credentials", "git stored passwords"),
    (".password-store", "pass(1) password store"),
    (".gitconfig", "git config may embed credentials"),
    (".mozilla", "Firefox profile (cookies, passwords)"),
    (".thunderbird", "Thunderbird profile"),
    (
        ".config/google-chrome",
        "Chrome profile (cookies, passwords)",
    ),
    (".config/chromium", "Chromium profile (cookies, passwords)"),
    (
        ".config/BraveSoftware",
        "Brave profile (cookies, passwords)",
    ),
    ("Library/Keychains", "macOS keychains"),
    ("Library/Cookies", "macOS cookies"),
    ("Library/Safari", "Safari browsing data"),
    (
        "Library/Application Support/Google/Chrome",
        "Chrome profile (cookies, passwords)",
    ),
    (
        "Library/Application Support/Firefox",
        "Firefox profile (cookies, passwords)",
    ),
    (
        "Library/Application Support/BraveSoftware",
        "Brave profile (cookies, passwords)",
    ),
    (".claude", "Claude Code credentials and state"),
    (".openai", "OpenAI credentials"),
    (".anthropic", "Anthropic credentials"),
];

/// All known risky paths that exist on this system, as absolute paths.
pub fn known_risky_paths(home: &Path, state_dir: &Path) -> Vec<(PathBuf, &'static str)> {
    let mut risks: Vec<(PathBuf, &'static str)> = HOME_RELATIVE_RISKS
        .iter()
        .map(|(rel, reason)| (home.join(rel), *reason))
        .collect();
    risks.push((
        state_dir.to_path_buf(),
        "llmdevsilo state (journals, frontend keys, harness credentials)",
    ));
    risks
}

fn normalize(path: &Path) -> PathBuf {
    // Canonicalize so symlinked entries are compared by their real
    // location. For nonexistent paths, canonicalize the nearest existing
    // ancestor and reattach the remainder.
    if let Ok(real) = std::fs::canonicalize(path) {
        return real;
    }
    if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
        if !parent.as_os_str().is_empty() {
            return normalize(parent).join(name);
        }
    }
    path.to_path_buf()
}

/// Checks whether adding `entry` to the read allowlist would expose any
/// known-sensitive path. A hit is reported when the entry equals, contains,
/// or is contained in a sensitive path that exists.
pub fn scan_allowlist_entry(entry: &Path, home: &Path, state_dir: &Path) -> Vec<RiskHit> {
    let entry = normalize(entry);
    let mut hits = Vec::new();
    for (risky, reason) in known_risky_paths(home, state_dir) {
        if !risky.exists() {
            continue;
        }
        let risky = normalize(&risky);
        if risky.starts_with(&entry) || entry.starts_with(&risky) {
            hits.push(RiskHit {
                path: risky,
                reason,
            });
        }
    }
    hits
}

/// Scans a whole allowlist; returns all hits with the offending entry.
pub fn scan_allowlist(
    entries: &[PathBuf],
    home: &Path,
    state_dir: &Path,
) -> Vec<(PathBuf, RiskHit)> {
    let mut all = Vec::new();
    for entry in entries {
        for hit in scan_allowlist_entry(entry, home, state_dir) {
            all.push((entry.clone(), hit));
        }
    }
    all
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home_directory_is_flagged_when_it_contains_ssh_keys() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        let state = home.join(".llmdevsilo");
        std::fs::create_dir_all(&state).unwrap();

        let hits = scan_allowlist_entry(home, home, &state);
        assert!(hits.iter().any(|h| h.reason.contains("SSH")));
        assert!(hits.iter().any(|h| h.reason.contains("llmdevsilo")));
    }

    #[test]
    fn risky_subpath_is_flagged_directly() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        let ssh = home.join(".ssh");
        std::fs::create_dir_all(&ssh).unwrap();
        let state = home.join(".llmdevsilo");

        let hits = scan_allowlist_entry(&ssh.join("id_rsa"), home, &state);
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn unrelated_path_is_clean() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        let state = home.join(".llmdevsilo");
        let usr_bin = temp.path().join("usr-bin");
        std::fs::create_dir_all(&usr_bin).unwrap();

        assert!(scan_allowlist_entry(&usr_bin, home, &state).is_empty());
    }
}
