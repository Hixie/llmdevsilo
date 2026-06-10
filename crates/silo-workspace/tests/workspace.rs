//! Integration tests for the workspace lifecycle using the plain-directory
//! strategy.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use silo_core::error::WorkspaceError;
use silo_workspace::{ChangeKind, ContainerStrategy, WorkspaceManager, MARKER_FILE_NAME};

fn manager(state: &Path) -> WorkspaceManager {
    WorkspaceManager::with_strategy(state.to_path_buf(), ContainerStrategy::PlainDir)
}

fn dir_entry_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Rewrites the registry entry for `workspace` to record an attachment
/// with the given pid, exercising the documented on-disk format.
fn set_attachment(state: &Path, workspace: &Path, harness_id: &str, pid: u32) {
    let registry_path = state.join("workspaces").join("registry.json");
    let text = fs::read_to_string(&registry_path).unwrap();
    let mut value: serde_json::Value = serde_json::from_str(&text).unwrap();
    let key = fs::canonicalize(workspace).unwrap();
    let key = key.to_str().unwrap();
    assert!(value.get(key).is_some(), "no registry entry for {key}");
    value[key]["attached"] = serde_json::json!({ "harness_id": harness_id, "pid": pid });
    fs::write(
        &registry_path,
        serde_json::to_string_pretty(&value).unwrap(),
    )
    .unwrap();
}

#[test]
fn full_lock_mutate_unlock_cycle() {
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("state");
    let ws = temp.path().join("project");

    fs::create_dir_all(ws.join("notes")).unwrap();
    fs::write(
        ws.join("notes/edit.txt"),
        "line one\nline two\nline three\n",
    )
    .unwrap();
    fs::write(ws.join("deleted.txt"), "goodbye\n").unwrap();
    fs::write(ws.join("bin.dat"), [0u8, 1, 2, b'b', b'i', b'n', 0]).unwrap();
    fs::write(ws.join("package.json"), "{\n  \"name\": \"demo\"\n}\n").unwrap();
    std::os::unix::fs::symlink("notes/edit.txt", ws.join("link")).unwrap();

    let mgr = manager(&state);
    let status = mgr.lock(&ws).unwrap();
    assert!(status.locked);
    assert!(status
        .warnings
        .iter()
        .any(|w| w.contains("file permissions only")));

    // The original directory holds only the marker and is not writable.
    assert_eq!(dir_entry_names(&ws), vec![MARKER_FILE_NAME.to_string()]);
    let mode = fs::metadata(&ws).unwrap().permissions().mode();
    assert_eq!(mode & 0o222, 0);

    // The contents moved into the state directory; mutate them there.
    let attached = mgr.attach(&ws, "h-1").unwrap();
    let data = attached.mount_path.clone();
    assert!(data.join("notes/edit.txt").is_file());
    assert!(data.join("bin.dat").is_file());
    assert_eq!(
        mgr.status(&ws).unwrap().attached_harness.as_deref(),
        Some("h-1")
    );

    fs::write(
        data.join("notes/edit.txt"),
        "line one\nline 2\nline three\n",
    )
    .unwrap();
    fs::remove_file(data.join("deleted.txt")).unwrap();
    fs::create_dir_all(data.join(".git/hooks")).unwrap();
    fs::write(
        data.join(".git/hooks/post-checkout"),
        "#!/bin/sh\necho pwned\n",
    )
    .unwrap();
    fs::write(
        data.join("package.json"),
        "{\n  \"name\": \"demo\",\n  \"scripts\": { \"postinstall\": \"evil\" }\n}\n",
    )
    .unwrap();
    fs::write(data.join("bin.dat"), [0u8, 0xff, 0xfe]).unwrap();
    fs::remove_file(data.join("link")).unwrap();
    std::os::unix::fs::symlink("deleted.txt", data.join("link")).unwrap();
    attached.detach();

    let report = mgr.unlock(&ws).unwrap();

    // Exactly the right changes, in path order.
    let summary: Vec<(String, ChangeKind, bool)> = report
        .changes
        .iter()
        .map(|c| (c.path.clone(), c.kind, c.diff.is_some()))
        .collect();
    assert_eq!(
        summary,
        vec![
            (".git".into(), ChangeKind::Added, false),
            (".git/hooks".into(), ChangeKind::Added, false),
            (".git/hooks/post-checkout".into(), ChangeKind::Added, true),
            ("bin.dat".into(), ChangeKind::Modified, false),
            ("deleted.txt".into(), ChangeKind::Deleted, true),
            ("link".into(), ChangeKind::Modified, false),
            ("notes/edit.txt".into(), ChangeKind::Modified, true),
            ("package.json".into(), ChangeKind::Modified, true),
        ]
    );

    let edit = report
        .changes
        .iter()
        .find(|c| c.path == "notes/edit.txt")
        .unwrap();
    let diff = edit.diff.as_ref().unwrap();
    assert!(
        diff.starts_with("--- locked\n+++ current\n"),
        "diff was: {diff}"
    );
    assert!(diff.contains("@@ -1,3 +1,3 @@"));
    assert!(diff.contains(" line one\n"));
    assert!(diff.contains("-line two\n"));
    assert!(diff.contains("+line 2\n"));
    assert!(diff.contains(" line three\n"));

    let deleted = report
        .changes
        .iter()
        .find(|c| c.path == "deleted.txt")
        .unwrap();
    assert!(deleted.diff.as_ref().unwrap().contains("-goodbye\n"));

    let hook = report
        .changes
        .iter()
        .find(|c| c.path == ".git/hooks/post-checkout")
        .unwrap();
    assert!(hook.diff.as_ref().unwrap().contains("+echo pwned\n"));

    // Auto-exec flags fire for the hook and package.json.
    let flags: Vec<(&str, &str)> = report
        .auto_exec_flags
        .iter()
        .map(|f| (f.path.as_str(), f.reason.as_str()))
        .collect();
    assert_eq!(flags.len(), 2, "flags were: {flags:?}");
    assert_eq!(flags[0].0, ".git/hooks/post-checkout");
    assert!(flags[0].1.contains("git hook"));
    assert_eq!(flags[1].0, "package.json");
    assert!(flags[1].1.contains("lifecycle"));

    // The original directory is fully restored with the mutations.
    assert_eq!(
        fs::read_to_string(ws.join("notes/edit.txt")).unwrap(),
        "line one\nline 2\nline three\n"
    );
    assert!(!ws.join("deleted.txt").exists());
    assert_eq!(fs::read(ws.join("bin.dat")).unwrap(), vec![0u8, 0xff, 0xfe]);
    assert_eq!(
        fs::read_link(ws.join("link")).unwrap(),
        PathBuf::from("deleted.txt")
    );
    assert!(ws.join(".git/hooks/post-checkout").is_file());
    assert!(!ws.join(MARKER_FILE_NAME).exists());
    let mode = fs::metadata(&ws).unwrap().permissions().mode();
    assert_ne!(mode & 0o200, 0);

    // The registry entry and the snapshot are gone.
    assert!(!mgr.status(&ws).unwrap().locked);
    assert_eq!(
        dir_entry_names(&state.join("workspaces")),
        vec!["registry.json".to_string()]
    );
}

