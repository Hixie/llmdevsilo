//! Risk validation of the sandbox read allowlist.

use std::path::{Path, PathBuf};

use silo_core::error::HarnessError;
use silo_core::risk;

/// Scans `entries` against the hardcoded risk list and refuses entries that
/// would expose known-sensitive paths under `home` or the harness state
/// directory. An entry listed in `allow_risky` is accepted despite its
/// hits; entries and overrides are compared by normalized location
/// (symlinks resolved, trailing slashes ignored), not by spelling. Returns
/// one warning per `allow_risky` entry that matches no allowlist entry.
pub fn validate_read_allowlist(
    entries: &[PathBuf],
    home: &Path,
    state_dir: &Path,
    allow_risky: &[PathBuf],
) -> Result<Vec<String>, HarnessError> {
    let normalized_entries: Vec<PathBuf> = entries
        .iter()
        .map(|entry| risk::normalize_path(entry))
        .collect();
    let overrides: Vec<PathBuf> = allow_risky
        .iter()
        .map(|path| risk::normalize_path(path))
        .collect();
    let mut warnings = Vec::new();
    for (original, normalized) in allow_risky.iter().zip(&overrides) {
        if !normalized_entries.contains(normalized) {
            warnings.push(format!(
                "--allow-risky-path {} matches no read-allowlist entry; it has no effect",
                original.display()
            ));
        }
    }
    let mut lines = Vec::new();
    for (entry, hit) in risk::scan_allowlist(entries, home, state_dir) {
        if overrides.contains(&risk::normalize_path(&entry)) {
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
        Ok(warnings)
    } else {
        Err(HarnessError::Config(format!(
            "refusing risky read allowlist entries: {}",
            lines.join("; ")
        )))
    }
}

/// Refuses a journal location the sandbox could read: an allowlist entry
/// that equals, contains, or is contained in the journal's parent directory
/// would expose the session log to sandboxed code.
pub fn validate_journal_path(
    journal_path: &Path,
    read_allowlist: &[PathBuf],
) -> Result<(), HarnessError> {
    let journal_dir = match journal_path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => risk::normalize_path(parent),
        _ => risk::normalize_path(journal_path),
    };
    for entry in read_allowlist {
        let entry = risk::normalize_path(entry);
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

    #[cfg(unix)]
    #[test]
    fn symlinked_override_matches_the_entry_by_location() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        let ssh = home.join(".ssh");
        std::fs::create_dir_all(&ssh).unwrap();
        let state = home.join("state");
        std::fs::create_dir_all(&state).unwrap();
        let link = home.join("ssh-alias");
        std::os::unix::fs::symlink(&ssh, &link).unwrap();

        let warnings =
            validate_read_allowlist(std::slice::from_ref(&ssh), home, &state, &[link]).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn trailing_slash_override_matches_the_entry() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        let ssh = home.join(".ssh");
        std::fs::create_dir_all(&ssh).unwrap();
        let state = home.join("state");
        std::fs::create_dir_all(&state).unwrap();

        let with_slash = PathBuf::from(format!("{}/", ssh.display()));
        let warnings =
            validate_read_allowlist(std::slice::from_ref(&ssh), home, &state, &[with_slash])
                .unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn unmatched_override_warns_that_it_has_no_effect() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        let state = home.join("state");
        let bin = temp.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let unrelated = temp.path().join("unrelated");

        let warnings = validate_read_allowlist(
            std::slice::from_ref(&bin),
            home,
            &state,
            std::slice::from_ref(&unrelated),
        )
        .unwrap();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("no effect"), "{}", warnings[0]);
        assert!(
            warnings[0].contains(&unrelated.display().to_string()),
            "{}",
            warnings[0]
        );
    }
}
