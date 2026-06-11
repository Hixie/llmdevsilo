//! Containerization strategies for locked workspaces.
//!
//! `PlainDir` moves the workspace contents into the harness state
//! directory and relies on file permissions. `SparseBundle` copies the
//! contents into a macOS sparse bundle disk image managed with `hdiutil`.
//! `Ext4Fuse` copies the contents into a raw ext4 disk image formatted
//! with `mkfs.ext4` and mounted user-space with `fuse2fs` on Linux. When
//! the image tooling is unavailable or fails, the lock falls back to
//! `PlainDir`.

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
    /// Contents live in a raw ext4 disk image mounted with `fuse2fs`
    /// (Linux).
    Ext4Fuse,
}

impl ContainerStrategy {
    /// The platform default: sparse bundle on macOS; on Linux, an ext4
    /// image when `fuse2fs` and `mkfs.ext4` are on `PATH`; plain
    /// directory otherwise.
    pub fn default_for_platform() -> Self {
        select_for_platform(&tool_on_path).0
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
            ContainerStrategy::Ext4Fuse => {
                "the workspace image is mounted host-visible while a harness is attached; \
                 the image size is fixed at lock time and does not grow"
            }
        }
    }

    /// True when the contents live in a disk image that must be mounted
    /// to be reached.
    pub(crate) fn uses_image(self) -> bool {
        matches!(
            self,
            ContainerStrategy::SparseBundle | ContainerStrategy::Ext4Fuse
        )
    }
}

/// Warning hint recorded when Linux falls back to `PlainDir` because the
/// ext4 image tooling is missing.
const LINUX_TOOLS_HINT: &str =
    "install fuse2fs and mkfs.ext4 (the fuse2fs and e2fsprogs packages) to hold \
     locked workspaces in an ext4 disk image instead";

/// Selects the default strategy for the platform, plus a warning hint
/// when a weaker strategy was chosen because tooling is missing.
/// `have_tool` reports whether a named executable is on `PATH`.
pub(crate) fn select_for_platform(
    have_tool: &dyn Fn(&str) -> bool,
) -> (ContainerStrategy, Option<String>) {
    if cfg!(target_os = "macos") {
        (ContainerStrategy::SparseBundle, None)
    } else if cfg!(target_os = "linux") {
        select_linux_strategy(have_tool)
    } else {
        (ContainerStrategy::PlainDir, None)
    }
}

/// The Linux selection: `Ext4Fuse` when both `fuse2fs` and `mkfs.ext4`
/// are available, `PlainDir` with an install hint otherwise.
pub(crate) fn select_linux_strategy(
    have_tool: &dyn Fn(&str) -> bool,
) -> (ContainerStrategy, Option<String>) {
    if have_tool("fuse2fs") && have_tool("mkfs.ext4") {
        (ContainerStrategy::Ext4Fuse, None)
    } else {
        (
            ContainerStrategy::PlainDir,
            Some(LINUX_TOOLS_HINT.to_string()),
        )
    }
}

/// True when an executable named `name` exists in a `PATH` directory.
pub(crate) fn tool_on_path(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| !dir.as_os_str().is_empty() && dir.join(name).is_file())
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

/// The mountpoint directory used by the image-based strategies.
pub(crate) fn mountpoint_path(workspace_dir: &Path) -> PathBuf {
    workspace_dir.join("mnt")
}

pub(crate) fn sparsebundle_paths(workspace_dir: &Path) -> (PathBuf, PathBuf) {
    (
        workspace_dir.join("workspace.sparsebundle"),
        mountpoint_path(workspace_dir),
    )
}

pub(crate) fn ext4_image_paths(workspace_dir: &Path) -> (PathBuf, PathBuf) {
    (
        workspace_dir.join("workspace.img"),
        mountpoint_path(workspace_dir),
    )
}

/// Size in bytes for a new ext4 workspace image: twice the content size
/// plus 256 MiB of headroom, at least 1 GiB, rounded up to a 4 KiB block
/// boundary. The image does not grow after creation.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn ext4_image_size_bytes(content_size: u64) -> u64 {
    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * MIB;
    const BLOCK: u64 = 4096;
    content_size
        .saturating_mul(2)
        .saturating_add(256 * MIB)
        .max(GIB)
        .div_ceil(BLOCK)
        .saturating_mul(BLOCK)
}

/// Total size in bytes of the regular files under `dir`, recursively.
/// Symlinks are not followed; directory and symlink entries contribute
/// nothing.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn dir_content_size(dir: &Path) -> io::Result<u64> {
    let mut total = 0u64;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_dir() {
            total = total.saturating_add(dir_content_size(&entry.path())?);
        } else if metadata.file_type().is_file() {
            total = total.saturating_add(metadata.len());
        }
    }
    Ok(total)
}

