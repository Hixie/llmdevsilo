//! Integration tests for the macOS sandbox-exec backend.
//!
//! These run a real sandbox: they spawn `/usr/bin/sandbox-exec` with a
//! generated profile around a real `silo-helper` binary and exercise the
//! tool path end to end, including the deny side of the policy. They are
//! `#[ignore]`d because they need macOS with a working `sandbox-exec`
//! (some CI and nested-sandbox environments refuse it). Run them with:
//!
//! ```text
//! cargo test -p silo-sandbox -- --ignored
//! ```
//!
//! The helper binary is taken from `SILO_HELPER_BIN`, or built with
//! `cargo build -p silo-helper` on first use.
#![cfg(target_os = "macos")]

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use silo_core::clock::FakeClock;
use silo_core::config::{SandboxConfig, SandboxKind};
use silo_core::error::SandboxError;
use silo_core::journal::JournalHandle;
use silo_core::tool::{ToolCall, ToolOutput};
use silo_core::traits::{ProxyHandle, Sandbox};

const CA_PEM: &str = "-----BEGIN CERTIFICATE-----\nFAKE\n-----END CERTIFICATE-----\n";

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

fn proxy_handle() -> ProxyHandle {
    ProxyHandle {
        http_addr: "127.0.0.1:3128".parse().expect("loopback address"),
        ca_cert_pem: CA_PEM.into(),
        dns_addr: None,
    }
}

fn journal() -> JournalHandle {
    JournalHandle::disabled(Arc::new(FakeClock::default()))
}

fn call(name: &str, input: serde_json::Value) -> ToolCall {
    ToolCall {
        id: "t1".into(),
        name: name.into(),
        input,
    }
}

struct Setup {
    sandbox: Box<dyn Sandbox>,
    workspace: tempfile::TempDir,
    allowed: tempfile::TempDir,
    denied: tempfile::TempDir,
}

/// Starts a sandbox around a fresh workspace, with one allowlisted
/// directory (holding `readable.txt`) and one directory kept off the
/// allowlist.
async fn start_sandbox() -> Setup {
    ensure_helper();
    let workspace = tempfile::tempdir().expect("workspace dir");
    let allowed = tempfile::tempdir().expect("allowed dir");
    let denied = tempfile::tempdir().expect("denied dir");
    std::fs::write(allowed.path().join("readable.txt"), "allowlisted content\n")
        .expect("write allowlisted file");
    std::fs::write(denied.path().join("secret.txt"), "should stay hidden\n")
        .expect("write denied file");

    let config = SandboxConfig {
        kind: SandboxKind::MacosSandboxExec,
        read_allowlist: vec![allowed.path().to_path_buf()],
        workspace_mount: Some(workspace.path().to_path_buf()),
        ..SandboxConfig::default()
    };
    let mut sandbox = silo_sandbox::create_sandbox(&config, Some(proxy_handle()), None, journal())
        .await
        .expect("create sandbox");
    sandbox.start().await.expect("start sandbox");
    Setup {
        sandbox,
        workspace,
        allowed,
        denied,
    }
}

async fn run(setup: &Setup, name: &str, input: serde_json::Value) -> ToolOutput {
    setup
        .sandbox
        .run_tool(&"agent-0".to_string(), &call(name, input))
        .await
        .expect("run_tool transport")
}

