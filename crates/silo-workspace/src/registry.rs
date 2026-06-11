//! On-disk registry of locked workspaces.
//!
//! The registry lives at `<state>/workspaces/registry.json` and maps each
//! canonicalized workspace path to its lock record (snapshot id,
//! containerization strategy, current attachment). Mutations are guarded by
//! a sibling lock file created with `create_new`, so concurrent harness
//! processes serialize their registry updates. The lock file records the
//! holder's pid and process start time; a waiter that finds the holder dead
//! (or the file unreadable and older than a minute) removes the stale file
//! and retries.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use silo_core::error::WorkspaceError;

use crate::process;
use crate::ContainerStrategy;

/// A harness currently using a locked workspace.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Attachment {
    pub harness_id: String,
    pub pid: u32,
    /// Process start-time hint captured at attach time; detects pid reuse.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub start_hint: String,
    /// Random token issued at attach time; only the detach guard holding
    /// the same token may clear this attachment.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub token: String,
}

/// An additional process sharing an attached workspace, e.g. a user shell
/// inspecting it while a harness runs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SecondaryAttachment {
    /// What the attachment is for, e.g. "shell".
    pub purpose: String,
    pub pid: u32,
    /// Process start-time hint captured at attach time; detects pid reuse.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub start_hint: String,
    /// Random token issued at attach time; only the detach guard holding
    /// the same token may clear this attachment.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub token: String,
}

/// One locked workspace.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct RegistryEntry {
    /// Snapshot id; names the per-workspace directory under
    /// `<state>/workspaces/`.
    pub id: String,
    pub locked: bool,
    /// True while an unlock is in progress; new attachments are refused
    /// until the unlock completes.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unlocking: bool,
    pub strategy: ContainerStrategy,
    /// The primary attachment: the harness using the workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attached: Option<Attachment>,
    /// Secondary attachments sharing the mount, e.g. user shells.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secondary_attachments: Vec<SecondaryAttachment>,
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

/// Identity of the process holding the registry lock, stored in the lock
/// file so waiters can detect a stale lock.
#[derive(Serialize, Deserialize)]
struct LockHolder {
    pid: u32,
    #[serde(default)]
    start_hint: String,
}

/// Age past which an unreadable lock file counts as stale.
const UNREADABLE_LOCK_STALE_AFTER: Duration = Duration::from_secs(60);

/// Holds the registry lock file; removes it on drop.
pub(crate) struct RegistryLock {
    path: PathBuf,
}

