//! End-to-end check of the async subagent flow through the real `silo run`
//! binary: a mock script spawns two subagents and collects both with
//! AwaitAgent, and the session exits 0 with the final report.

use std::process::Command;

#[test]
fn spawning_and_awaiting_two_subagents_exits_zero() {
    let temp = tempfile::tempdir().expect("tempdir");
    let state = temp.path().join("state");
    let workspace = temp.path().join("ws");
    std::fs::create_dir_all(&workspace).expect("create workspace");

    // Spawn alpha and beta, then collect both with AwaitAgent (await-any).
    // The subagent turns are keyed by their prompts; the two collection
    // turns are keyed by the previous result text, which the mock consumes
    // in order, so completion order does not matter.
    let script = serde_json::json!({
        "name": "run_subagents",
        "llm": [
            {
                "expect_user_contains": "do both",
                "response": {
                    "content": [
                        {"type": "text", "text": "Delegating."},
                        {"type": "tool_use", "id": "t1", "name": "Agent",
                         "input": {"prompt": "alpha task", "name": "alpha"}},
                        {"type": "tool_use", "id": "t2", "name": "Agent",
                         "input": {"prompt": "beta task", "name": "beta"}}
                    ],
                    "stop_reason": "tool_use",
                    "usage": {"input_tokens": 0, "output_tokens": 0}
                }
            },
            {
                "expect_user_contains": "alpha task",
                "response": {
                    "content": [{"type": "text", "text": "alpha done"}],
                    "stop_reason": "end_turn",
                    "usage": {"input_tokens": 0, "output_tokens": 0}
                }
            },
            {
                "expect_user_contains": "beta task",
                "response": {
                    "content": [{"type": "text", "text": "beta done"}],
                    "stop_reason": "end_turn",
                    "usage": {"input_tokens": 0, "output_tokens": 0}
                }
            },
            {
                "expect_user_contains": "runs in the background",
                "response": {
                    "content": [
                        {"type": "text", "text": "Collecting first."},
                        {"type": "tool_use", "id": "a1", "name": "AwaitAgent", "input": {}}
                    ],
                    "stop_reason": "tool_use",
                    "usage": {"input_tokens": 0, "output_tokens": 0}
                }
            },
            {
                "expect_user_contains": "finished",
                "response": {
                    "content": [
                        {"type": "text", "text": "Collecting second."},
                        {"type": "tool_use", "id": "a2", "name": "AwaitAgent", "input": {}}
                    ],
                    "stop_reason": "tool_use",
                    "usage": {"input_tokens": 0, "output_tokens": 0}
                }
            },
            {
                "expect_user_contains": "finished",
                "response": {
                    "content": [
                        {"type": "tool_use", "id": "x", "name": "Exit",
                         "input": {"message": "Both subagents collected."}}
                    ],
                    "stop_reason": "tool_use",
                    "usage": {"input_tokens": 0, "output_tokens": 0}
                }
            }
        ],
        "tools": [],
        "frontend": [
            {"step": "send_prompt", "text": "do both"},
            {"step": "expect_shutdown", "message_contains": "collected"}
        ],
        "network": []
    });

    let script_path = temp.path().join("subagents.json");
    std::fs::write(&script_path, script.to_string()).expect("write script");

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

    assert_eq!(output.status.code(), Some(0), "output: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Both subagents collected."),
        "stdout was: {stdout}"
    );
}
