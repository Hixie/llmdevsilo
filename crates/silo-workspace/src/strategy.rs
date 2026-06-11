//! Containerization strategies for locked workspaces.
//!
//! `PlainDir` moves the workspace contents into the harness state
//! directory and relies on file permissions. `SparseBundle` copies the
//! contents into a macOS sparse bundle disk image managed with `hdiutil`;
//! when `hdiutil` is unavailable the lock falls back to `PlainDir`.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use silo_core::error::WorkspaceError;

/// How a locked workspace's contents are held while locked.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContainerStrategy {
    /// Contents live in a plain directory under the state directory.
    PlainDir,
    /// Contents live in a macOS sparse bundle disk image.
    SparseBundle,
}

impl ContainerStrategy {
    /// The platform default: sparse bundle on macOS, plain directory
    /// elsewhere.
    pub fn default_for_platform() -> Self {
        if cfg!(target_os = "macos") {
            ContainerStrategy::SparseBundle
        } else {
            ContainerStrategy::PlainDir
        }
    }

    pub(crate) fn warning(self) -> &'static str {
        match self {
            ContainerStrategy::PlainDir => {
                "workspace contents moved under the harness state directory; \
                 they are protected by file permissions only"
            }
            ContainerStrategy::SparseBundle => {
                "the workspace image is mounted host-visible while a harness is attached"
            }
        }
    }
}

/// Name of the file left in the original directory while locked.
pub const MARKER_FILE_NAME: &str = "LOCKED_BY_LLMDEVSILO";

const MARKER_CONTENTS: &str = "\
This directory is a locked llmdevsilo workspace.

While locked, the contents are managed by the llmdevsilo harness and a
sandboxed coding agent may be working on them. Do not create, edit, or
delete files here from outside the sandbox.

To stop the harness, restore the contents, and review a report of every
change, unlock the workspace:

    silo workspace unlock <path-to-this-directory>
";

/// Volume-metadata entries macOS creates at the root of a mounted image;
/// they are not part of the workspace contents.
const VOLUME_METADATA: &[&str] = &[
    ".fseventsd",
    ".Spotlight-V100",
    ".Trashes",
    ".TemporaryItems",
];

pub(crate) fn write_marker(dir: &Path) -> io::Result<()> {
    fs::write(dir.join(MARKER_FILE_NAME), MARKER_CONTENTS)
}

pub(crate) fn remove_marker(dir: &Path) {
    let _ = fs::remove_file(dir.join(MARKER_FILE_NAME));
}

#[cfg(unix)]
pub(crate) fn make_dir_readonly(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = fs::metadata(dir)?.permissions();
    permissions.set_mode(permissions.mode() & !0o222);
    fs::set_permissions(dir, permissions)
}

#[cfg(unix)]
pub(crate) fn make_dir_writable(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = fs::metadata(dir)?.permissions();
    permissions.set_mode(permissions.mode() | 0o200);
    fs::set_permissions(dir, permissions)
}

/// Moves every entry of `from` into `to`, renaming when both are on the
/// same filesystem and copying plus deleting otherwise.
pub(crate) fn move_contents(from: &Path, to: &Path) -> io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let src = entry.path();
        let dst = to.join(entry.file_name());
        if fs::rename(&src, &dst).is_err() {
            copy_entry(&src, &dst)?;
            remove_entry(&src)?;
        }
    }
    Ok(())
}

/// Recursively copies every entry of `from` into `to`.
pub(crate) fn copy_contents(from: &Path, to: &Path) -> io::Result<()> {
    copy_contents_excluding(from, to, &[])
}

fn copy_contents_excluding(from: &Path, to: &Path, exclude: &[&str]) -> io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        if exclude
            .iter()
            .any(|name| entry.file_name().to_str() == Some(name))
        {
            continue;
        }
        copy_entry(&entry.path(), &to.join(entry.file_name()))?;
    }
    Ok(())
}

fn copy_entry(src: &Path, dst: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(src)?;
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        let target = fs::read_link(src)?;
        std::os::unix::fs::symlink(&target, dst)?;
    } else if file_type.is_dir() {
        fs::create_dir_all(dst)?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            copy_entry(&entry.path(), &dst.join(entry.file_name()))?;
        }
        fs::set_permissions(dst, metadata.permissions())?;
    } else {
        fs::copy(src, dst)?;
    }
    Ok(())
}

fn remove_entry(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

/// Removes every entry inside `dir`, leaving the directory in place.
pub(crate) fn remove_contents(dir: &Path) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        remove_entry(&entry?.path())?;
    }
    Ok(())
}

pub(crate) fn sparsebundle_paths(workspace_dir: &Path) -> (PathBuf, PathBuf) {
    (
        workspace_dir.join("workspace.sparsebundle"),
        workspace_dir.join("mnt"),
    )
}

pub(crate) fn hdiutil_create_args(bundle: &Path, id: &str) -> Vec<String> {
    vec![
        "create".into(),
        "-type".into(),
        "SPARSEBUNDLE".into(),
        "-fs".into(),
        "APFS".into(),
        "-size".into(),
        "64g".into(),
        "-volname".into(),
        format!("silo-{id}"),
        bundle.display().to_string(),
    ]
}