#[test]
fn mode_only_change_is_reported_with_a_note_and_no_diff() {
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("state");
    let ws = temp.path().join("ws");
    fs::create_dir_all(&ws).unwrap();
    fs::write(ws.join("script.sh"), "#!/bin/sh\necho hi\n").unwrap();
    fs::set_permissions(ws.join("script.sh"), fs::Permissions::from_mode(0o644)).unwrap();

    let mgr = manager(&state);
    mgr.lock(&ws).unwrap();

    // Change only the permissions; the content stays identical.
    let attached = mgr.attach(&ws, "h-mode").unwrap();
    fs::set_permissions(
        attached.mount_path.join("script.sh"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    attached.detach();

    let report = mgr.unlock(&ws).unwrap();
    assert_eq!(report.changes.len(), 1, "changes: {:?}", report.changes);
    let change = &report.changes[0];
    assert_eq!(change.path, "script.sh");
    assert_eq!(change.kind, ChangeKind::Modified);
    assert!(change.diff.is_none(), "diff was: {:?}", change.diff);
    assert_eq!(change.note.as_deref(), Some("mode 0644 -> 0755"));

    // The restored file keeps the new mode and the original content.
    let mode = fs::metadata(ws.join("script.sh"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o7777, 0o755);
    assert_eq!(
        fs::read_to_string(ws.join("script.sh")).unwrap(),
        "#!/bin/sh\necho hi\n"
    );
}

#[test]
fn double_lock_fails() {
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("state");
    let ws = temp.path().join("ws");
    fs::create_dir_all(&ws).unwrap();
    fs::write(ws.join("f.txt"), "x\n").unwrap();

    let mgr = manager(&state);
    mgr.lock(&ws).unwrap();
    let second = mgr.lock(&ws);
    assert!(matches!(second, Err(WorkspaceError::Locked(_))));
}

#[test]
fn lock_creates_a_missing_directory() {
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("state");
    let ws = temp.path().join("brand-new");

    let mgr = manager(&state);
    let status = mgr.lock(&ws).unwrap();
    assert!(status.locked);
    assert!(ws.is_dir());
    assert_eq!(dir_entry_names(&ws), vec![MARKER_FILE_NAME.to_string()]);

    let report = mgr.unlock(&ws).unwrap();
    assert!(report.changes.is_empty());
    assert!(report.auto_exec_flags.is_empty());
}

#[test]
fn unlock_of_unlocked_path_fails() {
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("state");
    let ws = temp.path().join("ws");
    fs::create_dir_all(&ws).unwrap();

    let mgr = manager(&state);
    assert!(matches!(mgr.unlock(&ws), Err(WorkspaceError::NotLocked(_))));
}

#[test]
fn attach_of_unlocked_path_fails() {
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("state");
    let ws = temp.path().join("ws");
    fs::create_dir_all(&ws).unwrap();

    let mgr = manager(&state);
    assert!(matches!(
        mgr.attach(&ws, "h-1"),
        Err(WorkspaceError::NotLocked(_))
    ));
}

#[test]
fn attach_twice_fails() {
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("state");
    let ws = temp.path().join("ws");
    fs::create_dir_all(&ws).unwrap();

    let mgr = manager(&state);
    mgr.lock(&ws).unwrap();
    let first = mgr.attach(&ws, "h-1").unwrap();
    let second = mgr.attach(&ws, "h-2");
    assert!(matches!(second, Err(WorkspaceError::Setup(_))));
    first.detach();
}

#[test]
fn detach_clears_the_registry_attachment() {
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("state");
    let ws = temp.path().join("ws");
    fs::create_dir_all(&ws).unwrap();

    let mgr = manager(&state);
    mgr.lock(&ws).unwrap();

    let attached = mgr.attach(&ws, "h-1").unwrap();
    assert_eq!(
        mgr.status(&ws).unwrap().attached_harness.as_deref(),
        Some("h-1")
    );
    attached.detach();
    assert!(mgr.status(&ws).unwrap().attached_harness.is_none());

    // Dropping an attachment also clears it, and re-attaching works.
    {
        let _attached = mgr.attach(&ws, "h-2").unwrap();
        assert_eq!(
            mgr.status(&ws).unwrap().attached_harness.as_deref(),
            Some("h-2")
        );
    }
    assert!(mgr.status(&ws).unwrap().attached_harness.is_none());
}

#[test]
fn unlock_kills_an_attached_harness() {
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("state");
    let ws = temp.path().join("ws");
    fs::create_dir_all(&ws).unwrap();
    fs::write(ws.join("f.txt"), "x\n").unwrap();

    let mgr = manager(&state);
    mgr.lock(&ws).unwrap();

    let mut child = std::process::Command::new("sleep")
        .arg("300")
        .spawn()
        .unwrap();
    let pid = child.id();
    set_attachment(&state, &ws, "h-kill", pid);

    let report = mgr.unlock(&ws).unwrap();
    assert!(report.changes.is_empty());

    // The child no longer exists; unlock reaped it after terminating it.
    let alive = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_ok();
    assert!(!alive);
    let _ = child.try_wait();

    assert!(!mgr.status(&ws).unwrap().locked);
    assert_eq!(fs::read_to_string(ws.join("f.txt")).unwrap(), "x\n");
}

#[test]
fn unlock_with_a_stale_pid_attachment_succeeds() {
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("state");
    let ws = temp.path().join("ws");
    fs::create_dir_all(&ws).unwrap();
    fs::write(ws.join("f.txt"), "x\n").unwrap();

    let mgr = manager(&state);
    mgr.lock(&ws).unwrap();

    // A process that has already exited and been reaped: its pid is stale.
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    set_attachment(&state, &ws, "h-stale", pid);

    let report = mgr.unlock(&ws).unwrap();
    assert!(report.changes.is_empty());
    assert!(!mgr.status(&ws).unwrap().locked);
    assert_eq!(fs::read_to_string(ws.join("f.txt")).unwrap(), "x\n");
}
