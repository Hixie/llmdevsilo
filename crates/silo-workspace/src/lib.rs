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
use std::time::Duration;

use serde::{Deserialize, Serialize};
use silo_core::error::WorkspaceError;
use silo_core::paths;

pub mod autoexec;
mod diff;
mod process;
mod registry;
mod snapshot;
mod strategy;

pub use strategy::{ContainerStrategy, MARKER_FILE_NAME};

use registry::{Attachment, RegistryEntry, SecondaryAttachment};

const DATA_DIR: &str = "data";
const BLOBS_DIR: &str = "blobs";
const MANIFEST_FILE: &str = "manifest.json";

/// Attempts made by a detach guard to record its removal in the registry.
const DETACH_ATTEMPTS: u32 = 3;

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
    /// Number of live shell attachments sharing the mount.
    #[serde(default)]
    pub live_shells: usize,
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
    /// exist). Snapshots all contents for the later unlock diff. The
    /// registry lock is held only to reserve and to commit the entry; the
    /// snapshot walk and container creation run without it.
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
        let id = silo_core::short_id();

        // Reserve the key so a concurrent lock of the same path fails.
        {
            let _guard = registry::acquire_lock(&workspaces_root)?;
            let mut reg = registry::load(&workspaces_root)?;
            if reg.get(&key).is_some_and(|entry| entry.locked) {
                return Err(WorkspaceError::Locked(key));
            }
            reg.insert(
                key.clone(),
                RegistryEntry {
                    id: id.clone(),
                    locked: true,
                    unlocking: false,
                    strategy: self.strategy,
                    attached: None,
                    secondary_attachments: Vec::new(),
                    warnings: Vec::new(),
                },
            );
            registry::save(&workspaces_root, &reg)?;
        }

        let workspace_dir = workspaces_root.join(&id);
        let slow_work = (|| -> Result<(ContainerStrategy, Vec<String>), WorkspaceError> {
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
            Ok((strategy, extra_warnings))
        })();

        let (strategy, extra_warnings) = match slow_work {
            Ok(result) => result,
            Err(error) => {
                undo_partial_lock(&canon, &workspace_dir);
                let rollback = (|| -> Result<(), WorkspaceError> {
                    let _guard = registry::acquire_lock(&workspaces_root)?;
                    let mut reg = registry::load(&workspaces_root)?;
                    if reg.get(&key).is_some_and(|entry| entry.id == id) {
                        reg.remove(&key);
                        registry::save(&workspaces_root, &reg)?;
                    }
                    Ok(())
                })();
                if let Err(rollback_error) = rollback {
                    tracing::warn!(
                        "cannot remove the reserved registry entry for {key}: {rollback_error}"
                    );
                }
                return Err(error);
            }
        };

        // Commit: the reserved entry must still be ours.
        {
            let _guard = registry::acquire_lock(&workspaces_root)?;
            let mut reg = registry::load(&workspaces_root)?;
            match reg.get_mut(&key) {
                Some(entry) if entry.id == id => {
                    entry.strategy = strategy;
                    entry.warnings = extra_warnings.clone();
                    registry::save(&workspaces_root, &reg)?;
                }
                _ => {
                    undo_partial_lock(&canon, &workspace_dir);
                    return Err(WorkspaceError::Setup(format!(
                        "the registry entry for {key} changed while locking; \
                         the lock was rolled back"
                    )));
                }
            }
        }

        let mut warnings = vec![strategy.warning().to_string()];
        warnings.extend(extra_warnings);
        Ok(WorkspaceStatus {
            path: canon,
            locked: true,
            attached_harness: None,
            live_shells: 0,
            warnings,
        })
    }

    /// Unlocks a workspace: terminates any attached processes, restores
    /// plain-directory access, and reports all changes since locking.
    ///
    /// The steps are ordered so a failure part-way through never destroys
    /// state that has not been safely released, and every step is
    /// idempotent: re-running `unlock` after a mid-step failure resumes
    /// and completes with the full report. The snapshot, the container,
    /// and the registry entry are kept until the report exists.
    pub fn unlock(&self, path: &Path) -> Result<UnlockReport, WorkspaceError> {
        let canon = canonicalize_lenient(path);
        let key = path_to_string(&canon)?;
        let workspaces_root = self.workspaces_root();

        // Step 1: read the entry and mark the unlock as in progress so no
        // new attachment can appear. The registry lock is held only for
        // this read.
        let entry = {
            let _guard = registry::acquire_lock(&workspaces_root)?;
            let mut reg = registry::load(&workspaces_root)?;
            let Some(entry) = reg.get_mut(&key) else {
                return Err(WorkspaceError::NotLocked(key));
            };
            entry.unlocking = true;
            let entry = entry.clone();
            registry::save(&workspaces_root, &reg)?;
            entry
        };
        let workspace_dir = workspaces_root.join(&entry.id);

        // Step 2: terminate attached processes, outside the registry
        // lock, then re-verify that they are gone.
        let mut targets: Vec<(u32, String)> = Vec::new();
        if let Some(attachment) = &entry.attached {
            targets.push((attachment.pid, attachment.start_hint.clone()));
        }
        for secondary in &entry.secondary_attachments {
            targets.push((secondary.pid, secondary.start_hint.clone()));
        }
        for (pid, hint) in &targets {
            process::terminate(*pid, hint);
        }
        let survivors: Vec<u32> = targets
            .iter()
            .filter(|(pid, hint)| {
                !process::is_protected(*pid)
                    && process::running(*pid)
                    && process::identity_matches(*pid, hint)
            })
            .map(|(pid, _)| *pid)
            .collect();
        if !survivors.is_empty() {
            return Err(unlock_step_error(
                "terminating attachments",
                &format!("process(es) {survivors:?} did not exit"),
            ));
        }

        // Step 3: release the image mount. A failure aborts the unlock
        // with the registry entry, the snapshot, and the container intact.
        if entry.strategy == ContainerStrategy::SparseBundle {
            let (_, mountpoint) = strategy::sparsebundle_paths(&workspace_dir);
            strategy::detach_image(&mountpoint)
                .map_err(|e| unlock_step_error("detaching the workspace image", &e.to_string()))?;
        }

        // Step 4: restore the contents into the original directory.
        fs::create_dir_all(&canon)?;
        strategy::make_dir_writable(&canon)
            .map_err(|e| unlock_step_error("restoring directory access", &e.to_string()))?;
        strategy::remove_marker(&canon);
        match entry.strategy {
            ContainerStrategy::PlainDir => {
                // A missing data directory means a previous unlock attempt
                // already moved the contents back.
                let data = workspace_dir.join(DATA_DIR);
                if data.is_dir() {
                    strategy::move_contents(&data, &canon).map_err(|e| {
                        unlock_step_error("restoring the workspace contents", &e.to_string())
                    })?;
                }
            }
            ContainerStrategy::SparseBundle => {
                strategy::restore_from_sparsebundle(&workspace_dir, &canon).map_err(|e| {
                    unlock_step_error("restoring the workspace contents", &e.to_string())
                })?;
            }
        }

        // Step 5: produce the report from the kept snapshot.
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

        // Step 6: the snapshot directory must not be deleted while it
        // still hosts a live mount.
        let (_, mountpoint) = strategy::sparsebundle_paths(&workspace_dir);
        strategy::detach_image(&mountpoint)
            .map_err(|e| unlock_step_error("detaching the workspace image", &e.to_string()))?;

        // Step 7: remove the registry entry.
        {
            let _guard = registry::acquire_lock(&workspaces_root)?;
            let mut reg = registry::load(&workspaces_root)?;
            if reg.get(&key).is_some_and(|current| current.id == entry.id) {
                reg.remove(&key);
                registry::save(&workspaces_root, &reg)?;
            }
        }

        // Step 8: delete the snapshot directory.
        if strategy::is_mountpoint(&mountpoint) {
            tracing::warn!(
                "the workspace image at {} is mounted again; not deleting {}; \
                 detach it and remove the directory manually",
                mountpoint.display(),
                workspace_dir.display()
            );
        } else if let Err(e) = fs::remove_dir_all(&workspace_dir) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(
                    "cannot remove the workspace snapshot directory {}: {e}; \
                     remove it manually",
                    workspace_dir.display()
                );
            }
        }

        Ok(UnlockReport {
            changes,
            auto_exec_flags,
        })
    }

    /// Reports the lock and attachment state of `path`. Attachment records
    /// whose process is dead (or whose pid was recycled) are pruned from
    /// the registry as part of the read.
    pub fn status(&self, path: &Path) -> Result<WorkspaceStatus, WorkspaceError> {
        let canon = canonicalize_lenient(path);
        let key = path_to_string(&canon)?;
        let workspaces_root = self.workspaces_root();
        let _guard = registry::acquire_lock(&workspaces_root)?;
        let mut reg = registry::load(&workspaces_root)?;
        let Some(entry) = reg.get_mut(&key) else {
            return Ok(WorkspaceStatus {
                path: canon,
                locked: false,
                attached_harness: None,
                live_shells: 0,
                warnings: Vec::new(),
            });
        };
        let pruned = prune_dead_attachments(entry);
        let mut warnings = vec![entry.strategy.warning().to_string()];
        warnings.extend(entry.warnings.iter().cloned());
        let status = WorkspaceStatus {
            path: canon,
            locked: entry.locked,
            attached_harness: entry
                .attached
                .as_ref()
                .map(|attachment| attachment.harness_id.clone()),
            live_shells: entry.secondary_attachments.len(),
            warnings,
        };
        if pruned {
            registry::save(&workspaces_root, &reg)?;
        }
        Ok(status)
    }

    /// Attaches a locked workspace for a harness. Fails if the workspace
    /// is not locked or already attached to a live harness; a recorded
    /// attachment whose process is dead is pruned and replaced.
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
        if entry.unlocking {
            return Err(WorkspaceError::Setup(format!(
                "an unlock of workspace {key} is in progress; re-run `silo workspace unlock` \
                 to finish it"
            )));
        }
        prune_dead_attachments(entry);
        if let Some(attachment) = &entry.attached {
            return Err(WorkspaceError::Setup(format!(
                "workspace {key} is already attached to harness {} (pid {})",
                attachment.harness_id, attachment.pid
            )));
        }
        let strategy_kind = entry.strategy;
        let workspace_dir = workspaces_root.join(&entry.id);
        let mount_path = mount_for_entry(entry, &workspace_dir)?;
        let pid = std::process::id();
        let token = silo_core::short_id();
        entry.attached = Some(Attachment {
            harness_id: harness_id.to_string(),
            pid,
            start_hint: process::start_hint(pid),
            token: token.clone(),
        });
        registry::save(&workspaces_root, &reg)?;

        let state_dir = self.state_dir.clone();
        let detach_guard: Box<dyn FnOnce() + Send> = Box::new(move || {
            detach_workspace(&state_dir, &key, strategy_kind, &token);
        });
        Ok(AttachedWorkspace {
            mount_path,
            detach_guard: Some(detach_guard),
        })
    }

    /// Attaches a locked workspace for an inspection process (for example
    /// a user shell), sharing the mount with any attached harness. The
    /// workspace must be locked; a harness may or may not be attached.
    /// The returned guard removes the registration on detach/drop and,
    /// when it was the last attachment of any kind, releases the mount.
    pub fn attach_shared(&self, path: &Path) -> Result<AttachedWorkspace, WorkspaceError> {
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
        if entry.unlocking {
            return Err(WorkspaceError::Setup(format!(
                "an unlock of workspace {key} is in progress; re-run `silo workspace unlock` \
                 to finish it"
            )));
        }
        prune_dead_attachments(entry);
        let strategy_kind = entry.strategy;
        let workspace_dir = workspaces_root.join(&entry.id);
        let mount_path = mount_for_entry(entry, &workspace_dir)?;
        let pid = std::process::id();
        let token = silo_core::short_id();
        entry.secondary_attachments.push(SecondaryAttachment {
            purpose: "shell".into(),
            pid,
            start_hint: process::start_hint(pid),
            token: token.clone(),
        });
        registry::save(&workspaces_root, &reg)?;

        let state_dir = self.state_dir.clone();
        let detach_guard: Box<dyn FnOnce() + Send> = Box::new(move || {
            detach_shared_workspace(&state_dir, &key, strategy_kind, pid, &token);
        });
        Ok(AttachedWorkspace {
            mount_path,
            detach_guard: Some(detach_guard),
        })
    }
}

