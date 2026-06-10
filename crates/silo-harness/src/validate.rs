//! Risk validation of the sandbox read allowlist.

use std::path::{Path, PathBuf};

use silo_core::error::HarnessError;
use silo_core::risk;

/// Scans `entries` against the hardcoded risk list and refuses entries that
/// would expose known-sensitive paths under `home` or the harness state
/// directory. An entry listed in `allow_risky` is accepted despite its
/// hits.
pub fn validate_read_allowlist(
    entries: &[PathBuf],
    home: &Path,
    state_dir: &Path,
    allow_risky: &[PathBuf],
) -> Result<(), HarnessError> {
    let mut lines = Vec::new();
    for (entry, hit) in risk::scan_allowlist(entries, home, state_dir) {
        if allow_risky.iter().any(|allowed| allowed == &entry) {
            continue;
        }
        lines.push(format!(
            "{} exposes {} ({})",
            entry.display(),
            hit.path.display(),
            hit.reason
        ));
    }
    if lines.is_empty() {
        Ok(())
    } else {
        Err(HarnessError::Config(format!(
            "refusing risky read allowlist entries: {}",
            lines.join("; ")
        )))
    }
}

/// Canonicalizes the nearest existing ancestor and reattaches the
/// remainder, so nonexistent paths compare by their real location.
fn normalize(path: &Path) -> PathBuf {
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

/// Refuses a journal location the sandbox could read: an allowlist entry
/// that equals, contains, or is contained in the journal's parent directory
/// would expose the session log to sandboxed code.
pub fn validate_journal_path(
    journal_path: &Path,
    read_allowlist: &[PathBuf],
) -> Result<(), HarnessError> {
    let journal_dir = match journal_path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => normalize(parent),
        _ => normalize(journal_path),
    };
    for entry in read_allowlist {
        let entry = normalize(entry);
        if entry.starts_with(&journal_dir) || journal_dir.starts_with(&entry) {
            return Err(HarnessError::Config(format!(
                "the journal at {} would be readable from the sandbox via the \
                 allowlist entry {}; move the journal or remove the entry",
                journal_path.display(),
                entry.display()
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_inside_an_allowlist_entry_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let shared = temp.path().join("shared");
        std::fs::create_dir_all(&shared).unwrap();
        let journal = shared.join("logs/session.jsonl");

        assert!(validate_journal_path(&journal, std::slice::from_ref(&shared)).is_err());
        // The reverse containment is refused too: allowlisting the journal
        // directory itself.
        assert!(validate_journal_path(&journal, &[shared.join("logs")]).is_err());
    }

    #[test]
    fn journal_outside_the_allowlist_passes() {
        let temp = tempfile::tempdir().unwrap();
        let shared = temp.path().join("shared");
        let private = temp.path().join("private");
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::create_dir_all(&private).unwrap();

        assert!(validate_journal_path(&private.join("session.jsonl"), &[shared]).is_ok());
        assert!(validate_journal_path(&private.join("session.jsonl"), &[]).is_ok());
    }

    #[test]
    fn risky_entry_is_refused_with_the_hit_listed() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        let state = home.join("state");
        std::fs::create_dir_all(&state).unwrap();

        let error = validate_read_allowlist(&[home.join(".ssh")], home, &state, &[]).unwrap_err();
        let message = error.to_string();
        assert!(message.contains(".ssh"), "{message}");
        assert!(message.contains("SSH"), "{message}");
    }

    #[test]
    fn override_accepts_the_listed_entry_only() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::create_dir_all(home.join(".aws")).unwrap();
        let state = home.join("state");
        std::fs::create_dir_all(&state).unwrap();

        let ssh = home.join(".ssh");
        let aws = home.join(".aws");
        assert!(validate_read_allowlist(
            std::slice::from_ref(&ssh),
            home,
            &state,
            std::slice::from_ref(&ssh)
        )
        .is_ok());
        let error = validate_read_allowlist(&[ssh.clone(), aws], home, &state, &[ssh]).unwrap_err();
        assert!(error.to_string().contains("AWS"));
    }

    #[test]
    fn clean_entries_pass() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        let state = home.join("state");
        let bin = temp.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();

        assert!(validate_read_allowlist(&[bin], home, &state, &[]).is_ok());
    }
}