#[cfg(any(target_os = "linux", test))]
pub(crate) fn mkfs_ext4_args(image: &Path) -> Vec<String> {
    vec![
        "-q".into(),
        "-F".into(),
        // A journal buys little inside a per-session image and costs
        // space in a fixed-size file.
        "-O".into(),
        "^has_journal".into(),
        // Root-directory ownership goes to the invoking user, so the
        // user-space mount is writable without privileges.
        "-E".into(),
        "root_owner".into(),
        image.display().to_string(),
    ]
}

#[cfg(any(target_os = "linux", test))]
pub(crate) fn fuse2fs_mount_args(image: &Path, mountpoint: &Path) -> Vec<String> {
    vec![
        image.display().to_string(),
        mountpoint.display().to_string(),
    ]
}

#[cfg(any(target_os = "linux", test))]
pub(crate) fn fusermount_unmount_args(mountpoint: &Path, lazy: bool) -> Vec<String> {
    let mut args = vec!["-u".to_string()];
    if lazy {
        args.push("-z".into());
    }
    args.push(mountpoint.display().to_string());
    args
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
    run_tool("hdiutil", args)
}

/// Runs `program` with `args`, mapping a spawn failure or a non-zero
/// exit to a setup error.
pub(crate) fn run_tool(program: &str, args: &[String]) -> Result<(), WorkspaceError> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|e| WorkspaceError::Setup(format!("cannot run {program}: {e}")))?;
    if !output.status.success() {
        return Err(WorkspaceError::Setup(format!(
            "{program} {} failed: {}",
            args.first().map(String::as_str).unwrap_or(""),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

/// Releases the image mount at `mountpoint` for an image-based strategy;
/// a no-op for `PlainDir`.
pub(crate) fn release_mount(
    strategy: ContainerStrategy,
    mountpoint: &Path,
) -> Result<(), WorkspaceError> {
    match strategy {
        ContainerStrategy::PlainDir => Ok(()),
        ContainerStrategy::SparseBundle => detach_image(mountpoint),
        ContainerStrategy::Ext4Fuse => unmount_fuse(mountpoint),
    }
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

/// Entry `mkfs.ext4` creates at the filesystem root; it is not part of
/// the workspace contents.
#[cfg(target_os = "linux")]
const EXT4_VOLUME_METADATA: &[&str] = &["lost+found"];

/// Removes any partial ext4 image state after a failed lock attempt.
pub(crate) fn cleanup_ext4_image(workspace_dir: &Path) {
    let (image, mountpoint) = ext4_image_paths(workspace_dir);
    let _ = unmount_fuse(&mountpoint);
    let _ = fs::remove_file(&image);
    if !is_mountpoint(&mountpoint) {
        let _ = fs::remove_dir_all(&mountpoint);
    }
}

pub(crate) use ext4_runtime::{
    lock_into_ext4_image, mount_ext4_image, restore_from_ext4_image, unmount_fuse,
};

/// Runtime invocation of the ext4 image tooling. The argv builders,
/// sizing, and selection logic above are cross-platform; only the code
/// that runs `mkfs.ext4`, `fuse2fs`, and `fusermount` is Linux-only.
#[cfg(target_os = "linux")]
mod ext4_runtime {
    use super::*;

    /// Creates the raw image sized from the source contents, formats it
    /// as ext4, mounts it with `fuse2fs`, copies the workspace contents
    /// in, unmounts, and removes the originals.
    pub(crate) fn lock_into_ext4_image(
        source: &Path,
        workspace_dir: &Path,
    ) -> Result<(), WorkspaceError> {
        let (image, mountpoint) = ext4_image_paths(workspace_dir);
        let content_size = dir_content_size(source)?;
        let file = fs::File::create(&image)?;
        file.set_len(ext4_image_size_bytes(content_size))?;
        drop(file);
        run_tool("mkfs.ext4", &mkfs_ext4_args(&image))?;
        fs::create_dir_all(&mountpoint)?;
        mount_ext4_image(&image, &mountpoint)?;
        let _ = fs::remove_dir(mountpoint.join("lost+found"));
        let copy_result = copy_contents(source, &mountpoint);
        let unmount_result = unmount_fuse(&mountpoint);
        copy_result?;
        unmount_result?;
        remove_contents(source)?;
        Ok(())
    }

    pub(crate) fn mount_ext4_image(image: &Path, mountpoint: &Path) -> Result<(), WorkspaceError> {
        run_tool("fuse2fs", &fuse2fs_mount_args(image, mountpoint))
    }

    /// Mounts the image (a no-op when it is already mounted), copies the
    /// contents back out, and unmounts. Both a copy failure and an
    /// unmount failure are returned.
    pub(crate) fn restore_from_ext4_image(
        workspace_dir: &Path,
        dest: &Path,
    ) -> Result<(), WorkspaceError> {
        let (image, mountpoint) = ext4_image_paths(workspace_dir);
        fs::create_dir_all(&mountpoint)?;
        if !is_mountpoint(&mountpoint) {
            mount_ext4_image(&image, &mountpoint)?;
        }
        let copy_result = copy_contents_excluding(&mountpoint, dest, EXT4_VOLUME_METADATA);
        let unmount_result = unmount_fuse(&mountpoint);
        copy_result?;
        unmount_result?;
        Ok(())
    }

    /// Unmounts the FUSE mount at `mountpoint`. An unmounted mountpoint
    /// is a no-op. A failed unmount is retried lazily (`-z`) after a
    /// short wait; a second failure is returned.
    pub(crate) fn unmount_fuse(mountpoint: &Path) -> Result<(), WorkspaceError> {
        if !is_mountpoint(mountpoint) {
            return Ok(());
        }
        if run_fusermount(&fusermount_unmount_args(mountpoint, false)).is_ok() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(500));
        run_fusermount(&fusermount_unmount_args(mountpoint, true)).map_err(|e| {
            WorkspaceError::Setup(format!(
                "cannot unmount the workspace image at {} (retried with a lazy unmount): {e}",
                mountpoint.display()
            ))
        })
    }

    /// Runs `fusermount` with the given arguments, falling back to
    /// `fusermount3` when `fusermount` is missing or fails.
    fn run_fusermount(args: &[String]) -> Result<(), WorkspaceError> {
        match run_tool("fusermount", args) {
            Ok(()) => Ok(()),
            Err(first) => run_tool("fusermount3", args)
                .map_err(|second| WorkspaceError::Setup(format!("{first}; {second}"))),
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod ext4_runtime {
    use super::*;

    pub(crate) fn lock_into_ext4_image(
        _source: &Path,
        _workspace_dir: &Path,
    ) -> Result<(), WorkspaceError> {
        Err(not_linux())
    }

    pub(crate) fn mount_ext4_image(
        _image: &Path,
        _mountpoint: &Path,
    ) -> Result<(), WorkspaceError> {
        Err(not_linux())
    }

    pub(crate) fn restore_from_ext4_image(
        _workspace_dir: &Path,
        _dest: &Path,
    ) -> Result<(), WorkspaceError> {
        Err(not_linux())
    }

    pub(crate) fn unmount_fuse(mountpoint: &Path) -> Result<(), WorkspaceError> {
        if is_mountpoint(mountpoint) {
            return Err(not_linux());
        }
        Ok(())
    }

    fn not_linux() -> WorkspaceError {
        WorkspaceError::Setup("the ext4 image strategy requires Linux".into())
    }
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
        assert!(ContainerStrategy::Ext4Fuse
            .warning()
            .contains("host-visible"));
        assert!(ContainerStrategy::Ext4Fuse
            .warning()
            .contains("fixed at lock time"));
    }

    #[test]
    fn strategy_serde_names_are_stable() {
        assert_eq!(
            serde_json::to_string(&ContainerStrategy::PlainDir).unwrap(),
            "\"plain_dir\""
        );
        assert_eq!(
            serde_json::to_string(&ContainerStrategy::SparseBundle).unwrap(),
            "\"sparse_bundle\""
        );
        assert_eq!(
            serde_json::to_string(&ContainerStrategy::Ext4Fuse).unwrap(),
            "\"ext4_fuse\""
        );
        let parsed: ContainerStrategy = serde_json::from_str("\"ext4_fuse\"").unwrap();
        assert_eq!(parsed, ContainerStrategy::Ext4Fuse);
    }

    #[test]
    fn only_the_image_strategies_use_an_image() {
        assert!(!ContainerStrategy::PlainDir.uses_image());
        assert!(ContainerStrategy::SparseBundle.uses_image());
        assert!(ContainerStrategy::Ext4Fuse.uses_image());
    }

    #[test]
    fn ext4_image_paths_are_under_the_workspace_dir() {
        let (image, mountpoint) = ext4_image_paths(Path::new("/state/ws"));
        assert_eq!(image, Path::new("/state/ws/workspace.img"));
        assert_eq!(mountpoint, Path::new("/state/ws/mnt"));
        assert_eq!(mountpoint, mountpoint_path(Path::new("/state/ws")));
    }

    #[test]
    fn mkfs_ext4_arguments() {
        let args = mkfs_ext4_args(Path::new("/state/ws/workspace.img"));
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        assert_eq!(
            args,
            vec![
                "-q",
                "-F",
                "-O",
                "^has_journal",
                "-E",
                "root_owner",
                "/state/ws/workspace.img",
            ]
        );
    }

    #[test]
    fn fuse2fs_mount_arguments() {
        let args = fuse2fs_mount_args(
            Path::new("/state/ws/workspace.img"),
            Path::new("/state/ws/mnt"),
        );
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        assert_eq!(args, vec!["/state/ws/workspace.img", "/state/ws/mnt"]);
    }

    #[test]
    fn fusermount_unmount_arguments() {
        let plain = fusermount_unmount_args(Path::new("/state/ws/mnt"), false);
        let plain: Vec<&str> = plain.iter().map(String::as_str).collect();
        assert_eq!(plain, vec!["-u", "/state/ws/mnt"]);

        let lazy = fusermount_unmount_args(Path::new("/state/ws/mnt"), true);
        let lazy: Vec<&str> = lazy.iter().map(String::as_str).collect();
        assert_eq!(lazy, vec!["-u", "-z", "/state/ws/mnt"]);
    }

    #[test]
    fn ext4_image_sizing_applies_headroom_minimum_and_rounding() {
        const MIB: u64 = 1024 * 1024;
        const GIB: u64 = 1024 * MIB;

        // Small contents hit the 1 GiB minimum.
        assert_eq!(ext4_image_size_bytes(0), GIB);
        assert_eq!(ext4_image_size_bytes(100 * MIB), GIB);

        // Above the minimum: twice the content size plus 256 MiB.
        assert_eq!(ext4_image_size_bytes(GIB), 2 * GIB + 256 * MIB);
        assert_eq!(ext4_image_size_bytes(10 * GIB), 20 * GIB + 256 * MIB);

        // The result is a multiple of the 4 KiB block size.
        let odd = ext4_image_size_bytes(GIB + 1);
        assert_eq!(odd % 4096, 0);
        assert!(odd >= 2 * GIB + 256 * MIB + 2);

        // Absurd input saturates instead of overflowing.
        assert!(ext4_image_size_bytes(u64::MAX) >= u64::MAX - 4096);
    }

    #[test]
    fn dir_content_size_counts_regular_files_only() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("ws");
        fs::create_dir_all(dir.join("nested/deep")).unwrap();
        fs::write(dir.join("top.bin"), vec![0u8; 1000]).unwrap();
        fs::write(dir.join("nested/deep/file.bin"), vec![0u8; 234]).unwrap();
        std::os::unix::fs::symlink("top.bin", dir.join("link")).unwrap();

        assert_eq!(dir_content_size(&dir).unwrap(), 1234);
    }

    #[test]
    fn linux_selection_follows_the_probe() {
        let both = |_: &str| true;
        let (strategy, hint) = select_linux_strategy(&both);
        assert_eq!(strategy, ContainerStrategy::Ext4Fuse);
        assert!(hint.is_none());

        let neither = |_: &str| false;
        let (strategy, hint) = select_linux_strategy(&neither);
        assert_eq!(strategy, ContainerStrategy::PlainDir);
        let hint = hint.unwrap();
        assert!(hint.contains("fuse2fs"));
        assert!(hint.contains("e2fsprogs"));

        let only_mkfs = |name: &str| name == "mkfs.ext4";
        assert_eq!(
            select_linux_strategy(&only_mkfs).0,
            ContainerStrategy::PlainDir
        );
        let only_fuse = |name: &str| name == "fuse2fs";
        assert_eq!(
            select_linux_strategy(&only_fuse).0,
            ContainerStrategy::PlainDir
        );
    }

    #[test]
    fn platform_selection_uses_the_probe_only_on_linux() {
        let both = |_: &str| true;
        let neither = |_: &str| false;
        if cfg!(target_os = "macos") {
            assert_eq!(
                select_for_platform(&both),
                (ContainerStrategy::SparseBundle, None)
            );
            assert_eq!(
                select_for_platform(&neither),
                (ContainerStrategy::SparseBundle, None)
            );
        } else if cfg!(target_os = "linux") {
            assert_eq!(
                select_for_platform(&both),
                (ContainerStrategy::Ext4Fuse, None)
            );
            let (strategy, hint) = select_for_platform(&neither);
            assert_eq!(strategy, ContainerStrategy::PlainDir);
            assert!(hint.is_some());
        }
    }

    #[test]
    fn tool_probe_finds_executables_on_path() {
        // Every supported platform has `ls` on the default PATH in CI and
        // development environments.
        assert!(tool_on_path("ls"));
        assert!(!tool_on_path("definitely-not-a-real-tool-name"));
    }
}
