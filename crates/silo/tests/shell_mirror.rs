//! CLI-level tests for `silo shell` against a workspace attached to a
//! (simulated) running harness: the registry records a live primary
//! attachment and a run file carries the harness's sandbox policy.
//!
//! The mirroring decision and its printed notices are covered without a
//! real sandbox. The full smoke test (a non-interactive command running
//! inside sandbox-exec under the mirrored policy) is `#[ignore]`d because
//! it needs macOS with a working `/usr/bin/sandbox-exec`:
//!
//! ```text
//! cargo test -p silo --test shell_mirror -- --ignored
//! ```

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use silo_core::protocol::RunInfo;
use silo_workspace::{ContainerStrategy, WorkspaceManager};

const HARNESS_ID: &str = "fakeharness";

struct Fixture {
    _temp: tempfile::TempDir,
    state: PathBuf,
    ws: PathBuf,
    /// Keeps the primary registration alive; detaches on drop.
    _primary: silo_workspace::AttachedWorkspace,
}

/// Locks a plain-directory workspace, attaches this test process as the
/// primary "harness", and writes a run file advertising the given sandbox
/// policy.
fn fixture(sandbox_kind: &str, read_allowlist: Vec<String>) -> Fixture {
    let temp = tempfile::tempdir().expect("tempdir");
    let state = temp.path().join("state");
    let ws = temp.path().join("project");
    std::fs::create_dir_all(&ws).expect("create workspace");
    std::fs::write(ws.join("seed.txt"), "seeded\n").expect("seed workspace");

    let manager = WorkspaceManager::with_strategy(state.clone(), ContainerStrategy::PlainDir);
    manager.lock(&ws).expect("lock");
    let primary = manager.attach(&ws, HARNESS_ID).expect("attach primary");

    let run_info = RunInfo {
        harness_id: HARNESS_ID.into(),
        addr: "127.0.0.1:1".into(),
        cert_fingerprint_sha256: "00".repeat(32),
        local_token_path: "/nonexistent/local-token".into(),
        pid: std::process::id(),
        workspace: ws.display().to_string(),
        sandbox_kind: Some(sandbox_kind.into()),
        read_allowlist,
        allowed_domains: vec!["example.com".into()],
    };
    let runs_dir = silo_core::paths::runs_dir(&state);
    std::fs::create_dir_all(&runs_dir).expect("create runs dir");
    std::fs::write(
        runs_dir.join(format!("{HARNESS_ID}.json")),
        serde_json::to_string_pretty(&run_info).expect("serialize run info"),
    )
    .expect("write run file");

    Fixture {
        _temp: temp,
        state,
        ws,
        _primary: primary,
    }
}

fn run_shell(fixture: &Fixture, extra_flags: &[&str], command: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_silo"));
    cmd.env("LLMDEVSILO_STATE_DIR", &fixture.state)
        .arg("shell")
        .arg("--workspace")
        .arg(&fixture.ws)
        .args(extra_flags)
        .arg("--")
        .args(command);
    cmd.output().expect("run silo shell")
}

