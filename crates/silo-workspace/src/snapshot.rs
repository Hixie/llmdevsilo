//! Snapshots of a workspace tree: a manifest of every entry plus a
//! content-addressed blob store holding each regular file's bytes.
//!
//! The walk covers everything including hidden files. Symlinks are never
//! followed; they are recorded as symlinks with their target. Regular file
//! contents are stored under `blobs/<sha256>`, deduplicated by hash, so the
//! unlock diff can reconstruct the locked side of every change.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use silo_core::error::WorkspaceError;
use walkdir::WalkDir;

use crate::path_to_string;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EntryKind {
    File,
    Symlink,
    Dir,
}

/// One manifest record as serialized in `manifest.json`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct ManifestEntry {
    /// Path relative to the workspace root, with `/` separators.
    pub path: String,
    pub kind: EntryKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symlink_target: Option<String>,
    /// Permission bits (`mode & 0o7777`) for files and directories. Absent
    /// in manifests written before modes were recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<u32>,
}

/// In-memory form of one tree entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TreeEntry {
    File {
        sha256: String,
        size: u64,
        mode: Option<u32>,
    },
    Symlink {
        target: String,
    },
    Dir {
        mode: Option<u32>,
    },
}

impl ManifestEntry {
    fn from_tree(path: &str, entry: &TreeEntry) -> Self {
        match entry {
            TreeEntry::File { sha256, size, mode } => ManifestEntry {
                path: path.to_string(),
                kind: EntryKind::File,
                sha256: Some(sha256.clone()),
                size: Some(*size),
                symlink_target: None,
                mode: *mode,
            },
            TreeEntry::Symlink { target } => ManifestEntry {
                path: path.to_string(),
                kind: EntryKind::Symlink,
                sha256: None,
                size: None,
                symlink_target: Some(target.clone()),
                mode: None,
            },
            TreeEntry::Dir { mode } => ManifestEntry {
                path: path.to_string(),
                kind: EntryKind::Dir,
                sha256: None,
                size: None,
                symlink_target: None,
                mode: *mode,
            },
        }
    }

    fn into_tree(self) -> Result<(String, TreeEntry), WorkspaceError> {
        let entry = match self.kind {
            EntryKind::File => match (self.sha256, self.size) {
                (Some(sha256), Some(size)) => TreeEntry::File {
                    sha256,
                    size,
                    mode: self.mode,
                },
                _ => {
                    return Err(WorkspaceError::Damaged(format!(
                        "manifest entry {} is a file without a hash or size",
                        self.path
                    )))
                }
            },
            EntryKind::Symlink => match self.symlink_target {
                Some(target) => TreeEntry::Symlink { target },
                None => {
                    return Err(WorkspaceError::Damaged(format!(
                        "manifest entry {} is a symlink without a target",
                        self.path
                    )))
                }
            },
            EntryKind::Dir => TreeEntry::Dir { mode: self.mode },
        };
        Ok((self.path, entry))
    }
}

fn scan_tree<F>(
    root: &Path,
    mut file_entry: F,
) -> Result<BTreeMap<String, TreeEntry>, WorkspaceError>
where
    F: FnMut(&Path) -> Result<(String, u64), WorkspaceError>,
{
    let mut tree = BTreeMap::new();
    for entry in WalkDir::new(root).min_depth(1).follow_links(false) {
        let entry = entry.map_err(|e| WorkspaceError::Io(e.into()))?;
        let rel = entry
            .path()
            .strip_prefix(root)
            .map_err(|_| {
                WorkspaceError::Setup(format!(
                    "walked path {} is outside the workspace root",
                    entry.path().display()
                ))
            })
            .and_then(path_to_string)?;
        let file_type = entry.file_type();
        let node = if file_type.is_symlink() {
            let target = fs::read_link(entry.path())?;
            TreeEntry::Symlink {
                target: path_to_string(&target)?,
            }
        } else if file_type.is_dir() {
            TreeEntry::Dir {
                mode: entry_mode(entry.path())?,
            }
        } else if file_type.is_file() {
            let (sha256, size) = file_entry(entry.path())?;
            TreeEntry::File {
                sha256,
                size,
                mode: entry_mode(entry.path())?,
            }
        } else {
            return Err(WorkspaceError::Setup(format!(
                "unsupported file type at {}",
                entry.path().display()
            )));
        };
        tree.insert(rel, node);
    }
    Ok(tree)
}

