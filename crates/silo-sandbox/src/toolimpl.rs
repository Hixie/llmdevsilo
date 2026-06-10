//! Execution of the sandbox tools (Read, Write, Edit, Bash, WebFetch,
//! WebSearch) through a helper session.
//!
//! Tool-level failures — a missing file, a nonzero exit code, an edit
//! mismatch, an HTTP error status, malformed tool input — come back as
//! `Ok(ToolOutput::error(..))` so the model sees them as tool results.
//! Session and protocol failures come back as `Err(SandboxError)`.

use std::path::Path;

use silo_core::error::SandboxError;
use silo_core::helper::{b64, unb64, HelperOp, HelperPayload};
use silo_core::tool::{ToolCall, ToolOutput};

use crate::scratch::ScratchSpace;
use crate::search;
use crate::session::HelperSession;

/// Default Bash timeout when the call does not specify one.
const DEFAULT_BASH_TIMEOUT_MS: u64 = 120_000;

/// Upper bound on the Bash timeout regardless of the call input.
const MAX_BASH_TIMEOUT_MS: u64 = 600_000;

/// Default cap on a WebFetch response body.
const DEFAULT_FETCH_MAX_BYTES: u64 = 1_048_576;

/// Cap on the search results page fetched for WebSearch.
const SEARCH_MAX_BYTES: u64 = 2 * 1_048_576;

/// Runs one sandbox tool call via `session`. Relative paths in the call
/// input resolve against `workspace_mount`; Bash commands run in
/// `workspace_mount` with `HOME` and `TMPDIR` pointing into `scratch`.
pub async fn run_tool(
    session: &HelperSession,
    workspace_mount: &Path,
    scratch: &ScratchSpace,
    call: &ToolCall,
) -> Result<ToolOutput, SandboxError> {
    match call.name.as_str() {
        "Read" => read_tool(session, workspace_mount, call).await,
        "Write" => write_tool(session, workspace_mount, call).await,
        "Edit" => edit_tool(session, workspace_mount, call).await,
        "Bash" => bash_tool(session, workspace_mount, scratch, call).await,
        "WebFetch" => web_fetch_tool(session, call).await,
        "WebSearch" => web_search_tool(session, call).await,
        other => Err(SandboxError::Rejected(format!(
            "unknown sandbox tool {other:?}"
        ))),
    }
}

/// Sends one helper operation, separating per-request helper failures
/// (tool level, inner `Err`) from session failures (outer `Err`).
async fn helper_call(
    session: &HelperSession,
    op: HelperOp,
) -> Result<Result<HelperPayload, String>, SandboxError> {
    match session.request(op).await {
        Ok(payload) => Ok(Ok(payload)),
        Err(SandboxError::Helper(message)) => Ok(Err(message)),
        Err(other) => Err(other),
    }
}

fn unexpected_payload(tool: &str, payload: HelperPayload) -> SandboxError {
    SandboxError::Helper(format!("unexpected helper payload for {tool}: {payload:?}"))
}

fn str_field(call: &ToolCall, key: &str) -> Result<String, ToolOutput> {
    match call.input.get(key) {
        Some(serde_json::Value::String(value)) => Ok(value.clone()),
        _ => Err(ToolOutput::error(format!(
            "{}: missing required string field {key:?}",
            call.name
        ))),
    }
}

fn u64_field(call: &ToolCall, key: &str) -> Result<Option<u64>, ToolOutput> {
    match call.input.get(key) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(value) => match value.as_u64() {
            Some(number) => Ok(Some(number)),
            None => Err(ToolOutput::error(format!(
                "{}: field {key:?} must be a non-negative integer",
                call.name
            ))),
        },
    }
}

