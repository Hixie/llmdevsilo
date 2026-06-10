//! Integration tests for the command-line surface of silo-tui. These
//! exercise the paths that resolve a connection target before any terminal
//! or network setup, so they run headless and deterministically.

use std::path::Path;
use std::process::{Command, Output};

fn run_tui(state_dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_silo-tui"))
        .args(args)
        .env("LLMDEVSILO_STATE_DIR", state_dir)
        .env_remove("USER")
        .output()
        .expect("the silo-tui binary should spawn")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn write_run_file(state_dir: &Path, harness_id: &str, token_path: &Path) {
    let runs = state_dir.join("run");
    std::fs::create_dir_all(&runs).unwrap();
    let info = serde_json::json!({
        "harness_id": harness_id,
        "addr": "127.0.0.1:1",
        "cert_fingerprint_sha256": "00".repeat(32),
        "local_token_path": token_path.to_string_lossy(),
        "pid": 1,
        "workspace": "/tmp/ws",
    });
    std::fs::write(
        runs.join(format!("{harness_id}.json")),
        serde_json::to_string(&info).unwrap(),
    )
    .unwrap();
}

#[test]
fn no_harnesses_exits_with_guidance() {
    let state = tempfile::tempdir().unwrap();
    let output = run_tui(state.path(), &[]);
    assert!(!output.status.success());
    let message = stderr(&output);
    assert!(message.contains("no local harnesses"), "stderr: {message}");
    assert!(message.contains("silo run"), "stderr: {message}");
}

#[test]
fn unknown_harness_id_lists_running_harnesses() {
    let state = tempfile::tempdir().unwrap();
    let token = state.path().join("token");
    std::fs::write(&token, "aa".repeat(32)).unwrap();
    write_run_file(state.path(), "abc123", &token);

    let output = run_tui(state.path(), &["--harness", "nope"]);
    assert!(!output.status.success());
    let message = stderr(&output);
    assert!(message.contains("nope"), "stderr: {message}");
    assert!(message.contains("abc123"), "stderr: {message}");
}

#[test]
fn unknown_harness_id_without_any_running_says_so() {
    let state = tempfile::tempdir().unwrap();
    let output = run_tui(state.path(), &["--harness", "nope"]);
    assert!(!output.status.success());
    let message = stderr(&output);
    assert!(
        message.contains("no local harnesses are running"),
        "stderr: {message}"
    );
}

#[test]
fn harness_with_unreadable_token_fails_cleanly() {
    let state = tempfile::tempdir().unwrap();
    let missing = state.path().join("missing-token");
    write_run_file(state.path(), "abc123", &missing);

    let output = run_tui(state.path(), &["--harness", "abc123"]);
    assert!(!output.status.success());
    let message = stderr(&output);
    assert!(message.contains("local token"), "stderr: {message}");
}

#[test]
fn remote_url_without_fingerprint_or_known_host_is_rejected() {
    let state = tempfile::tempdir().unwrap();
    let output = run_tui(state.path(), &["--url", "example.com:7777"]);
    assert!(!output.status.success());
    let message = stderr(&output);
    assert!(message.contains("--fingerprint"), "stderr: {message}");
}

#[test]
fn remote_url_without_a_saved_key_suggests_pairing() {
    let state = tempfile::tempdir().unwrap();
    let output = run_tui(
        state.path(),
        &[
            "--url",
            "example.com:7777",
            "--fingerprint",
            &"ab".repeat(32),
        ],
    );
    assert!(!output.status.success());
    let message = stderr(&output);
    assert!(message.contains("--pair"), "stderr: {message}");
}

#[test]
fn fingerprint_mismatch_with_known_hosts_is_rejected() {
    let state = tempfile::tempdir().unwrap();
    let keys_dir = state.path().join("client-keys");
    std::fs::create_dir_all(&keys_dir).unwrap();
    let known = serde_json::json!({
        "hosts": { "example.com:7777": "ab".repeat(32) }
    });
    std::fs::write(
        keys_dir.join("known-hosts.json"),
        serde_json::to_string(&known).unwrap(),
    )
    .unwrap();

    let output = run_tui(
        state.path(),
        &[
            "--url",
            "example.com:7777",
            "--fingerprint",
            &"cd".repeat(32),
        ],
    );
    assert!(!output.status.success());
    let message = stderr(&output);
    assert!(message.contains("does not match"), "stderr: {message}");
}

#[test]
fn pair_flag_requires_url() {
    let state = tempfile::tempdir().unwrap();
    let output = run_tui(state.path(), &["--pair", "CODE1234"]);
    assert!(!output.status.success());
    let message = stderr(&output);
    assert!(message.contains("--url"), "stderr: {message}");
}

#[test]
fn harness_and_url_conflict() {
    let state = tempfile::tempdir().unwrap();
    let output = run_tui(state.path(), &["--harness", "x", "--url", "example.com:1"]);
    assert!(!output.status.success());
}