/// Permission bits (`mode & 0o7777`) of the entry, without following
/// symlinks. `None` on platforms without Unix permission modes.
#[cfg(unix)]
fn entry_mode(path: &Path) -> Result<Option<u32>, WorkspaceError> {
    use std::os::unix::fs::MetadataExt;
    let metadata = fs::symlink_metadata(path)?;
    Ok(Some(metadata.mode() & 0o7777))
}

#[cfg(not(unix))]
fn entry_mode(_path: &Path) -> Result<Option<u32>, WorkspaceError> {
    Ok(None)
}

/// Walks the tree and records every entry without storing any content.
pub(crate) fn walk_tree(root: &Path) -> Result<BTreeMap<String, TreeEntry>, WorkspaceError> {
    scan_tree(root, |path| hash_file(path).map_err(WorkspaceError::Io))
}

/// Walks the tree and stores every regular file's content in the blob
/// store, keyed by SHA-256.
pub(crate) fn snapshot(
    root: &Path,
    blobs_dir: &Path,
) -> Result<BTreeMap<String, TreeEntry>, WorkspaceError> {
    fs::create_dir_all(blobs_dir)?;
    scan_tree(root, |path| store_file_blob(path, blobs_dir))
}

fn hash_file(path: &Path) -> std::io::Result<(String, u64)> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 65536];
    let mut size = 0u64;
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
        size += n as u64;
    }
    Ok((hex::encode(hasher.finalize()), size))
}

/// Hashes a file while copying it into the blob store. The copy goes to a
/// temporary name first and is renamed into place, so a blob file is always
/// complete. Existing blobs are reused.
fn store_file_blob(path: &Path, blobs_dir: &Path) -> Result<(String, u64), WorkspaceError> {
    let mut file = File::open(path)?;
    let tmp_path = blobs_dir.join(".incoming");
    let mut tmp = File::create(&tmp_path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 65536];
    let mut size = 0u64;
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
        tmp.write_all(&buffer[..n])?;
        size += n as u64;
    }
    drop(tmp);
    let sha256 = hex::encode(hasher.finalize());
    let blob_path = blobs_dir.join(&sha256);
    if blob_path.exists() {
        fs::remove_file(&tmp_path)?;
    } else {
        fs::rename(&tmp_path, &blob_path)?;
    }
    Ok((sha256, size))
}

pub(crate) fn write_manifest(
    path: &Path,
    tree: &BTreeMap<String, TreeEntry>,
) -> Result<(), WorkspaceError> {
    let entries: Vec<ManifestEntry> = tree
        .iter()
        .map(|(rel, entry)| ManifestEntry::from_tree(rel, entry))
        .collect();
    let bytes = serde_json::to_vec_pretty(&entries)
        .map_err(|e| WorkspaceError::Setup(format!("cannot serialize manifest: {e}")))?;
    fs::write(path, bytes)?;
    Ok(())
}

