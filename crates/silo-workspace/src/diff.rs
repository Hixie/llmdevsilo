//! Diffing the current workspace tree against the locked snapshot.
//!
//! Every path that differs becomes a [`FileChange`]. Text files get a
//! unified diff between the blob-store copy (the locked side) and the
//! current file; binary files (a NUL byte in the first 8 KiB, or larger
//! than 1 MiB) get no diff. Permission-mode differences are reported as
//! modifications with a "mode 0644 -> 0755"-style note.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use silo_core::error::WorkspaceError;

use crate::snapshot::TreeEntry;
use crate::{ChangeKind, FileChange};

const BINARY_SNIFF_BYTES: usize = 8192;
const MAX_DIFF_BYTES: u64 = 1024 * 1024;

/// Compares the locked manifest tree against the current tree and returns
/// one change per differing path, sorted by path.
pub(crate) fn diff_trees(
    locked: &BTreeMap<String, TreeEntry>,
    current: &BTreeMap<String, TreeEntry>,
    blobs_dir: &Path,
    current_root: &Path,
) -> Result<Vec<FileChange>, WorkspaceError> {
    let paths: BTreeSet<&String> = locked.keys().chain(current.keys()).collect();
    let mut changes = Vec::new();
    for path in paths {
        match (locked.get(path), current.get(path)) {
            (None, Some(new_entry)) => {
                let diff = if matches!(new_entry, TreeEntry::File { .. }) {
                    read_current_text(current_root, path)?.map(|new| unified("", &new))
                } else {
                    None
                };
                changes.push(FileChange {
                    path: path.clone(),
                    kind: ChangeKind::Added,
                    diff,
                    note: None,
                });
            }
            (Some(old_entry), None) => {
                let diff = if let TreeEntry::File { sha256, size, .. } = old_entry {
                    read_blob_text(blobs_dir, sha256, *size)?.map(|old| unified(&old, ""))
                } else {
                    None
                };
                changes.push(FileChange {
                    path: path.clone(),
                    kind: ChangeKind::Deleted,
                    diff,
                    note: None,
                });
            }
            (Some(old_entry), Some(new_entry)) => {
                let content_changed = !content_equal(old_entry, new_entry);
                let note = mode_note(old_entry, new_entry);
                if !content_changed && note.is_none() {
                    continue;
                }
                let diff = if !content_changed {
                    None
                } else if let (TreeEntry::File { sha256, size, .. }, TreeEntry::File { .. }) =
                    (old_entry, new_entry)
                {
                    match (
                        read_blob_text(blobs_dir, sha256, *size)?,
                        read_current_text(current_root, path)?,
                    ) {
                        (Some(old), Some(new)) => Some(unified(&old, &new)),
                        _ => None,
                    }
                } else {
                    None
                };
                changes.push(FileChange {
                    path: path.clone(),
                    kind: ChangeKind::Modified,
                    diff,
                    note,
                });
            }
            (None, None) => {}
        }
    }
    Ok(changes)
}

/// Compares two entries ignoring permission modes.
fn content_equal(old: &TreeEntry, new: &TreeEntry) -> bool {
    match (old, new) {
        (
            TreeEntry::File {
                sha256: old_sha256,
                size: old_size,
                ..
            },
            TreeEntry::File {
                sha256: new_sha256,
                size: new_size,
                ..
            },
        ) => old_sha256 == new_sha256 && old_size == new_size,
        (TreeEntry::Symlink { target: old_target }, TreeEntry::Symlink { target: new_target }) => {
            old_target == new_target
        }
        (TreeEntry::Dir { .. }, TreeEntry::Dir { .. }) => true,
        _ => false,
    }
}

/// Builds a "mode 0644 -> 0755"-style note when both entries record a
/// permission mode and the modes differ.
fn mode_note(old: &TreeEntry, new: &TreeEntry) -> Option<String> {
    match (entry_mode(old), entry_mode(new)) {
        (Some(old_mode), Some(new_mode)) if old_mode != new_mode => {
            Some(format!("mode {old_mode:04o} -> {new_mode:04o}"))
        }
        _ => None,
    }
}

fn entry_mode(entry: &TreeEntry) -> Option<u32> {
    match entry {
        TreeEntry::File { mode, .. } | TreeEntry::Dir { mode } => *mode,
        TreeEntry::Symlink { .. } => None,
    }
}

fn unified(old: &str, new: &str) -> String {
    let text_diff = similar::TextDiff::from_lines(old, new);
    text_diff
        .unified_diff()
        .header("locked", "current")
        .to_string()
}