/// Resolves the mount path for an entry, mounting the image when the
/// strategy uses one and it is not already mounted.
fn mount_for_entry(entry: &RegistryEntry, workspace_dir: &Path) -> Result<PathBuf, WorkspaceError> {
    match entry.strategy {
        ContainerStrategy::PlainDir => {
            let data = workspace_dir.join(DATA_DIR);
            if !data.is_dir() {
                return Err(WorkspaceError::Damaged(format!(
                    "workspace data directory is missing at {}",
                    data.display()
                )));
            }
            Ok(data)
        }
        ContainerStrategy::SparseBundle => {
            let (bundle, mountpoint) = strategy::sparsebundle_paths(workspace_dir);
            fs::create_dir_all(&mountpoint)?;
            if !strategy::is_mountpoint(&mountpoint) {
                strategy::run_hdiutil(&strategy::hdiutil_attach_args(&bundle, &mountpoint))?;
            }
            Ok(mountpoint)
        }
    }
}

/// Best-effort rollback of a partially completed lock: restores the
/// directory contents and removes the snapshot directory.
fn undo_partial_lock(canon: &Path, workspace_dir: &Path) {
    let _ = strategy::make_dir_writable(canon);
    strategy::remove_marker(canon);
    let data = workspace_dir.join(DATA_DIR);
    if data.is_dir() {
        let _ = strategy::move_contents(&data, canon);
    }
    let (bundle, mountpoint) = strategy::sparsebundle_paths(workspace_dir);
    if bundle.exists() {
        let _ = strategy::restore_from_sparsebundle(workspace_dir, canon);
        let _ = strategy::detach_image(&mountpoint);
    }
    if !strategy::is_mountpoint(&mountpoint) {
        let _ = fs::remove_dir_all(workspace_dir);
    }
}

