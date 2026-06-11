//! End-to-end check of the script-failure exit code: `silo run` with a
//! deliberately short test script exits with code 4 and prints the
//! mismatch detail with the remaining-entry summary on standard error.

use std::process::Command;

#[test]
fn a_short_script_exits_4_with_the_summary_on_stderr() {
    let temp = tempfile::tempdir().expect("tempdir");
    let state = temp.path().join("state");
    let workspace = temp.path().join("ws");
    std::fs::create_dir_all(&workspace).expect("create workspace");

    // The scripted user sends a prompt but the llm list is empty, so the
    // first completion request finds the script exhausted.
    let script_path = temp.path().join("short.json");
    std::fs::write(
        &script_path,
        serde_json::json!({
            "name": "short",
            "frontend": [{ "step": "send_prompt", "text": "go" }]
        })
        .to_string(),
    )
    .expect("write script");

    let output = Command::new(env!("CARGO_BIN_EXE_silo"))
        .env("LLMDEVSILO_STATE_DIR", &state)
        .args(["run", "--workspace"])
        .arg(&workspace)
        .args([
            "--create",
            "--deterministic",
            "--frontend",
            "mock",
            "--llm",
            "mock",
            "--sandbox",
            "mock",
            "--mock-proxy",
            "--script",
        ])
        .arg(&script_path)
        .output()
        .expect("run silo");

    assert_eq!(output.status.code(), Some(4), "output: {output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("script failure: llm script mismatch: llm script exhausted"),
        "stderr was: {stderr}"
    );
    assert!(
        stderr.contains("remaining: llm 0/0, tools 0/0, frontend 1/1"),
        "stderr was: {stderr}"
    );
}
