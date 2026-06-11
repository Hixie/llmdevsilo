//! End-to-end proof that a sandboxed user shell can inspect a workspace
//! while it is attached to a harness: the workspace is locked and attached
//! as the primary (simulating the harness), a shared attachment is taken
//! for the shell, and a real sandbox-exec shell writes a file through the
//! shared mount that is immediately visible at the primary's mount path.
//!
//! The sandbox-exec test is `#[ignore]`d because it needs macOS with a
//! working `/usr/bin/sandbox-exec`. Run it with:
//!
//! ```text
//! cargo test -p silo-harness --test shared_shell -- --ignored
//! ```
#![cfg(target_os = "macos")]

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use silo_core::clock::FakeClock;
use silo_core::config::{SandboxConfig, SandboxKind};
use silo_core::journal::JournalHandle;
use silo_workspace::{ContainerStrategy, WorkspaceManager};

/// Resolves (building if necessary) the silo-helper binary and exports it
/// via `SILO_HELPER_BIN` so the backend finds it regardless of where the
/// test binary lives.
fn ensure_helper() {
    static HELPER: OnceLock<PathBuf> = OnceLock::new();
    let path = HELPER.get_or_init(|| {
        if let Ok(path) = std::env::var("SILO_HELPER_BIN") {
            if !path.is_empty() {
                return PathBuf::from(path);
            }
        }
        // Test binaries live in target/<profile>/deps/.
        let exe = std::env::current_exe().expect("current_exe");
        let profile_dir = exe
            .parent()
            .and_then(|deps| deps.parent())
            .expect("target profile dir")
            .to_path_buf();
        let candidate = profile_dir.join("silo-helper");
        if !candidate.is_file() {
            let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
            let status = std::process::Command::new(cargo)
                .args(["build", "-p", "silo-helper"])
                .status()
                .expect("spawn cargo build -p silo-helper");
            assert!(status.success(), "building silo-helper failed");
        }
        assert!(candidate.is_file(), "missing {}", candidate.display());
        candidate
    });
    std::env::set_var("SILO_HELPER_BIN", path);
}

#[tokio::test]
#[ignore = "requires a working /usr/bin/sandbox-exec"]
async fn shared_shell_writes_are_visible_at_the_primary_mount() {
    ensure_helper();
    let temp = tempfile::tempdir().expect("tempdir");
    let state = temp.path().join("state");
    let ws = temp.path().join("project");
    std::fs::create_dir_all(&ws).expect("create workspace");
    std::fs::write(ws.join("existing.txt"), "already here\n").expect("seed workspace");

    let manager = WorkspaceManager::with_strategy(state, ContainerStrategy::PlainDir);
    manager.lock(&ws).expect("lock");

    // The primary attachment simulates the running harness.
    let primary = manager.attach(&ws, "harness-under-test").expect("attach");
    let shared = manager.attach_shared(&ws).expect("attach_shared");
    assert_eq!(shared.mount_path, primary.mount_path);

    // A real sandbox-exec shell over the shared mount.
    let config = SandboxConfig {
        kind: SandboxKind::MacosSandboxExec,
        workspace_mount: Some(shared.mount_path.clone()),
        ..SandboxConfig::default()
    };
    let proxy = silo_core::traits::ProxyHandle {
        http_addr: "127.0.0.1:3128".parse().expect("loopback address"),
        ca_cert_pem: "-----BEGIN CERTIFICATE-----\nFAKE\n-----END CERTIFICATE-----\n".into(),
        dns_addr: None,
    };
    let journal = JournalHandle::disabled(Arc::new(FakeClock::default()));
    let mut sandbox = silo_sandbox::create_sandbox(&config, Some(proxy), None, journal)
        .await
        .expect("create sandbox");
    sandbox.start().await.expect("start sandbox");

    let code = sandbox
        .user_shell(Some(vec![
            "/bin/sh".into(),
            "-c".into(),
            "cat existing.txt && echo from-the-shell > shell-wrote.txt".into(),
        ]))
        .await
        .expect("user_shell");
    assert_eq!(code, 0, "user shell exited with {code}");

    // The write is visible at the primary's mount path immediately.
    let through_primary = std::fs::read_to_string(primary.mount_path.join("shell-wrote.txt"))
        .expect("read through the primary mount");
    assert_eq!(through_primary, "from-the-shell\n");

    sandbox.shutdown().await.expect("shutdown");
    shared.detach();
    primary.detach();

    // The workspace is still locked and intact for the harness.
    let status = manager.status(&ws).expect("status");
    assert!(status.locked);
    assert_eq!(status.live_shells, 0);
}