pub(crate) fn hdiutil_attach_args(bundle: &Path, mountpoint: &Path) -> Vec<String> {
    vec![
        "attach".into(),
        "-nobrowse".into(),
        "-mountpoint".into(),
        mountpoint.display().to_string(),
        bundle.display().to_string(),
    ]
}

pub(crate) fn hdiutil_detach_args(mountpoint: &Path) -> Vec<String> {
    vec!["detach".into(), mountpoint.display().to_string()]
}

/// True when `path` is a live mountpoint: it is a directory whose device
/// id differs from its parent's. A missing path is not a mountpoint.
#[cfg(unix)]
pub(crate) fn is_mountpoint(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    if !meta.is_dir() {
        return false;
    }
    let Some(parent) = path.parent() else {
        return true;
    };
    let Ok(parent_meta) = fs::metadata(parent) else {
        return false;
    };
    meta.dev() != parent_meta.dev()
}

#[cfg(not(unix))]
pub(crate) fn is_mountpoint(_path: &Path) -> bool {
    false
}

/// Detaches the image mounted at `mountpoint`. An unmounted mountpoint is
/// a no-op. A failed detach is retried once with `-force` after a short
/// wait; a second failure is returned.
pub(crate) fn detach_image(mountpoint: &Path) -> Result<(), WorkspaceError> {
    if !is_mountpoint(mountpoint) {
        return Ok(());
    }
    if run_hdiutil(&hdiutil_detach_args(mountpoint)).is_ok() {
        return Ok(());
    }
    std::thread::sleep(Duration::from_millis(500));
    let mut force_args = hdiutil_detach_args(mountpoint);
    force_args.push("-force".into());
    run_hdiutil(&force_args).map_err(|e| {
        WorkspaceError::Setup(format!(
            "cannot detach the workspace image at {} (retried with -force): {e}",
            mountpoint.display()
        ))
    })
}