impl Drop for RegistryLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Takes the registry lock, waiting up to thirty seconds under contention.
pub(crate) fn acquire_lock(workspaces_dir: &Path) -> Result<RegistryLock, WorkspaceError> {
    acquire_lock_with(workspaces_dir, 600, Duration::from_millis(50))
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
            Ok(mut file) => {
                let pid = std::process::id();
                let holder = LockHolder {
                    pid,
                    start_hint: process::start_hint(pid),
                };
                if let Ok(bytes) = serde_json::to_vec(&holder) {
                    let _ = file.write_all(&bytes);
                }
                return Ok(RegistryLock { path });
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                if remove_if_stale(&path) {
                    continue;
                }
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

/// Removes the lock file when its holder is stale: the recorded pid is
/// dead (or recycled, per the start-time hint), or the file is unreadable
/// and older than [`UNREADABLE_LOCK_STALE_AFTER`]. Before removal the file
/// is re-read and the removal is skipped when the contents changed.
fn remove_if_stale(path: &Path) -> bool {
    let Ok(bytes) = fs::read(path) else {
        return false;
    };
    let stale = match serde_json::from_slice::<LockHolder>(&bytes) {
        Ok(holder) => !process::alive_with_hint(holder.pid, &holder.start_hint),
        Err(_) => fs::metadata(path)
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > UNREADABLE_LOCK_STALE_AFTER),
    };
    if !stale {
        return false;
    }
    match fs::read(path) {
        Ok(reread) if reread == bytes => fs::remove_file(path).is_ok(),
        _ => false,
    }
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
            unlocking: false,
            strategy: ContainerStrategy::PlainDir,
            attached: Some(Attachment {
                harness_id: "h-1".into(),
                pid: 4242,
                start_hint: "Mon Jun  1 10:00:00 2026".into(),
                token: "tok-primary".into(),
            }),
            secondary_attachments: vec![SecondaryAttachment {
                purpose: "shell".into(),
                pid: 4243,
                start_hint: String::new(),
                token: "tok-shell".into(),
            }],
            warnings: vec!["note".into()],
        }
    }

    fn write_lock_file(dir: &Path, contents: &[u8]) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let path = lock_file_path(dir);
        fs::write(&path, contents).unwrap();
        path
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
    fn registry_without_secondary_attachments_parses() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("workspaces");
        fs::create_dir_all(&dir).unwrap();
        let json = serde_json::json!({
            "/some/path": {
                "id": "abc123def456",
                "locked": true,
                "strategy": "plain_dir",
                "attached": { "harness_id": "h-1", "pid": 4242 }
            }
        });
        fs::write(
            dir.join("registry.json"),
            serde_json::to_string_pretty(&json).unwrap(),
        )
        .unwrap();
        let registry = load(&dir).unwrap();
        let entry = registry.get("/some/path").unwrap();
        assert!(entry.secondary_attachments.is_empty());
        let attached = entry.attached.as_ref().unwrap();
        assert_eq!(attached.harness_id, "h-1");
        // Records written before the identity and token fields existed
        // parse with empty defaults.
        assert!(attached.start_hint.is_empty());
        assert!(attached.token.is_empty());
    }

    #[test]
    fn registry_with_an_ext4_fuse_entry_parses() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("workspaces");
        fs::create_dir_all(&dir).unwrap();
        let json = serde_json::json!({
            "/some/path": {
                "id": "abc123def456",
                "locked": true,
                "strategy": "ext4_fuse"
            }
        });
        fs::write(
            dir.join("registry.json"),
            serde_json::to_string_pretty(&json).unwrap(),
        )
        .unwrap();
        let registry = load(&dir).unwrap();
        let entry = registry.get("/some/path").unwrap();
        assert_eq!(entry.strategy, ContainerStrategy::Ext4Fuse);
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

    #[test]
    fn the_lock_file_records_the_holder() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("workspaces");
        let guard = acquire_lock(&dir).unwrap();
        let holder: LockHolder =
            serde_json::from_slice(&fs::read(lock_file_path(&dir)).unwrap()).unwrap();
        assert_eq!(holder.pid, std::process::id());
        drop(guard);
    }

    #[test]
    fn a_stale_lock_with_a_dead_holder_is_replaced() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("workspaces");

        // A process that has already exited and been reaped: its pid is
        // dead.
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let dead_pid = child.id();
        child.wait().unwrap();

        let holder = LockHolder {
            pid: dead_pid,
            start_hint: String::new(),
        };
        write_lock_file(&dir, &serde_json::to_vec(&holder).unwrap());

        let guard = acquire_lock_with(&dir, 2, Duration::from_millis(1)).unwrap();
        drop(guard);
    }

    #[test]
    fn a_lock_holder_with_a_mismatched_identity_is_stale() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("workspaces");

        // The recorded pid is alive but the start hint belongs to another
        // process generation: the lock is stale.
        let holder = LockHolder {
            pid: std::process::id(),
            start_hint: "Mon Jan  1 00:00:00 2001".into(),
        };
        write_lock_file(&dir, &serde_json::to_vec(&holder).unwrap());

        let guard = acquire_lock_with(&dir, 2, Duration::from_millis(1)).unwrap();
        drop(guard);
    }

    #[test]
    fn a_live_holder_still_blocks() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("workspaces");

        let pid = std::process::id();
        let holder = LockHolder {
            pid,
            start_hint: process::start_hint(pid),
        };
        let path = write_lock_file(&dir, &serde_json::to_vec(&holder).unwrap());

        let contended = acquire_lock_with(&dir, 3, Duration::from_millis(1));
        assert!(matches!(contended, Err(WorkspaceError::Setup(_))));
        assert!(path.exists());
        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn an_unreadable_fresh_lock_file_blocks() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("workspaces");
        let path = write_lock_file(&dir, b"not json");

        let contended = acquire_lock_with(&dir, 3, Duration::from_millis(1));
        assert!(matches!(contended, Err(WorkspaceError::Setup(_))));
        assert!(path.exists());
        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn an_unreadable_old_lock_file_is_replaced() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("workspaces");
        let path = write_lock_file(&dir, b"not json");

        let two_minutes_ago = std::time::SystemTime::now() - Duration::from_secs(120);
        let file = OpenOptions::new().write(true).open(&path).unwrap();
        file.set_times(fs::FileTimes::new().set_modified(two_minutes_ago))
            .unwrap();
        drop(file);

        let guard = acquire_lock_with(&dir, 2, Duration::from_millis(1)).unwrap();
        drop(guard);
    }
}