/// Decodes bytes for diffing; `None` marks the content as binary.
fn bytes_to_text(bytes: Vec<u8>) -> Option<String> {
    if bytes.len() as u64 > MAX_DIFF_BYTES {
        return None;
    }
    let sniff = &bytes[..bytes.len().min(BINARY_SNIFF_BYTES)];
    if sniff.contains(&0) {
        return None;
    }
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

fn read_current_text(root: &Path, rel: &str) -> Result<Option<String>, WorkspaceError> {
    let path = root.join(rel);
    let metadata = fs::metadata(&path)?;
    if metadata.len() > MAX_DIFF_BYTES {
        return Ok(None);
    }
    Ok(bytes_to_text(fs::read(&path)?))
}

fn read_blob_text(
    blobs_dir: &Path,
    sha256: &str,
    size: u64,
) -> Result<Option<String>, WorkspaceError> {
    if size > MAX_DIFF_BYTES {
        return Ok(None);
    }
    let bytes = fs::read(blobs_dir.join(sha256)).map_err(|e| {
        WorkspaceError::Damaged(format!("snapshot blob {sha256} is unreadable: {e}"))
    })?;
    Ok(bytes_to_text(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot;

    #[test]
    fn binary_detection_rules() {
        assert!(bytes_to_text(vec![b'a', 0, b'b']).is_none());
        assert!(bytes_to_text(vec![b'a'; (MAX_DIFF_BYTES + 1) as usize]).is_none());
        assert!(bytes_to_text(b"plain text\n".to_vec()).is_some());

        let mut late_nul = vec![b'a'; BINARY_SNIFF_BYTES + 16];
        late_nul[BINARY_SNIFF_BYTES + 8] = 0;
        assert!(bytes_to_text(late_nul).is_some());
    }

    #[test]
    fn unified_diff_uses_locked_current_headers() {
        let diff = unified("a\nb\nc\n", "a\nx\nc\n");
        assert!(diff.starts_with("--- locked\n+++ current\n"));
        assert!(diff.contains("-b\n"));
        assert!(diff.contains("+x\n"));
    }

    #[test]
    fn detects_added_deleted_and_modified_files() {
        let temp = tempfile::tempdir().unwrap();
        let old_root = temp.path().join("old");
        let new_root = temp.path().join("new");
        let blobs = temp.path().join("blobs");
        fs::create_dir_all(&old_root).unwrap();
        fs::create_dir_all(&new_root).unwrap();
        fs::write(old_root.join("keep.txt"), "same\n").unwrap();
        fs::write(old_root.join("gone.txt"), "bye\n").unwrap();
        fs::write(old_root.join("change.txt"), "one\n").unwrap();
        fs::write(new_root.join("keep.txt"), "same\n").unwrap();
        fs::write(new_root.join("change.txt"), "two\n").unwrap();
        fs::write(new_root.join("fresh.txt"), "hi\n").unwrap();

        let locked = snapshot::snapshot(&old_root, &blobs).unwrap();
        let current = snapshot::walk_tree(&new_root).unwrap();
        let changes = diff_trees(&locked, &current, &blobs, &new_root).unwrap();

        let summary: Vec<(&str, ChangeKind)> =
            changes.iter().map(|c| (c.path.as_str(), c.kind)).collect();
        assert_eq!(
            summary,
            vec![
                ("change.txt", ChangeKind::Modified),
                ("fresh.txt", ChangeKind::Added),
                ("gone.txt", ChangeKind::Deleted),
            ]
        );

        let modified = &changes[0];
        let diff = modified.diff.as_ref().unwrap();
        assert!(diff.contains("-one\n"));
        assert!(diff.contains("+two\n"));

        let added = &changes[1];
        assert!(added.diff.as_ref().unwrap().contains("+hi\n"));

        let deleted = &changes[2];
        assert!(deleted.diff.as_ref().unwrap().contains("-bye\n"));
    }

    #[test]
    fn binary_files_get_no_diff() {
        let temp = tempfile::tempdir().unwrap();
        let old_root = temp.path().join("old");
        let new_root = temp.path().join("new");
        let blobs = temp.path().join("blobs");
        fs::create_dir_all(&old_root).unwrap();
        fs::create_dir_all(&new_root).unwrap();
        fs::write(old_root.join("blob.bin"), [0u8, 1, 2, 3]).unwrap();
        fs::write(new_root.join("blob.bin"), [0u8, 9, 9]).unwrap();

        let locked = snapshot::snapshot(&old_root, &blobs).unwrap();
        let current = snapshot::walk_tree(&new_root).unwrap();
        let changes = diff_trees(&locked, &current, &blobs, &new_root).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, ChangeKind::Modified);
        assert!(changes[0].diff.is_none());
    }

    #[test]
    fn symlink_and_kind_changes_are_modified_without_diff() {
        let mut locked = BTreeMap::new();
        locked.insert(
            "link".to_string(),
            TreeEntry::Symlink { target: "a".into() },
        );
        locked.insert("thing".to_string(), TreeEntry::Dir { mode: None });
        let mut current = BTreeMap::new();
        current.insert(
            "link".to_string(),
            TreeEntry::Symlink { target: "b".into() },
        );
        current.insert(
            "thing".to_string(),
            TreeEntry::Symlink { target: "x".into() },
        );

        let nowhere = Path::new("/nonexistent");
        let changes = diff_trees(&locked, &current, nowhere, nowhere).unwrap();
        let summary: Vec<(&str, ChangeKind, bool)> = changes
            .iter()
            .map(|c| (c.path.as_str(), c.kind, c.diff.is_some()))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("link", ChangeKind::Modified, false),
                ("thing", ChangeKind::Modified, false),
            ]
        );
    }

    fn file_entry(sha256: &str, size: u64, mode: Option<u32>) -> TreeEntry {
        TreeEntry::File {
            sha256: sha256.into(),
            size,
            mode,
        }
    }

    #[test]
    fn mode_only_change_is_modified_with_a_note_and_no_diff() {
        let mut locked = BTreeMap::new();
        locked.insert("run.sh".to_string(), file_entry("abc", 4, Some(0o644)));
        locked.insert("dir".to_string(), TreeEntry::Dir { mode: Some(0o755) });
        let mut current = BTreeMap::new();
        current.insert("run.sh".to_string(), file_entry("abc", 4, Some(0o755)));
        current.insert("dir".to_string(), TreeEntry::Dir { mode: Some(0o700) });

        let nowhere = Path::new("/nonexistent");
        let changes = diff_trees(&locked, &current, nowhere, nowhere).unwrap();
        let summary: Vec<(&str, ChangeKind, Option<&str>, bool)> = changes
            .iter()
            .map(|c| (c.path.as_str(), c.kind, c.note.as_deref(), c.diff.is_some()))
            .collect();
        assert_eq!(
            summary,
            vec![
                (
                    "dir",
                    ChangeKind::Modified,
                    Some("mode 0755 -> 0700"),
                    false
                ),
                (
                    "run.sh",
                    ChangeKind::Modified,
                    Some("mode 0644 -> 0755"),
                    false
                ),
            ]
        );
    }

    #[test]
    fn unknown_modes_do_not_report_a_change() {
        // A manifest written before modes were recorded has no modes; a
        // fresh walk has them. Identical content stays unreported.
        let mut locked = BTreeMap::new();
        locked.insert("f.txt".to_string(), file_entry("abc", 4, None));
        locked.insert("dir".to_string(), TreeEntry::Dir { mode: None });
        let mut current = BTreeMap::new();
        current.insert("f.txt".to_string(), file_entry("abc", 4, Some(0o644)));
        current.insert("dir".to_string(), TreeEntry::Dir { mode: Some(0o755) });

        let nowhere = Path::new("/nonexistent");
        let changes = diff_trees(&locked, &current, nowhere, nowhere).unwrap();
        assert!(changes.is_empty(), "changes were: {changes:?}");
    }

    #[test]
    fn content_and_mode_change_gets_a_diff_and_a_note() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let old_root = temp.path().join("old");
        let new_root = temp.path().join("new");
        let blobs = temp.path().join("blobs");
        fs::create_dir_all(&old_root).unwrap();
        fs::create_dir_all(&new_root).unwrap();
        fs::write(old_root.join("run.sh"), "old\n").unwrap();
        fs::set_permissions(
            old_root.join("run.sh"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        fs::write(new_root.join("run.sh"), "new\n").unwrap();
        fs::set_permissions(
            new_root.join("run.sh"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();

        let locked = snapshot::snapshot(&old_root, &blobs).unwrap();
        let current = snapshot::walk_tree(&new_root).unwrap();
        let changes = diff_trees(&locked, &current, &blobs, &new_root).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, ChangeKind::Modified);
        assert_eq!(changes[0].note.as_deref(), Some("mode 0644 -> 0755"));
        let diff = changes[0].diff.as_ref().unwrap();
        assert!(diff.contains("-old\n"));
        assert!(diff.contains("+new\n"));
    }
}