fn bool_field(call: &ToolCall, key: &str) -> bool {
    call.input
        .get(key)
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// Resolves a tool-supplied path against the workspace mount.
fn resolve_path(workspace_mount: &Path, path: &str) -> String {
    let candidate = Path::new(path);
    if candidate.is_absolute() {
        path.to_string()
    } else {
        workspace_mount.join(candidate).display().to_string()
    }
}

async fn read_tool(
    session: &HelperSession,
    workspace_mount: &Path,
    call: &ToolCall,
) -> Result<ToolOutput, SandboxError> {
    let path = match str_field(call, "path") {
        Ok(path) => path,
        Err(output) => return Ok(output),
    };
    let offset = match u64_field(call, "offset") {
        Ok(offset) => offset,
        Err(output) => return Ok(output),
    };
    let limit = match u64_field(call, "limit") {
        Ok(limit) => limit,
        Err(output) => return Ok(output),
    };
    let op = HelperOp::ReadFile {
        path: resolve_path(workspace_mount, &path),
        offset,
        limit,
    };
    let payload = match helper_call(session, op).await? {
        Ok(payload) => payload,
        Err(message) => return Ok(ToolOutput::error(message)),
    };
    let HelperPayload::File {
        content_b64,
        truncated,
    } = payload
    else {
        return Err(unexpected_payload("Read", payload));
    };
    let bytes = unb64(&content_b64).map_err(SandboxError::Helper)?;
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if truncated {
        if !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str("[truncated]");
    }
    Ok(ToolOutput::ok(text))
}

async fn write_tool(
    session: &HelperSession,
    workspace_mount: &Path,
    call: &ToolCall,
) -> Result<ToolOutput, SandboxError> {
    let path = match str_field(call, "path") {
        Ok(path) => path,
        Err(output) => return Ok(output),
    };
    // "content_b64" carries binary content base64-encoded; the harness uses
    // it to store client uploads byte-for-byte. "content" is the plain-text
    // field in the LLM-facing schema. Exactly one must be present.
    let content_b64 = match call.input.get("content_b64") {
        Some(value) => {
            if call.input.get("content").is_some() {
                return Ok(ToolOutput::error(
                    "Write: the content and content_b64 fields are mutually exclusive",
                ));
            }
            match value.as_str() {
                Some(text) => match unb64(text) {
                    Ok(_) => text.to_string(),
                    Err(message) => return Ok(ToolOutput::error(message)),
                },
                None => {
                    return Ok(ToolOutput::error(
                        "the content_b64 field must be a base64 string",
                    ))
                }
            }
        }
        None => match str_field(call, "content") {
            Ok(content) => b64(content.as_bytes()),
            Err(output) => return Ok(output),
        },
    };
    let append = bool_field(call, "append");
    let op = HelperOp::WriteFile {
        path: resolve_path(workspace_mount, &path),
        content_b64,
        append,
    };
    let payload = match helper_call(session, op).await? {
        Ok(payload) => payload,
        Err(message) => return Ok(ToolOutput::error(message)),
    };
    let HelperPayload::Written { bytes } = payload else {
        return Err(unexpected_payload("Write", payload));
    };
    let verb = if append { "Appended" } else { "Wrote" };
    Ok(ToolOutput::ok(format!("{verb} {bytes} bytes to {path}")))
}

async fn edit_tool(
    session: &HelperSession,
    workspace_mount: &Path,
    call: &ToolCall,
) -> Result<ToolOutput, SandboxError> {
    let path = match str_field(call, "path") {
        Ok(path) => path,
        Err(output) => return Ok(output),
    };
    let old = match str_field(call, "old_string") {
        Ok(old) => old,
        Err(output) => return Ok(output),
    };
    let new = match str_field(call, "new_string") {
        Ok(new) => new,
        Err(output) => return Ok(output),
    };
    let op = HelperOp::EditFile {
        path: resolve_path(workspace_mount, &path),
        old,
        new,
        replace_all: bool_field(call, "replace_all"),
    };
    let payload = match helper_call(session, op).await? {
        Ok(payload) => payload,
        Err(message) => return Ok(ToolOutput::error(message)),
    };
    let HelperPayload::Edited { replacements } = payload else {
        return Err(unexpected_payload("Edit", payload));
    };
    let noun = if replacements == 1 {
        "occurrence"
    } else {
        "occurrences"
    };
    Ok(ToolOutput::ok(format!(
        "Replaced {replacements} {noun} in {path}"
    )))
}

async fn bash_tool(
    session: &HelperSession,
    workspace_mount: &Path,
    scratch: &ScratchSpace,
    call: &ToolCall,
) -> Result<ToolOutput, SandboxError> {
    let command = match str_field(call, "command") {
        Ok(command) => command,
        Err(output) => return Ok(output),
    };
    let timeout_ms = match u64_field(call, "timeout_ms") {
        Ok(timeout) => timeout
            .unwrap_or(DEFAULT_BASH_TIMEOUT_MS)
            .min(MAX_BASH_TIMEOUT_MS),
        Err(output) => return Ok(output),
    };
    let op = HelperOp::Exec {
        command,
        cwd: Some(workspace_mount.display().to_string()),
        env: vec![
            ("HOME".into(), scratch.home_dir().display().to_string()),
            ("TMPDIR".into(), scratch.tmp_dir().display().to_string()),
        ],
        timeout_ms,
    };
    let payload = match helper_call(session, op).await? {
        Ok(payload) => payload,
        Err(message) => return Ok(ToolOutput::error(message)),
    };
    let HelperPayload::Exec {
        exit_code,
        stdout,
        stderr,
        timed_out,
        truncated,
        cancelled,
    } = payload
    else {
        return Err(unexpected_payload("Bash", payload));
    };

    let mut sections: Vec<String> = Vec::new();
    let stdout = stdout.trim_end_matches('\n');
    let stderr = stderr.trim_end_matches('\n');
    if !stdout.is_empty() {
        sections.push(stdout.to_string());
    }
    if !stderr.is_empty() {
        sections.push(format!("--- stderr ---\n{stderr}"));
    }
    if truncated {
        sections.push("[output truncated]".to_string());
    }
    if cancelled {
        sections.push("(cancelled)".to_string());
    } else if timed_out {
        sections.push("(timed out)".to_string());
    } else if exit_code != 0 {
        sections.push(format!("(exit code {exit_code})"));
    }
    let content = sections.join("\n");
    if cancelled || timed_out || exit_code != 0 {
        Ok(ToolOutput::error(content))
    } else {
        Ok(ToolOutput::ok(content))
    }
}

async fn web_fetch_tool(
    session: &HelperSession,
    call: &ToolCall,
) -> Result<ToolOutput, SandboxError> {
    let url = match str_field(call, "url") {
        Ok(url) => url,
        Err(output) => return Ok(output),
    };
    let max_bytes = match u64_field(call, "max_bytes") {
        Ok(max_bytes) => max_bytes.unwrap_or(DEFAULT_FETCH_MAX_BYTES),
        Err(output) => return Ok(output),
    };
    let op = HelperOp::Fetch {
        url,
        method: "GET".into(),
        headers: vec![],
        body_b64: None,
        max_bytes,
    };
    let payload = match helper_call(session, op).await? {
        Ok(payload) => payload,
        Err(message) => return Ok(ToolOutput::error(message)),
    };
    let HelperPayload::Fetched {
        status,
        headers: _,
        body_b64,
        truncated,
    } = payload
    else {
        return Err(unexpected_payload("WebFetch", payload));
    };
    let body = unb64(&body_b64).map_err(SandboxError::Helper)?;
    let mut content = format!("HTTP {status}\n{}", String::from_utf8_lossy(&body));
    if truncated {
        if !content.ends_with('\n') {
            content.push('\n');
        }
        content.push_str("[truncated]");
    }
    if status >= 400 {
        Ok(ToolOutput::error(content))
    } else {
        Ok(ToolOutput::ok(content))
    }
}

async fn web_search_tool(
    session: &HelperSession,
    call: &ToolCall,
) -> Result<ToolOutput, SandboxError> {
    let query = match str_field(call, "query") {
        Ok(query) => query,
        Err(output) => return Ok(output),
    };
    let url = format!(
        "https://html.duckduckgo.com/html/?q={}",
        search::percent_encode(&query)
    );
    let op = HelperOp::Fetch {
        url,
        method: "GET".into(),
        headers: vec![(
            "User-Agent".into(),
            "Mozilla/5.0 (compatible; llmdevsilo)".into(),
        )],
        body_b64: None,
        max_bytes: SEARCH_MAX_BYTES,
    };
    let payload = match helper_call(session, op).await? {
        Ok(payload) => payload,
        Err(message) => return Ok(ToolOutput::error(message)),
    };
    let HelperPayload::Fetched {
        status,
        headers: _,
        body_b64,
        truncated: _,
    } = payload
    else {
        return Err(unexpected_payload("WebSearch", payload));
    };
    if status != 200 {
        return Ok(ToolOutput::error(format!("search failed: HTTP {status}")));
    }
    let body = unb64(&body_b64).map_err(SandboxError::Helper)?;
    let html = String::from_utf8_lossy(&body);
    Ok(ToolOutput::ok(search::parse_results(&html)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    async fn test_session() -> (HelperSession, tempfile::TempDir, ScratchSpace) {
        let (client_side, server_side) = tokio::io::duplex(1 << 16);
        tokio::spawn(async move {
            let _ = silo_helper::serve_stream_with_config(
                server_side,
                silo_helper::FetchConfig::default(),
            )
            .await;
        });
        let session = HelperSession::from_stream(client_side).await.unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let scratch = ScratchSpace::create(None, "CA").unwrap();
        (session, workspace, scratch)
    }

    fn call(name: &str, input: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "t1".into(),
            name: name.into(),
            input,
        }
    }

    #[tokio::test]
    async fn missing_required_fields_are_tool_errors() {
        let (session, workspace, scratch) = test_session().await;
        for (name, input) in [
            ("Read", json!({})),
            ("Write", json!({"path": "x"})),
            ("Edit", json!({"path": "x", "old_string": "a"})),
            ("Bash", json!({})),
            ("WebFetch", json!({})),
            ("WebSearch", json!({})),
        ] {
            let output = run_tool(&session, workspace.path(), &scratch, &call(name, input))
                .await
                .unwrap();
            assert!(output.is_error, "{name} accepted incomplete input");
            assert!(
                output.content.contains("missing required"),
                "{}",
                output.content
            );
        }
    }

    #[tokio::test]
    async fn write_content_b64_stores_non_utf8_bytes_intact() {
        let (session, workspace, scratch) = test_session().await;
        let bytes: &[u8] = &[0xde, 0xad, 0xbe, 0xef, 0x00, 0xff, 0xfe];
        let output = run_tool(
            &session,
            workspace.path(),
            &scratch,
            &call(
                "Write",
                json!({"path": "blob.bin", "content_b64": b64(bytes)}),
            ),
        )
        .await
        .unwrap();
        assert!(!output.is_error, "{}", output.content);
        assert!(output.content.contains("7 bytes"), "{}", output.content);
        assert_eq!(
            std::fs::read(workspace.path().join("blob.bin")).unwrap(),
            bytes
        );
    }

    #[tokio::test]
    async fn write_rejects_content_alongside_content_b64() {
        let (session, workspace, scratch) = test_session().await;
        let output = run_tool(
            &session,
            workspace.path(),
            &scratch,
            &call(
                "Write",
                json!({"path": "x", "content": "a", "content_b64": "YQ=="}),
            ),
        )
        .await
        .unwrap();
        assert!(output.is_error);
        assert!(
            output.content.contains("mutually exclusive"),
            "{}",
            output.content
        );
        assert!(!workspace.path().join("x").exists());
    }

    #[tokio::test]
    async fn write_rejects_invalid_content_b64() {
        let (session, workspace, scratch) = test_session().await;
        for input in [
            json!({"path": "x", "content_b64": "%%%not-base64%%%"}),
            json!({"path": "x", "content_b64": 5}),
        ] {
            let output = run_tool(&session, workspace.path(), &scratch, &call("Write", input))
                .await
                .unwrap();
            assert!(output.is_error, "{}", output.content);
        }
        assert!(!workspace.path().join("x").exists());
    }

    #[tokio::test]
    async fn bad_field_types_are_tool_errors() {
        let (session, workspace, scratch) = test_session().await;
        let output = run_tool(
            &session,
            workspace.path(),
            &scratch,
            &call("Read", json!({"path": "x", "offset": -4})),
        )
        .await
        .unwrap();
        assert!(output.is_error);
        assert!(output.content.contains("non-negative integer"));
    }

    #[tokio::test]
    async fn unknown_tool_is_a_session_error() {
        let (session, workspace, scratch) = test_session().await;
        let err = run_tool(
            &session,
            workspace.path(),
            &scratch,
            &call("Nope", json!({})),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, SandboxError::Rejected(_)), "got {err:?}");
    }

    #[test]
    fn relative_paths_resolve_against_the_workspace() {
        let workspace = Path::new("/work/space");
        assert_eq!(
            resolve_path(workspace, "src/main.rs"),
            "/work/space/src/main.rs"
        );
        assert_eq!(resolve_path(workspace, "/etc/hosts"), "/etc/hosts");
    }
}