pub(crate) fn run_hdiutil(args: &[String]) -> Result<(), WorkspaceError> {
    let output = Command::new("hdiutil")
        .args(args)
        .output()
        .map_err(|e| WorkspaceError::Setup(format!("cannot run hdiutil: {e}")))?;
    if !output.status.success() {
        return Err(WorkspaceError::Setup(format!(
            "hdiutil {} failed: {}",
            args.first().map(String::as_str).unwrap_or(""),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

/// Creates the sparse bundle, copies the workspace contents into it, and
/// removes the originals.
pub(crate) fn lock_into_sparsebundle(
    source: &Path,
    workspace_dir: &Path,
    id: &str,
) -> Result<(), WorkspaceError> {
    let (bundle, mountpoint) = sparsebundle_paths(workspace_dir);
    run_hdiutil(&hdiutil_create_args(&bundle, id))?;
    fs::create_dir_all(&mountpoint)?;
    run_hdiutil(&hdiutil_attach_args(&bundle, &mountpoint))?;
    let copy_result = copy_contents(source, &mountpoint);
    let detach_result = run_hdiutil(&hdiutil_detach_args(&mountpoint));
    copy_result?;
    detach_result?;
    remove_contents(source)?;
    Ok(())
}

/// Removes any partial sparse bundle state after a failed lock attempt.
pub(crate) fn cleanup_sparsebundle(workspace_dir: &Path) {
    let (bundle, mountpoint) = sparsebundle_paths(workspace_dir);
    let _ = detach_image(&mountpoint);
    let _ = fs::remove_dir_all(&bundle);
    if !is_mountpoint(&mountpoint) {
        let _ = fs::remove_dir_all(&mountpoint);
    }
}

/// Mounts the sparse bundle (a no-op when it is already mounted), copies
/// the contents back out, and unmounts. Both a copy failure and a detach
/// failure are returned.
pub(crate) fn restore_from_sparsebundle(
    workspace_dir: &Path,
    dest: &Path,
) -> Result<(), WorkspaceError> {
    let (bundle, mountpoint) = sparsebundle_paths(workspace_dir);
    fs::create_dir_all(&mountpoint)?;
    if !is_mountpoint(&mountpoint) {
        run_hdiutil(&hdiutil_attach_args(&bundle, &mountpoint))?;
    }
    let copy_result = copy_contents_excluding(&mountpoint, dest, VOLUME_METADATA);
    let detach_result = detach_image(&mountpoint);
    copy_result?;
    detach_result?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn hdiutil_create_arguments() {
        let args = hdiutil_create_args(Path::new("/state/ws/workspace.sparsebundle"), "abc123");
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        assert_eq!(
            args,
            vec![
                "create",
                "-type",
                "SPARSEBUNDLE",
                "-fs",
                "APFS",
                "-size",
                "64g",
                "-volname",
                "silo-abc123",
                "/state/ws/workspace.sparsebundle",
            ]
        );
    }

    #[test]
    fn hdiutil_attach_arguments() {
        let args = hdiutil_attach_args(
            Path::new("/state/ws/workspace.sparsebundle"),
            Path::new("/state/ws/mnt"),
        );
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        assert_eq!(
            args,
            vec![
                "attach",
                "-nobrowse",
                "-mountpoint",
                "/state/ws/mnt",
                "/state/ws/workspace.sparsebundle",
            ]
        );
    }

    #[test]
    fn hdiutil_detach_arguments() {
        let args = hdiutil_detach_args(Path::new("/state/ws/mnt"));
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        assert_eq!(args, vec!["detach", "/state/ws/mnt"]);
    }

    #[test]
    fn sparsebundle_paths_are_under_the_workspace_dir() {
        let (bundle, mountpoint) = sparsebundle_paths(Path::new("/state/ws"));
        assert_eq!(bundle, Path::new("/state/ws/workspace.sparsebundle"));
        assert_eq!(mountpoint, Path::new("/state/ws/mnt"));
    }

    #[test]
    fn move_contents_moves_everything() {
        let temp = tempfile::tempdir().unwrap();
        let from = temp.path().join("from");
        let to = temp.path().join("to");
        fs::create_dir_all(from.join("nested/deep")).unwrap();
        fs::write(from.join("nested/deep/file.txt"), "data\n").unwrap();
        fs::write(from.join("top.txt"), "top\n").unwrap();
        std::os::unix::fs::symlink("top.txt", from.join("link")).unwrap();

        move_contents(&from, &to).unwrap();

        assert_eq!(fs::read_dir(&from).unwrap().count(), 0);
        assert_eq!(
            fs::read_to_string(to.join("nested/deep/file.txt")).unwrap(),
            "data\n"
        );
        assert_eq!(fs::read_to_string(to.join("top.txt")).unwrap(), "top\n");
        assert_eq!(
            fs::read_link(to.join("link")).unwrap(),
            Path::new("top.txt")
        );
    }

    #[test]
    fn copy_then_remove_contents() {
        let temp = tempfile::tempdir().unwrap();
        let from = temp.path().join("from");
        let to = temp.path().join("to");
        fs::create_dir_all(from.join("sub")).unwrap();
        fs::write(from.join("sub/a.txt"), "a\n").unwrap();
        std::os::unix::fs::symlink("sub/a.txt", from.join("alias")).unwrap();

        copy_contents(&from, &to).unwrap();
        assert_eq!(fs::read_to_string(to.join("sub/a.txt")).unwrap(), "a\n");
        assert_eq!(
            fs::read_link(to.join("alias")).unwrap(),
            Path::new("sub/a.txt")
        );
        assert_eq!(fs::read_to_string(from.join("sub/a.txt")).unwrap(), "a\n");

        remove_contents(&from).unwrap();
        assert_eq!(fs::read_dir(&from).unwrap().count(), 0);
    }

    #[test]
    fn copy_excluding_skips_volume_metadata_names() {
        let temp = tempfile::tempdir().unwrap();
        let from = temp.path().join("from");
        let to = temp.path().join("to");
        fs::create_dir_all(from.join(".fseventsd")).unwrap();
        fs::write(from.join("real.txt"), "x\n").unwrap();

        copy_contents_excluding(&from, &to, VOLUME_METADATA).unwrap();
        assert!(to.join("real.txt").is_file());
        assert!(!to.join(".fseventsd").exists());
    }

    #[test]
    fn marker_and_permission_helpers() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("ws");
        fs::create_dir_all(&dir).unwrap();

        write_marker(&dir).unwrap();
        let contents = fs::read_to_string(dir.join(MARKER_FILE_NAME)).unwrap();
        assert!(contents.contains("locked llmdevsilo workspace"));
        assert!(contents.contains("silo workspace unlock"));

        make_dir_readonly(&dir).unwrap();
        let mode = fs::metadata(&dir).unwrap().permissions().mode();
        assert_eq!(mode & 0o222, 0);

        make_dir_writable(&dir).unwrap();
        let mode = fs::metadata(&dir).unwrap().permissions().mode();
        assert_ne!(mode & 0o200, 0);

        remove_marker(&dir);
        assert!(!dir.join(MARKER_FILE_NAME).exists());
    }

    #[test]
    fn mountpoint_probe_distinguishes_mounts_from_plain_dirs() {
        let temp = tempfile::tempdir().unwrap();
        assert!(!is_mountpoint(temp.path()));
        assert!(!is_mountpoint(&temp.path().join("missing")));
        let file = temp.path().join("file");
        fs::write(&file, "x").unwrap();
        assert!(!is_mountpoint(&file));
        assert!(is_mountpoint(Path::new("/")));
    }

    #[test]
    fn detaching_an_unmounted_mountpoint_is_a_no_op() {
        let temp = tempfile::tempdir().unwrap();
        detach_image(temp.path()).unwrap();
        detach_image(&temp.path().join("missing")).unwrap();
    }

    #[test]
    fn strategy_warnings_mention_their_caveat() {
        assert!(ContainerStrategy::PlainDir
            .warning()
            .contains("file permissions only"));
        assert!(ContainerStrategy::SparseBundle
            .warning()
            .contains("host-visible"));
    }
}