pub(crate) fn load_manifest(path: &Path) -> Result<BTreeMap<String, TreeEntry>, WorkspaceError> {
    let bytes = fs::read(path)?;
    let entries: Vec<ManifestEntry> = serde_json::from_slice(&bytes).map_err(|e| {
        WorkspaceError::Damaged(format!("manifest at {} is unreadable: {e}", path.display()))
    })?;
    entries.into_iter().map(ManifestEntry::into_tree).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_stores_and_dedupes_blobs() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        let blobs = temp.path().join("blobs");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a.txt"), "duplicate\n").unwrap();
        fs::write(root.join("b.txt"), "duplicate\n").unwrap();
        fs::write(root.join("c.bin"), [0u8, 1, 2]).unwrap();

        let tree = snapshot(&root, &blobs).unwrap();
        assert_eq!(tree.len(), 3);
        assert_eq!(tree["a.txt"], tree["b.txt"]);

        let blob_count = fs::read_dir(&blobs).unwrap().count();
        assert_eq!(blob_count, 2);

        if let TreeEntry::File { sha256, size, .. } = &tree["a.txt"] {
            assert_eq!(*size, 10);
            let stored = fs::read(blobs.join(sha256)).unwrap();
            assert_eq!(stored, b"duplicate\n");
        } else {
            panic!("a.txt is not a file entry");
        }
    }

    #[test]
    fn walk_tree_records_all_kinds_without_following_symlinks() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("sub/file.txt"), "hello\n").unwrap();
        std::os::unix::fs::symlink("sub", root.join("link")).unwrap();

        let tree = walk_tree(&root).unwrap();
        assert_eq!(tree.len(), 3);
        assert!(matches!(tree["sub"], TreeEntry::Dir { .. }));
        assert!(matches!(tree["sub/file.txt"], TreeEntry::File { .. }));
        assert_eq!(
            tree["link"],
            TreeEntry::Symlink {
                target: "sub".into()
            }
        );
    }

    #[test]
    fn walk_tree_records_permission_modes() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("script.sh"), "#!/bin/sh\n").unwrap();
        fs::set_permissions(
            root.join("script.sh"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        fs::set_permissions(root.join("sub"), std::fs::Permissions::from_mode(0o700)).unwrap();

        let tree = walk_tree(&root).unwrap();
        assert!(
            matches!(
                tree["script.sh"],
                TreeEntry::File {
                    mode: Some(0o755),
                    ..
                }
            ),
            "script.sh entry was {:?}",
            tree["script.sh"]
        );
        assert_eq!(tree["sub"], TreeEntry::Dir { mode: Some(0o700) });
    }

    #[test]
    fn manifest_roundtrips() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        let blobs = temp.path().join("blobs");
        fs::create_dir_all(root.join("nested/deep")).unwrap();
        fs::write(root.join("nested/deep/data.txt"), "payload\n").unwrap();
        std::os::unix::fs::symlink("nested", root.join("alias")).unwrap();

        let tree = snapshot(&root, &blobs).unwrap();
        let manifest_path = temp.path().join("manifest.json");
        write_manifest(&manifest_path, &tree).unwrap();
        assert_eq!(load_manifest(&manifest_path).unwrap(), tree);
    }

    #[test]
    fn manifest_without_modes_parses_with_none() {
        let temp = tempfile::tempdir().unwrap();
        let manifest_path = temp.path().join("manifest.json");
        fs::write(
            &manifest_path,
            r#"[
                {"path": "d", "kind": "dir"},
                {"path": "d/f.txt", "kind": "file", "sha256": "abc", "size": 3}
            ]"#,
        )
        .unwrap();
        let tree = load_manifest(&manifest_path).unwrap();
        assert_eq!(tree["d"], TreeEntry::Dir { mode: None });
        assert_eq!(
            tree["d/f.txt"],
            TreeEntry::File {
                sha256: "abc".into(),
                size: 3,
                mode: None
            }
        );
    }

    #[test]
    fn manifest_missing_hash_is_damaged() {
        let temp = tempfile::tempdir().unwrap();
        let manifest_path = temp.path().join("manifest.json");
        fs::write(&manifest_path, r#"[{"path": "f.txt", "kind": "file"}]"#).unwrap();
        assert!(matches!(
            load_manifest(&manifest_path),
            Err(WorkspaceError::Damaged(_))
        ));
    }
}