fn unlock_step_error(step: &str, detail: &str) -> WorkspaceError {
    WorkspaceError::Setup(format!(
        "unlock step '{step}' failed: {detail}; the workspace is unchanged for this step — \
         re-run unlock to resume"
    ))
}

/// Releases the primary workspace attachment: clears the registry
/// attachment created with the same token and, when no live attachments
/// remain, unmounts the image when the strategy uses one. While shells
/// remain attached the mount stays up; the last attachment releases it.
/// The cleanup is retried with backoff; a final failure is logged with
/// recovery instructions because this runs from `Drop`.
fn detach_workspace(state_dir: &Path, key: &str, strategy_kind: ContainerStrategy, token: &str) {
    retry_detach("workspace detach", key, || {
        let workspaces_root = paths::workspaces_dir(state_dir);
        let _guard = registry::acquire_lock_with(&workspaces_root, 50, Duration::from_millis(100))?;
        let mut reg = registry::load(&workspaces_root)?;
        let Some(entry) = reg.get_mut(key) else {
            return Ok(());
        };
        if entry
            .attached
            .as_ref()
            .is_some_and(|attachment| attachment.token == token)
        {
            entry.attached = None;
        }
        let release_mount = strategy_kind == ContainerStrategy::SparseBundle
            && !has_live_attachments(entry, &process::alive_with_hint);
        let mountpoint = strategy::sparsebundle_paths(&workspaces_root.join(&entry.id)).1;
        registry::save(&workspaces_root, &reg)?;
        if release_mount {
            strategy::detach_image(&mountpoint)?;
        }
        Ok(())
    });
}