#[tokio::test]
#[ignore = "requires a working /usr/bin/sandbox-exec"]
async fn write_read_edit_bash_roundtrip_in_the_workspace() {
    let mut setup = start_sandbox().await;

    let output = run(
        &setup,
        "Write",
        serde_json::json!({"path": "notes.txt", "content": "first line\n"}),
    )
    .await;
    assert!(!output.is_error, "Write failed: {}", output.content);

    let output = run(&setup, "Read", serde_json::json!({"path": "notes.txt"})).await;
    assert!(!output.is_error, "Read failed: {}", output.content);
    assert_eq!(output.content, "first line\n");

    let output = run(
        &setup,
        "Edit",
        serde_json::json!({"path": "notes.txt", "old_string": "first", "new_string": "edited"}),
    )
    .await;
    assert!(!output.is_error, "Edit failed: {}", output.content);

    let output = run(
        &setup,
        "Bash",
        serde_json::json!({"command": "cat notes.txt && echo from-bash"}),
    )
    .await;
    assert!(!output.is_error, "Bash failed: {}", output.content);
    assert!(output.content.contains("edited line"));
    assert!(output.content.contains("from-bash"));

    // The write really landed in the host workspace directory.
    let on_disk =
        std::fs::read_to_string(setup.workspace.path().join("notes.txt")).expect("read back");
    assert_eq!(on_disk, "edited line\n");

    setup.sandbox.shutdown().await.expect("shutdown");
}

#[tokio::test]
#[ignore = "requires a working /usr/bin/sandbox-exec"]
async fn bash_cannot_write_outside_the_workspace() {
    let mut setup = start_sandbox().await;
    let target = setup.denied.path().join("escape.txt");

    let output = run(
        &setup,
        "Bash",
        serde_json::json!({"command": format!("echo escaped > '{}'", target.display())}),
    )
    .await;
    assert!(
        output.is_error,
        "writing outside the workspace succeeded: {}",
        output.content
    );
    assert!(!target.exists(), "file appeared outside the workspace");

    // The allowlisted directory is read-only too.
    let allowed_target = setup.allowed.path().join("escape.txt");
    let output = run(
        &setup,
        "Bash",
        serde_json::json!({"command": format!("echo escaped > '{}'", allowed_target.display())}),
    )
    .await;
    assert!(
        output.is_error,
        "writing into the allowlist succeeded: {}",
        output.content
    );
    assert!(!allowed_target.exists());

    setup.sandbox.shutdown().await.expect("shutdown");
}

#[tokio::test]
#[ignore = "requires a working /usr/bin/sandbox-exec"]
async fn reads_respect_the_allowlist() {
    let mut setup = start_sandbox().await;

    let allowed_file = setup.allowed.path().join("readable.txt");
    let output = run(
        &setup,
        "Read",
        serde_json::json!({"path": allowed_file.display().to_string()}),
    )
    .await;
    assert!(
        !output.is_error,
        "allowlisted read failed: {}",
        output.content
    );
    assert_eq!(output.content, "allowlisted content\n");

    let denied_file = setup.denied.path().join("secret.txt");
    let output = run(
        &setup,
        "Read",
        serde_json::json!({"path": denied_file.display().to_string()}),
    )
    .await;
    assert!(
        output.is_error,
        "read outside the allowlist succeeded: {}",
        output.content
    );

    // Same through Bash.
    let output = run(
        &setup,
        "Bash",
        serde_json::json!({"command": format!("cat '{}'", denied_file.display())}),
    )
    .await;
    assert!(
        output.is_error,
        "cat outside the allowlist succeeded: {}",
        output.content
    );
    assert!(!output.content.contains("should stay hidden"));

    setup.sandbox.shutdown().await.expect("shutdown");
}

#[tokio::test]
#[ignore = "requires a working /usr/bin/sandbox-exec"]
async fn user_shell_runs_in_the_workspace_and_stays_confined() {
    let escape_marker = PathBuf::from("/tmp/silo-shell-escape");
    let _ = std::fs::remove_file(&escape_marker);

    let mut setup = start_sandbox().await;
    let code = setup
        .sandbox
        .user_shell(Some(vec![
            "/bin/sh".into(),
            "-c".into(),
            "pwd && touch shell-made.txt && \
             (touch /tmp/silo-shell-escape 2>/dev/null && echo ESCAPED || echo CONFINED)"
                .into(),
        ]))
        .await
        .expect("user_shell");
    assert_eq!(code, 0, "user shell exited with {code}");

    // The shell ran in the workspace mount.
    assert!(
        setup.workspace.path().join("shell-made.txt").is_file(),
        "shell-made.txt missing from the workspace"
    );

    // The write outside the workspace was denied by the profile.
    assert!(
        !escape_marker.exists(),
        "the user shell wrote outside the sandbox"
    );

    setup.sandbox.shutdown().await.expect("shutdown");
}

