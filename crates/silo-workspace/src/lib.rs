//! Workspace lifecycle: lock, attach, unlock.
//!
//! A workspace is a directory the user hands over to a harness. Locking
//! snapshots every file (path, hash, content for diffing) into the state
//! directory and, where the platform allows, moves the data into a
//! container that the unsandboxed environment cannot casually touch.
//! Unlocking force-terminates any harness using the workspace, restores
//! plain-directory access, and reports every change since locking — with
//! known auto-exec surfaces (git hooks, `.envrc`, `.vscode` configuration,
//! `package.json` scripts, and the like) flagged explicitly.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use silo_core::error::WorkspaceError;
use silo_core::paths;

pub mod autoexec;
mod diff;
mod registry;
mod snapshot;
mod strategy;

pub use strategy::{ContainerStrategy, MARKER_FILE_NAME};

use registry::Attachment;

const DATA_DIR: &str = "data";
const BLOBS_DIR: &str = "blobs";
const MANIFEST_FILE: &str = "manifest.json";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FileChange {
    /// Path relative to the workspace root.
    pub path: String,
    pub kind: ChangeKind,
    /// Unified diff against the locked snapshot, when both sides are text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
    /// Metadata difference, e.g. "mode 0644 -> 0755".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AutoExecFlag {
    /// Path relative to the workspace root.
    pub path: String,
    /// Why this file can cause code execution outside the sandbox.
    pub reason: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct UnlockReport {
    pub changes: Vec<FileChange>,
    /// Changed files that are auto-exec surfaces; review these first.
    pub auto_exec_flags: Vec<AutoExecFlag>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceStatus {
    pub path: PathBuf,
    pub locked: bool,
    /// Harness id currently attached, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attached_harness: Option<String>,
    /// Platform caveats, e.g. "host can still read the workspace while
    /// locked on this platform".
    #[serde(default)]
    pub warnings: Vec<String>,
}

/// A locked workspace mounted for a running harness. Dropping (or calling
/// `detach`) releases the mount; the workspace stays locked.
pub struct AttachedWorkspace {
    /// Directory the sandbox should mount as its read/write workspace.
    pub mount_path: PathBuf,
    detach_guard: Option<Box<dyn FnOnce() + Send>>,
}

impl AttachedWorkspace {
    pub fn detach(mut self) {
        if let Some(guard) = self.detach_guard.take() {
            guard();
        }
    }
}

impl Drop for AttachedWorkspace {
    fn drop(&mut self) {
        if let Some(guard) = self.detach_guard.take() {
            guard();
        }
    }
}

/// Manages workspace state under the harness state directory.
pub struct WorkspaceManager {
    state_dir: PathBuf,
    strategy: ContainerStrategy,
}

impl WorkspaceManager {
    /// Creates a manager using the default containerization strategy for
    /// the platform.
    pub fn new(state_dir: PathBuf) -> Self {
        WorkspaceManager::with_strategy(state_dir, ContainerStrategy::default_for_platform())
    }

    /// Creates a manager that containerizes new locks with the given
    /// strategy. Existing locks keep the strategy recorded at lock time.
    pub fn with_strategy(state_dir: PathBuf, strategy: ContainerStrategy) -> Self {
        WorkspaceManager {
            state_dir,
            strategy,
        }
    }

    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    fn workspaces_root(&self) -> PathBuf {
        paths::workspaces_dir(&self.state_dir)
    }

    /// Locks `path` as a workspace (creating it, empty, if it does not
    /// exist). Snapshots all contents for the later unlock diff.
    pub fn lock(&self, path: &Path) -> Result<WorkspaceStatus, WorkspaceError> {
        fs::create_dir_all(path)?;
        let canon = fs::canonicalize(path)?;
        let workspaces_root = self.workspaces_root();
        fs::create_dir_all(&workspaces_root)?;
        let state_canon = fs::canonicalize(&self.state_dir)?;
        if canon.starts_with(&state_canon) || state_canon.starts_with(&canon) {
            return Err(WorkspaceError::Setup(format!(
                "workspace {} cannot contain or live inside the harness state directory {}",
                canon.display(),
                state_canon.display()
            )));
        }
        let key = path_to_string(&canon)?;

        let _guard = registry::acquire_lock(&workspaces_root)?;
        let mut reg = registry::load(&workspaces_root)?;
        if reg.get(&key).is_some_and(|entry| entry.locked) {
            return Err(WorkspaceError::Locked(key));
        }

        let id = silo_core::short_id();
        let workspace_dir = workspaces_root.join(&id);
        let tree = snapshot::snapshot(&canon, &workspace_dir.join(BLOBS_DIR))?;
        snapshot::write_manifest(&workspace_dir.join(MANIFEST_FILE), &tree)?;

        let mut extra_warnings = Vec::new();
        let mut strategy = self.strategy;
        match strategy {
            ContainerStrategy::PlainDir => {
                strategy::move_contents(&canon, &workspace_dir.join(DATA_DIR))?;
            }
            ContainerStrategy::SparseBundle => {
                if let Err(e) = strategy::lock_into_sparsebundle(&canon, &workspace_dir, &id) {
                    strategy::cleanup_sparsebundle(&workspace_dir);
                    strategy = ContainerStrategy::PlainDir;
                    extra_warnings.push(format!(
                        "hdiutil failed ({e}); using the plain-directory strategy instead"
                    ));
                    strategy::move_contents(&canon, &workspace_dir.join(DATA_DIR))?;
                }
            }
        }
        strategy::write_marker(&canon)?;
        strategy::make_dir_readonly(&canon)?;

        reg.insert(
            key,
            registry::RegistryEntry {
                id,
                locked: true,
                strategy,
                attached: None,
                warnings: extra_warnings.clone(),
            },
        );
        registry::save(&workspaces_root, &reg)?;

        let mut warnings = vec![strategy.warning().to_string()];
        warnings.extend(extra_warnings);
        Ok(WorkspaceStatus {
            path: canon,
            locked: true,
            attached_harness: None,
            warnings,
        })
    }

    /// Unlocks a workspace: terminates any attached harness, restores
    /// plain-directory access, and reports all changes since locking.
    pub fn unlock(&self, path: &Path) -> Result<UnlockReport, WorkspaceError> {
        let canon = canonicalize_lenient(path);
        let key = path_to_string(&canon)?;
        let workspaces_root = self.workspaces_root();
        let _guard = registry::acquire_lock(&workspaces_root)?;
        let mut reg = registry::load(&workspaces_root)?;
        let Some(entry) = reg.get(&key).cloned() else {
            return Err(WorkspaceError::NotLocked(key));
        };
        let workspace_dir = workspaces_root.join(&entry.id);

        if let Some(attachment) = &entry.attached {
            terminate_attached(attachment.pid);
            if entry.strategy == ContainerStrategy::SparseBundle {
                let (_, mountpoint) = strategy::sparsebundle_paths(&workspace_dir);
                let _ = strategy::run_hdiutil(&strategy::hdiutil_detach_args(&mountpoint));
            }
        }

        fs::create_dir_all(&canon)?;
        strategy::make_dir_writable(&canon)?;
        strategy::remove_marker(&canon);
        match entry.strategy {
            ContainerStrategy::PlainDir => {
                strategy::move_contents(&workspace_dir.join(DATA_DIR), &canon)?;
            }
            ContainerStrategy::SparseBundle => {
                strategy::restore_from_sparsebundle(&workspace_dir, &canon)?;
            }
        }

        let locked_tree = snapshot::load_manifest(&workspace_dir.join(MANIFEST_FILE))?;
        let current_tree = snapshot::walk_tree(&canon)?;
        let changes = diff::diff_trees(
            &locked_tree,
            &current_tree,
            &workspace_dir.join(BLOBS_DIR),
            &canon,
        )?;
        let auto_exec_flags = changes
            .iter()
            .filter_map(|change| {
                autoexec::match_path(&change.path).map(|reason| AutoExecFlag {
                    path: change.path.clone(),
                    reason: reason.to_string(),
                })
            })
            .collect();

        fs::remove_dir_all(&workspace_dir)?;
        reg.remove(&key);
        registry::save(&workspaces_root, &reg)?;

        Ok(UnlockReport {
            changes,
            auto_exec_flags,
        })
    }

    pub fn status(&self, path: &Path) -> Result<WorkspaceStatus, WorkspaceError> {
        let canon = canonicalize_lenient(path);
        let key = path_to_string(&canon)?;
        let reg = registry::load(&self.workspaces_root())?;
        match reg.get(&key) {
            None => Ok(WorkspaceStatus {
                path: canon,
                locked: false,
                attached_harness: None,
                warnings: Vec::new(),
            }),
            Some(entry) => {
                let mut warnings = vec![entry.strategy.warning().to_string()];
                warnings.extend(entry.warnings.iter().cloned());
                Ok(WorkspaceStatus {
                    path: canon,
                    locked: entry.locked,
                    attached_harness: entry
                        .attached
                        .as_ref()
                        .map(|attachment| attachment.harness_id.clone()),
                    warnings,
                })
            }
        }
    }

    /// Attaches a locked workspace for a harness. Fails if the workspace is
    /// not locked or already attached.
    pub fn attach(
        &self,
        path: &Path,
        harness_id: &str,
    ) -> Result<AttachedWorkspace, WorkspaceError> {
        let canon = canonicalize_lenient(path);
        let key = path_to_string(&canon)?;
        let workspaces_root = self.workspaces_root();
        let _guard = registry::acquire_lock(&workspaces_root)?;
        let mut reg = registry::load(&workspaces_root)?;
        let entry = reg
            .get_mut(&key)
            .ok_or_else(|| WorkspaceError::NotLocked(key.clone()))?;
        if !entry.locked {
            return Err(WorkspaceError::NotLocked(key));
        }
        if let Some(attachment) = &entry.attached {
            return Err(WorkspaceError::Setup(format!(
                "workspace {key} is already attached to harness {} (pid {})",
                attachment.harness_id, attachment.pid
            )));
        }
        let strategy_kind = entry.strategy;
        let workspace_dir = workspaces_root.join(&entry.id);
        let mount_path = match strategy_kind {
            ContainerStrategy::PlainDir => {
                let data = workspace_dir.join(DATA_DIR);
                if !data.is_dir() {
                    return Err(WorkspaceError::Damaged(format!(
                        "workspace data directory is missing at {}",
                        data.display()
                    )));
                }
                data
            }
            ContainerStrategy::SparseBundle => {
                let (bundle, mountpoint) = strategy::sparsebundle_paths(&workspace_dir);
                fs::create_dir_all(&mountpoint)?;
                strategy::run_hdiutil(&strategy::hdiutil_attach_args(&bundle, &mountpoint))?;
                mountpoint
            }
        };
        entry.attached = Some(Attachment {
            harness_id: harness_id.to_string(),
            pid: std::process::id(),
        });
        registry::save(&workspaces_root, &reg)?;

        let state_dir = self.state_dir.clone();
        let detach_guard: Box<dyn FnOnce() + Send> = Box::new(move || {
            detach_workspace(&state_dir, &key, strategy_kind);
        });
        Ok(AttachedWorkspace {
            mount_path,
            detach_guard: Some(detach_guard),
        })
    }
}

/// Releases a workspace attachment: unmounts the image when the strategy
/// uses one and clears the registry attachment. Errors are logged and
/// swallowed because this runs from `Drop`.
fn detach_workspace(state_dir: &Path, key: &str, strategy_kind: ContainerStrategy) {
    let workspaces_root = paths::workspaces_dir(state_dir);
    let guard = match registry::acquire_lock(&workspaces_root) {
        Ok(guard) => guard,
        Err(e) => {
            tracing::warn!("workspace detach: cannot take the registry lock: {e}");
            return;
        }
    };
    let mut reg = match registry::load(&workspaces_root) {
        Ok(reg) => reg,
        Err(e) => {
            tracing::warn!("workspace detach: cannot load the registry: {e}");
            return;
        }
    };
    if let Some(entry) = reg.get_mut(key) {
        if strategy_kind == ContainerStrategy::SparseBundle {
            let (_, mountpoint) = strategy::sparsebundle_paths(&workspaces_root.join(&entry.id));
            let _ = strategy::run_hdiutil(&strategy::hdiutil_detach_args(&mountpoint));
        }
        entry.attached = None;
        if let Err(e) = registry::save(&workspaces_root, &reg) {
            tracing::warn!("workspace detach: cannot save the registry: {e}");
        }
    }
    drop(guard);
}

/// Sends SIGTERM to an attached harness, waits up to five seconds for it
/// to exit, then sends SIGKILL. Errors are ignored so a stale attachment
/// never blocks an unlock. The current process and pids 0 and 1 are never
/// signalled.
#[cfg(unix)]
fn terminate_attached(pid: u32) {
    use nix::sys::signal::{kill, Signal};
    use nix::sys::wait::{waitpid, WaitPidFlag};
    use nix::unistd::Pid;
    use std::time::{Duration, Instant};

    if pid == 0 || pid == 1 || pid == std::process::id() {
        return;
    }
    let target = Pid::from_raw(pid as i32);
    if kill(target, Signal::SIGTERM).is_err() {
        return;
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        // Reaps the process when it is a child of this one, so the
        // existence check below sees it disappear.
        let _ = waitpid(target, Some(WaitPidFlag::WNOHANG));
        if kill(target, None).is_err() {
            return;
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let _ = kill(target, Signal::SIGKILL);
    let _ = waitpid(target, Some(WaitPidFlag::WNOHANG));
}

#[cfg(not(unix))]
fn terminate_attached(_pid: u32) {}

/// Canonicalizes the longest existing ancestor and reattaches the
/// remainder, so paths whose final components do not exist still map to a
/// stable registry key.
fn canonicalize_lenient(path: &Path) -> PathBuf {
    if let Ok(real) = fs::canonicalize(path) {
        return real;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) if !parent.as_os_str().is_empty() => {
            canonicalize_lenient(parent).join(name)
        }
        _ => path.to_path_buf(),
    }
}

pub(crate) fn path_to_string(path: &Path) -> Result<String, WorkspaceError> {
    path.to_str().map(str::to_owned).ok_or_else(|| {
        WorkspaceError::Setup(format!("path is not valid UTF-8: {}", path.display()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalize_lenient_resolves_the_existing_prefix() {
        let temp = tempfile::tempdir().unwrap();
        let real = fs::canonicalize(temp.path()).unwrap();
        let missing = temp.path().join("missing-child");
        assert_eq!(canonicalize_lenient(&missing), real.join("missing-child"));
    }

    #[test]
    fn default_strategy_matches_the_platform() {
        let expected = if cfg!(target_os = "macos") {
            ContainerStrategy::SparseBundle
        } else {
            ContainerStrategy::PlainDir
        };
        assert_eq!(ContainerStrategy::default_for_platform(), expected);
    }

    #[test]
    fn status_of_an_unknown_path_is_unlocked() {
        let temp = tempfile::tempdir().unwrap();
        let manager =
            WorkspaceManager::with_strategy(temp.path().join("state"), ContainerStrategy::PlainDir);
        let status = manager.status(&temp.path().join("nowhere")).unwrap();
        assert!(!status.locked);
        assert!(status.attached_harness.is_none());
        assert!(status.warnings.is_empty());
    }
}
