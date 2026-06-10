//! On-disk registry of locked workspaces.
//!
//! The registry lives at `<state>/workspaces/registry.json` and maps each
//! canonicalized workspace path to its lock record (snapshot id,
//! containerization strategy, current attachment). Mutations are guarded by
//! a sibling lock file created with `create_new`, so concurrent harness
//! processes serialize their registry updates.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use silo_core::error::WorkspaceError;

use crate::ContainerStrategy;

/// A harness currently using a locked workspace.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Attachment {
    pub harness_id: String,
    pub pid: u32,
}

/// One locked workspace.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct RegistryEntry {
    /// Snapshot id; names the per-workspace directory under
    /// `<state>/workspaces/`.
    pub id: String,
    pub locked: bool,
    pub strategy: ContainerStrategy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attached: Option<Attachment>,
    /// Extra warnings recorded at lock time, e.g. a strategy fallback note.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// Map from canonicalized workspace path to its lock record.
pub(crate) type Registry = BTreeMap<String, RegistryEntry>;

fn registry_path(workspaces_dir: &Path) -> PathBuf {
    workspaces_dir.join("registry.json")
}

fn lock_file_path(workspaces_dir: &Path) -> PathBuf {
    workspaces_dir.join("registry.json.lock")
}

/// Holds the registry lock file; removes it on drop.
pub(crate) struct RegistryLock {
    path: PathBuf,
}

impl Drop for RegistryLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Takes the registry lock, retrying with a bounded backoff.
pub(crate) fn acquire_lock(workspaces_dir: &Path) -> Result<RegistryLock, WorkspaceError> {
    acquire_lock_with(workspaces_dir, 200, Duration::from_millis(10))
}

pub(crate) fn acquire_lock_with(
    workspaces_dir: &Path,
    attempts: u32,
    delay: Duration,
) -> Result<RegistryLock, WorkspaceError> {
    fs::create_dir_all(workspaces_dir)?;
    let path = lock_file_path(workspaces_dir);
    for attempt in 0..attempts {
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(_) => return Ok(RegistryLock { path }),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                if attempt + 1 < attempts {
                    std::thread::sleep(delay);
                }
            }
            Err(e) => return Err(WorkspaceError::Io(e)),
        }
    }
    Err(WorkspaceError::Setup(format!(
        "timed out waiting for the workspace registry lock at {}",
        path.display()
    )))
}

pub(crate) fn load(workspaces_dir: &Path) -> Result<Registry, WorkspaceError> {
    let path = registry_path(workspaces_dir);
    match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| {
            WorkspaceError::Damaged(format!(
                "workspace registry at {} is unreadable: {e}",
                path.display()
            ))
        }),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Registry::new()),
        Err(e) => Err(WorkspaceError::Io(e)),
    }
}

/// Writes the registry atomically (temporary file plus rename), so readers
/// never observe a partially written file.
pub(crate) fn save(workspaces_dir: &Path, registry: &Registry) -> Result<(), WorkspaceError> {
    fs::create_dir_all(workspaces_dir)?;
    let bytes = serde_json::to_vec_pretty(registry)
        .map_err(|e| WorkspaceError::Setup(format!("cannot serialize workspace registry: {e}")))?;
    let tmp = workspaces_dir.join("registry.json.tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, registry_path(workspaces_dir))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_entry() -> RegistryEntry {
        RegistryEntry {
            id: "abc123def456".into(),
            locked: true,
            strategy: ContainerStrategy::PlainDir,
            attached: Some(Attachment {
                harness_id: "h-1".into(),
                pid: 4242,
            }),
            warnings: vec!["note".into()],
        }
    }

    #[test]
    fn registry_roundtrips() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("workspaces");
        let mut registry = Registry::new();
        registry.insert("/some/path".into(), sample_entry());
        save(&dir, &registry).unwrap();
        assert_eq!(load(&dir).unwrap(), registry);
    }

    #[test]
    fn missing_registry_loads_empty() {
        let temp = tempfile::tempdir().unwrap();
        assert!(load(&temp.path().join("workspaces")).unwrap().is_empty());
    }

    #[test]
    fn corrupt_registry_reports_damaged() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("workspaces");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("registry.json"), "not json").unwrap();
        assert!(matches!(load(&dir), Err(WorkspaceError::Damaged(_))));
    }

    #[test]
    fn lock_is_exclusive_and_released_on_drop() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("workspaces");

        let guard = acquire_lock(&dir).unwrap();
        let contended = acquire_lock_with(&dir, 2, Duration::from_millis(1));
        assert!(matches!(contended, Err(WorkspaceError::Setup(_))));

        drop(guard);
        assert!(!lock_file_path(&dir).exists());
        let reacquired = acquire_lock(&dir).unwrap();
        drop(reacquired);
        assert!(!lock_file_path(&dir).exists());
    }
}