/// Releases one shared (shell) attachment: removes the registration
/// created with the same token and, when it was the last attachment of
/// any kind, unmounts the image when the strategy uses one. The cleanup
/// is retried with backoff; a final failure is logged with recovery
/// instructions because this runs from `Drop`.
fn detach_shared_workspace(
    state_dir: &Path,
    key: &str,
    strategy_kind: ContainerStrategy,
    pid: u32,
    token: &str,
) {
    retry_detach("workspace shared detach", key, || {
        let workspaces_root = paths::workspaces_dir(state_dir);
        let _guard = registry::acquire_lock_with(&workspaces_root, 50, Duration::from_millis(100))?;
        let mut reg = registry::load(&workspaces_root)?;
        let Some(entry) = reg.get_mut(key) else {
            return Ok(());
        };
        if let Some(index) = entry
            .secondary_attachments
            .iter()
            .position(|secondary| secondary.pid == pid && secondary.token == token)
        {
            entry.secondary_attachments.remove(index);
        }
        let release_mount = strategy_kind == ContainerStrategy::SparseBundle
            && !has_live_attachments(entry, &process::alive_with_hint);
        let mountpoint = strategy::sparsebundle_paths(&workspaces_root.join(&entry.id)).1;
        registry::save(&workspaces_root, &reg)?;
        if release_mount {
            strategy::detach_image(&mountpoint)?;
        }
        Ok(())
    });
}

/// Runs a detach guard's registry cleanup, retrying with backoff. The
/// final failure is logged with recovery instructions.
fn retry_detach<F>(context: &str, key: &str, mut cleanup: F)
where
    F: FnMut() -> Result<(), WorkspaceError>,
{
    for attempt in 0..DETACH_ATTEMPTS {
        match cleanup() {
            Ok(()) => return,
            Err(error) if attempt + 1 < DETACH_ATTEMPTS => {
                tracing::warn!(
                    "{context} for {key} failed (attempt {}): {error}; retrying",
                    attempt + 1
                );
                std::thread::sleep(Duration::from_millis(500 * u64::from(attempt + 1)));
            }
            Err(error) => {
                tracing::error!(
                    "{context} for {key} failed after {DETACH_ATTEMPTS} attempts: {error}; \
                     the registry may still record this attachment — it is ignored once \
                     this process exits, and `silo workspace unlock {key}` clears it"
                );
            }
        }
    }
}

