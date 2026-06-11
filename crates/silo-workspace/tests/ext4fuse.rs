//! Real `mkfs.ext4` plus `fuse2fs` lifecycle test for the Ext4Fuse
//! strategy: lock, shared mounts, mount handoff between attachments, and
//! an unlock that terminates a live registered shell process.
//!
//! `#[ignore]`d because it needs Linux with `mkfs.ext4`, `fuse2fs`, and
//! `fusermount` (or `fusermount3`) installed. Run it with:
//!
//! ```text
//! cargo test -p silo-workspace --test ext4fuse -- --ignored
//! ```

#![cfg(target_os = "linux")]

use std::fs;
use std::os::unix::process::CommandExt;
use std::path::Path;

use silo_workspace::{ChangeKind, ContainerStrategy, WorkspaceManager};

/// True when `path` appears as a mountpoint in the mount table.
fn is_mounted(path: &Path) -> bool {
    let table = fs::read_to_string("/proc/self/mounts").expect("read mounts");
    let needle = format!(" {} ", path.display());
    table.lines().any(|line| line.contains(&needle))
}

/// Records a secondary (shell) attachment for `workspace` directly in the
/// registry, the way a `silo shell` process registers itself.
fn add_secondary_attachment(state: &Path, workspace: &Path, pid: u32, start_hint: &str) {
    let registry_path = state.join("workspaces").join("registry.json");
    let text = fs::read_to_string(&registry_path).unwrap();
    let mut value: serde_json::Value = serde_json::from_str(&text).unwrap();
    let key = fs::canonicalize(workspace).unwrap();
    let key = key.to_str().unwrap();
    assert!(value.get(key).is_some(), "no registry entry for {key}");
    let mut list = value[key]["secondary_attachments"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    list.push(serde_json::json!({
        "purpose": "shell",
        "pid": pid,
        "start_hint": start_hint,
        "token": "test-shell-token",
    }));
    value[key]["secondary_attachments"] = serde_json::Value::Array(list);
    fs::write(
        &registry_path,
        serde_json::to_string_pretty(&value).unwrap(),
    )
    .unwrap();
}

/// Start-time hint for a pid, matching what attachments record.
fn start_hint(pid: u32) -> String {
    let output = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "lstart="])
        .output()
        .expect("run ps");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

#[test]
#[ignore = "requires mkfs.ext4 and fuse2fs"]
fn ext4_image_lifecycle_with_shared_mounts_and_unlock() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = fs::canonicalize(temp.path()).expect("canonicalize tempdir");
    let state = root.join("state");
    let ws = root.join("project");
    fs::create_dir_all(&ws).expect("create workspace");
    fs::write(ws.join("seed.txt"), "seeded\n").expect("seed workspace");

    let mgr = WorkspaceManager::with_strategy(state.clone(), ContainerStrategy::Ext4Fuse);
    let status = mgr.lock(&ws).expect("lock");
    assert!(status.locked);
    assert!(
        !status
            .warnings
            .iter()
            .any(|w| w.contains("ext4 image setup failed")),
        "lock fell back to the plain-directory strategy: {:?}",
        status.warnings
    );

    // The image exists, sized at the 1 GiB minimum for tiny contents.
    let image = state
        .join("workspaces")
        .read_dir()
        .expect("read workspaces dir")
        .filter_map(Result::ok)
        .map(|entry| entry.path().join("workspace.img"))
        .find(|p| p.exists())
        .expect("workspace.img exists");
    assert_eq!(
        fs::metadata(&image).expect("image metadata").len(),
        1024 * 1024 * 1024
    );

    // Primary plus shared attachment over the same image mount.
    let primary = mgr.attach(&ws, "h-ext4").expect("attach primary");
    assert!(is_mounted(&primary.mount_path), "image is not mounted");
    let shared = mgr.attach_shared(&ws).expect("attach_shared");
    assert_eq!(shared.mount_path, primary.mount_path);
    assert_eq!(mgr.status(&ws).expect("status").live_shells, 1);

    // A write through the shared mount is visible at the primary mount.
    fs::write(shared.mount_path.join("from-shell.txt"), "hello\n").expect("write via shared");
    assert_eq!(
        fs::read_to_string(primary.mount_path.join("from-shell.txt")).expect("read via primary"),
        "hello\n"
    );

    // Primary detach keeps the mount while the secondary lives; the
    // secondary's detach releases it.
    let mount_path = shared.mount_path.clone();
    primary.detach();
    assert!(is_mounted(&mount_path), "primary detach dropped the mount");
    shared.detach();
    assert!(
        !is_mounted(&mount_path),
        "last detach did not release the mount"
    );

    // Unlock with a live registered shell process: a sleep child in its
    // own process group stands in for a `silo shell`.
    let reattached = mgr.attach_shared(&ws).expect("reattach for unlock");
    assert!(is_mounted(&mount_path), "reattach did not mount the image");
    let mut sleeper = std::process::Command::new("sleep");
    sleeper.arg("300");
    sleeper.process_group(0);
    let mut child = sleeper.spawn().expect("spawn sleeper");
    let pid = child.id();
    add_secondary_attachment(&state, &ws, pid, &start_hint(pid));
    assert_eq!(mgr.status(&ws).expect("status").live_shells, 2);
    // The reattached guard stays registered through the unlock; its
    // pid is this test process, which unlock never signals.
    std::mem::forget(reattached);

    let report = mgr.unlock(&ws).expect("unlock");

    // The shell process was terminated and reaped.
    let alive = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_ok();
    assert!(!alive, "unlock left the shell process running");
    let _ = child.try_wait();

    // The image is unmounted and the contents are restored, without the
    // ext4 lost+found entry.
    assert!(!is_mounted(&mount_path), "unlock left the image mounted");
    assert_eq!(fs::read_to_string(ws.join("seed.txt")).unwrap(), "seeded\n");
    assert_eq!(
        fs::read_to_string(ws.join("from-shell.txt")).unwrap(),
        "hello\n"
    );
    assert!(!ws.join("lost+found").exists());

    // The report covers the change made through the shared mount.
    let summary: Vec<(String, ChangeKind)> = report
        .changes
        .iter()
        .map(|c| (c.path.clone(), c.kind))
        .collect();
    assert_eq!(summary, vec![("from-shell.txt".into(), ChangeKind::Added)]);

    let status = mgr.status(&ws).expect("status after unlock");
    assert!(!status.locked);
    assert_eq!(status.live_shells, 0);
}