#[tokio::test]
#[ignore = "requires a working /usr/bin/sandbox-exec"]
async fn interrupt_cancels_a_slow_bash_command() {
    let mut setup = start_sandbox().await;

    let started = std::time::Instant::now();
    let slow_call = call(
        "Bash",
        serde_json::json!({"command": "echo before-sleep; sleep 30; echo after-sleep"}),
    );
    let agent = "agent-0".to_string();
    let output = {
        let sandbox: &dyn Sandbox = setup.sandbox.as_ref();
        let slow = sandbox.run_tool(&agent, &slow_call);
        tokio::pin!(slow);
        // Give the command time to start and emit its first line, then
        // cancel.
        tokio::select! {
            output = &mut slow => panic!("the command finished before the interrupt: {output:?}"),
            () = tokio::time::sleep(std::time::Duration::from_secs(2)) => {
                sandbox.interrupt().await.expect("interrupt");
                tokio::time::timeout(std::time::Duration::from_secs(10), slow)
                    .await
                    .expect("run_tool did not return promptly after the interrupt")
                    .expect("run_tool transport")
            }
        }
    };
    assert!(
        started.elapsed() < std::time::Duration::from_secs(20),
        "cancellation took {:?}",
        started.elapsed()
    );
    assert!(output.is_error, "expected an error output: {output:?}");
    assert!(
        output.content.contains("(cancelled)"),
        "missing cancelled marker: {}",
        output.content
    );
    assert!(
        output.content.contains("before-sleep"),
        "partial stdout missing: {}",
        output.content
    );
    assert!(!output.content.contains("after-sleep"));

    // The session still serves tool calls after a cancellation.
    let output = run(&setup, "Bash", serde_json::json!({"command": "echo alive"})).await;
    assert!(
        !output.is_error,
        "follow-up Bash failed: {}",
        output.content
    );
    assert!(output.content.contains("alive"));

    setup.sandbox.shutdown().await.expect("shutdown");
}

#[tokio::test]
#[ignore = "requires a working /usr/bin/sandbox-exec"]
async fn shutdown_removes_the_scratch_space() {
    let mut setup = start_sandbox().await;
    let scratch_dir = PathBuf::from(setup.sandbox.access_report().scratch_dir);
    assert!(scratch_dir.is_dir(), "scratch missing while running");
    assert!(
        scratch_dir.join("proxy-ca.pem").is_file(),
        "proxy CA certificate missing from the scratch space"
    );
    setup.sandbox.shutdown().await.expect("shutdown");
    assert!(!scratch_dir.exists(), "scratch survived shutdown");
}

#[tokio::test]
#[ignore = "requires a working /usr/bin/sandbox-exec"]
async fn start_fails_cleanly_when_the_helper_cannot_run() {
    ensure_helper();
    let workspace = tempfile::tempdir().expect("workspace dir");
    let config = SandboxConfig {
        kind: SandboxKind::MacosSandboxExec,
        // A nonexistent allowlist entry fails canonicalization.
        read_allowlist: vec![PathBuf::from("/no/such/allowlist/path")],
        workspace_mount: Some(workspace.path().to_path_buf()),
        ..SandboxConfig::default()
    };
    let mut sandbox = silo_sandbox::create_sandbox(&config, Some(proxy_handle()), None, journal())
        .await
        .expect("create sandbox");
    let err = sandbox.start().await.expect_err("start must fail");
    assert!(matches!(err, SandboxError::Setup(_)), "got {err:?}");
}