#[test]
fn shell_prints_the_mirroring_notice_for_a_live_harness() {
    // The mock sandbox kind exercises the mirroring decision without a
    // platform sandbox; sandbox creation then fails (the mock backend
    // needs a script), which is irrelevant to the notice under test.
    let fixture = fixture("mock", Vec::new());
    let output = run_shell(&fixture, &[], &["/bin/sh", "-c", "true"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(&format!(
            "Mirroring running harness {HARNESS_ID}'s sandbox policy (mock"
        )),
        "stdout was: {stdout}"
    );
    assert!(
        stdout.contains("credential injection is not mirrored"),
        "stdout was: {stdout}"
    );
}

#[test]
fn mirrored_risky_entries_are_accepted_by_inheritance() {
    // The harness state directory is a guaranteed risk-scan hit. A run
    // file advertising it in the read allowlist exercises the inheritance
    // path: the mirrored entry passes the shell's risk scan with a
    // printed notice instead of a refusal.
    let fixture = fixture("mock", Vec::new());
    let state_entry = fixture.state.display().to_string();
    let run_file = silo_core::paths::runs_dir(&fixture.state).join(format!("{HARNESS_ID}.json"));
    let text = std::fs::read_to_string(&run_file).expect("read run file");
    let mut info: RunInfo = serde_json::from_str(&text).expect("parse run file");
    info.read_allowlist = vec![state_entry.clone()];
    std::fs::write(
        &run_file,
        serde_json::to_string_pretty(&info).expect("serialize run info"),
    )
    .expect("rewrite run file");

    let output = run_shell(&fixture, &[], &["/bin/sh", "-c", "true"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains(&format!(
            "accepted by inheritance from running harness {HARNESS_ID}"
        )),
        "stdout was: {stdout}"
    );
    assert!(
        stdout.contains(&state_entry),
        "stdout did not list the inherited entry: {stdout}"
    );
    assert!(
        !stderr.contains("refusing risky read allowlist"),
        "the inherited entry was refused: {stderr}"
    );
}

#[test]
fn explicit_sandbox_flags_win_over_mirroring_with_a_note() {
    let fixture = fixture("mock", Vec::new());
    let output = run_shell(&fixture, &["--sandbox", "mock"], &["/bin/sh", "-c", "true"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(&format!(
            "explicit sandbox flags given; this shell's access policy \
             differs from running harness {HARNESS_ID}'s"
        )),
        "stdout was: {stdout}"
    );
    assert!(!stdout.contains("Mirroring"), "stdout was: {stdout}");
}

/// Resolves (building if necessary) the silo-helper binary for the
/// sandbox-exec backend.
#[cfg(target_os = "macos")]
fn helper_bin() -> PathBuf {
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
        let status = Command::new(cargo)
            .args(["build", "-p", "silo-helper"])
            .status()
            .expect("spawn cargo build -p silo-helper");
        assert!(status.success(), "building silo-helper failed");
    }
    assert!(candidate.is_file(), "missing {}", candidate.display());
    candidate
}

#[test]
#[ignore = "requires a working /usr/bin/sandbox-exec"]
#[cfg(target_os = "macos")]
fn shell_smoke_mirrors_the_policy_and_runs_a_command() {
    let allowed = tempfile::tempdir().expect("allowed dir");
    std::fs::write(allowed.path().join("readable.txt"), "allowlisted\n")
        .expect("write allowlisted file");
    let allowed_canon = std::fs::canonicalize(allowed.path()).expect("canonicalize");

    let fixture = fixture(
        "macos-sandbox-exec",
        vec![allowed_canon.display().to_string()],
    );

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_silo"));
    cmd.env("LLMDEVSILO_STATE_DIR", &fixture.state)
        .env("SILO_HELPER_BIN", helper_bin())
        .arg("shell")
        .arg("--workspace")
        .arg(&fixture.ws)
        .arg("--")
        .args([
            "/bin/sh",
            "-c",
            &format!(
                "cat seed.txt && cat '{}/readable.txt' && echo proven > from-shell.txt",
                allowed_canon.display()
            ),
        ]);
    let output = cmd.output().expect("run silo shell");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "silo shell failed.\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains(&format!(
            "Mirroring running harness {HARNESS_ID}'s sandbox policy (macos-sandbox-exec"
        )),
        "stdout was: {stdout}"
    );
    assert!(stdout.contains("seeded"), "stdout was: {stdout}");
    assert!(stdout.contains("allowlisted"), "stdout was: {stdout}");

    // The shell's write landed in the shared mount, visible through the
    // primary attachment's view of the workspace data.
    let manager =
        WorkspaceManager::with_strategy(fixture.state.clone(), ContainerStrategy::PlainDir);
    let status = manager.status(&fixture.ws).expect("status");
    assert!(status.locked);
    assert_eq!(status.attached_harness.as_deref(), Some(HARNESS_ID));
    assert_eq!(status.live_shells, 0, "the shell detached on exit");
    let data = data_dir(&fixture.state, &fixture.ws);
    assert_eq!(
        std::fs::read_to_string(data.join("from-shell.txt")).expect("read from-shell.txt"),
        "proven\n"
    );
}

/// Locates the locked workspace's data directory through the registry.
#[cfg(target_os = "macos")]
fn data_dir(state: &Path, ws: &Path) -> PathBuf {
    let registry_path = state.join("workspaces").join("registry.json");
    let text = std::fs::read_to_string(registry_path).expect("read registry");
    let value: serde_json::Value = serde_json::from_str(&text).expect("parse registry");
    let key = std::fs::canonicalize(ws).expect("canonicalize workspace");
    let id = value[key.to_str().expect("utf-8 path")]["id"]
        .as_str()
        .expect("registry id")
        .to_string();
    state.join("workspaces").join(id).join("data")
}