/// Drops attachment records whose process is dead or whose pid was
/// recycled. Returns true when anything was removed.
fn prune_dead_attachments(entry: &mut RegistryEntry) -> bool {
    let mut pruned = false;
    if let Some(attachment) = &entry.attached {
        if !process::alive_with_hint(attachment.pid, &attachment.start_hint) {
            entry.attached = None;
            pruned = true;
        }
    }
    let before = entry.secondary_attachments.len();
    entry
        .secondary_attachments
        .retain(|secondary| process::alive_with_hint(secondary.pid, &secondary.start_hint));
    pruned || entry.secondary_attachments.len() != before
}

/// Number of secondary attachments whose process is alive (pid and
/// identity hint both match).
fn live_secondary_count(entry: &RegistryEntry, alive: &dyn Fn(u32, &str) -> bool) -> usize {
    entry
        .secondary_attachments
        .iter()
        .filter(|secondary| alive(secondary.pid, &secondary.start_hint))
        .count()
}

/// True when the entry has a live primary attachment or any live
/// secondary attachment.
fn has_live_attachments(entry: &RegistryEntry, alive: &dyn Fn(u32, &str) -> bool) -> bool {
    let primary_live = entry
        .attached
        .as_ref()
        .is_some_and(|attachment| alive(attachment.pid, &attachment.start_hint));
    primary_live || live_secondary_count(entry, alive) > 0
}

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
        assert_eq!(status.live_shells, 0);
        assert!(status.warnings.is_empty());
    }

    fn entry_with(attached: Option<u32>, secondary_pids: &[u32]) -> RegistryEntry {
        RegistryEntry {
            id: "id".into(),
            locked: true,
            unlocking: false,
            strategy: ContainerStrategy::PlainDir,
            attached: attached.map(|pid| Attachment {
                harness_id: "h".into(),
                pid,
                start_hint: String::new(),
                token: "tok".into(),
            }),
            secondary_attachments: secondary_pids
                .iter()
                .map(|&pid| SecondaryAttachment {
                    purpose: "shell".into(),
                    pid,
                    start_hint: String::new(),
                    token: "tok".into(),
                })
                .collect(),
            warnings: Vec::new(),
        }
    }

    #[test]
    fn dead_secondaries_do_not_count_as_live() {
        let alive = |pid: u32, _hint: &str| pid == 10 || pid == 20;
        let entry = entry_with(None, &[10, 99, 20]);
        assert_eq!(live_secondary_count(&entry, &alive), 2);
        let entry = entry_with(None, &[99]);
        assert_eq!(live_secondary_count(&entry, &alive), 0);
    }

    #[test]
    fn the_mount_is_released_only_by_the_last_live_attachment() {
        let alive = |pid: u32, _hint: &str| pid < 50;

        // Live primary alone, live secondary alone, or both: keep the
        // mount.
        assert!(has_live_attachments(&entry_with(Some(10), &[]), &alive));
        assert!(has_live_attachments(&entry_with(None, &[20]), &alive));
        assert!(has_live_attachments(&entry_with(Some(10), &[20]), &alive));

        // Dead primary with a live secondary keeps the mount; the
        // secondary alone holds it.
        assert!(has_live_attachments(&entry_with(Some(99), &[20]), &alive));

        // No attachments, or only dead ones: release.
        assert!(!has_live_attachments(&entry_with(None, &[]), &alive));
        assert!(!has_live_attachments(&entry_with(Some(99), &[98]), &alive));
        assert!(!has_live_attachments(&entry_with(None, &[98]), &alive));
    }

    #[test]
    fn pruning_drops_dead_attachments_and_reports_changes() {
        // A dead pid in the primary slot and one dead secondary.
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let dead = child.id();
        child.wait().unwrap();
        let live = std::process::id();

        let mut entry = entry_with(Some(dead), &[live, dead]);
        assert!(prune_dead_attachments(&mut entry));
        assert!(entry.attached.is_none());
        assert_eq!(entry.secondary_attachments.len(), 1);
        assert_eq!(entry.secondary_attachments[0].pid, live);

        // A second pass changes nothing.
        assert!(!prune_dead_attachments(&mut entry));
    }
}
