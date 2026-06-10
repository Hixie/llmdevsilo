//! End-to-end tool execution against an in-process helper: one side of a
//! duplex stream runs `silo_helper::serve_stream`, the other side is a
//! `HelperSession`, and a temporary directory stands in for the workspace
//! mount.

use std::net::SocketAddr;
use std::path::Path;

use serde_json::json;
use silo_core::helper::{HelperOp, HelperPayload};
use silo_core::tool::{ToolCall, ToolOutput};
use silo_sandbox::scratch::ScratchSpace;
use silo_sandbox::session::HelperSession;
use silo_sandbox::toolimpl::run_tool;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

struct Fixture {
    session: HelperSession,
    workspace: tempfile::TempDir,
    scratch: ScratchSpace,
}

impl Fixture {
    async fn new() -> Fixture {
        let (client_side, server_side) = tokio::io::duplex(1 << 16);
        let config = silo_helper::FetchConfig {
            proxy_url: None,
            ca_cert_path: None,
        };
        tokio::spawn(async move {
            let _ = silo_helper::serve_stream_with_config(server_side, config).await;
        });
        let session = HelperSession::from_stream(client_side).await.unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let scratch = ScratchSpace::create(None, "TEST CA PEM").unwrap();
        Fixture {
            session,
            workspace,
            scratch,
        }
    }

    fn workspace(&self) -> &Path {
        self.workspace.path()
    }

    async fn run(&self, name: &str, input: serde_json::Value) -> ToolOutput {
        let call = ToolCall {
            id: "call-1".into(),
            name: name.into(),
            input,
        };
        run_tool(&self.session, self.workspace(), &self.scratch, &call)
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn write_content_b64_stores_binary_bytes_intact() {
    let fx = Fixture::new().await;
    let bytes: Vec<u8> = vec![0x00, 0xff, 0x10, 0x80, 0x7f, 0x00, 0xfe];
    let output = fx
        .run(
            "Write",
            json!({
                "path": "_uploads/blob.bin",
                "content_b64": silo_core::helper::b64(&bytes),
            }),
        )
        .await;
    assert!(!output.is_error, "{}", output.content);
    assert_eq!(
        std::fs::read(fx.workspace().join("_uploads/blob.bin")).unwrap(),
        bytes
    );

    let output = fx
        .run(
            "Write",
            json!({"path": "x.bin", "content_b64": "not base64!"}),
        )
        .await;
    assert!(output.is_error);
}

#[tokio::test]
async fn write_read_edit_listdir_roundtrip() {
    let fx = Fixture::new().await;

    // Write, with parent directory creation.
    let output = fx
        .run(
            "Write",
            json!({"path": "notes/list.txt", "content": "alpha beta alpha\n"}),
        )
        .await;
    assert!(!output.is_error, "{}", output.content);
    assert_eq!(output.content, "Wrote 17 bytes to notes/list.txt");
    assert_eq!(
        std::fs::read_to_string(fx.workspace().join("notes/list.txt")).unwrap(),
        "alpha beta alpha\n"
    );

    // Append.
    let output = fx
        .run(
            "Write",
            json!({"path": "notes/list.txt", "content": "tail\n", "append": true}),
        )
        .await;
    assert_eq!(output.content, "Appended 5 bytes to notes/list.txt");

    // Read the whole file.
    let output = fx.run("Read", json!({"path": "notes/list.txt"})).await;
    assert!(!output.is_error);
    assert_eq!(output.content, "alpha beta alpha\ntail\n");

    // Read with offset and limit; more bytes remain, so a truncation note
    // is appended.
    let output = fx
        .run(
            "Read",
            json!({"path": "notes/list.txt", "offset": 6, "limit": 4}),
        )
        .await;
    assert!(!output.is_error);
    assert_eq!(output.content, "beta\n[truncated]");

    // Read a missing file: tool-level error.
    let output = fx.run("Read", json!({"path": "missing.txt"})).await;
    assert!(output.is_error);
    assert!(output.content.contains("cannot open"), "{}", output.content);

    // Edit: old string not found.
    let output = fx
        .run(
            "Edit",
            json!({"path": "notes/list.txt", "old_string": "zeta", "new_string": "x"}),
        )
        .await;
    assert!(output.is_error);
    assert!(output.content.contains("not found"), "{}", output.content);

    // Edit: ambiguous match without replace_all.
    let output = fx
        .run(
            "Edit",
            json!({"path": "notes/list.txt", "old_string": "alpha", "new_string": "x"}),
        )
        .await;
    assert!(output.is_error);
    assert!(
        output.content.contains("matches 2 times"),
        "{}",
        output.content
    );

    // Edit: unique match succeeds.
    let output = fx
        .run(
            "Edit",
            json!({"path": "notes/list.txt", "old_string": "beta", "new_string": "gamma"}),
        )
        .await;
    assert!(!output.is_error, "{}", output.content);
    assert_eq!(output.content, "Replaced 1 occurrence in notes/list.txt");

    // Edit: replace_all replaces every occurrence.
    let output = fx.run(
        "Edit",
        json!({"path": "notes/list.txt", "old_string": "alpha", "new_string": "delta", "replace_all": true}),
    )
    .await;
    assert_eq!(output.content, "Replaced 2 occurrences in notes/list.txt");
    assert_eq!(
        std::fs::read_to_string(fx.workspace().join("notes/list.txt")).unwrap(),
        "delta gamma delta\ntail\n"
    );

    // ListDir (helper op, not a tool): sorted entries.
    std::fs::write(fx.workspace().join("notes/apple.txt"), "1").unwrap();
    std::fs::create_dir(fx.workspace().join("notes/zoo")).unwrap();
    let payload = fx
        .session
        .request(HelperOp::ListDir {
            path: fx.workspace().join("notes").display().to_string(),
        })
        .await
        .unwrap();
    let HelperPayload::Dir { entries } = payload else {
        panic!("expected Dir payload");
    };
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["apple.txt", "list.txt", "zoo"]);
    assert!(entries[2].is_dir);

    fx.session.shutdown().await.unwrap();
}

#[tokio::test]
async fn bash_formats_streams_exit_codes_and_timeouts() {
    let fx = Fixture::new().await;

    // stdout only, exit 0.
    let output = fx.run("Bash", json!({"command": "echo hello"})).await;
    assert!(!output.is_error);
    assert_eq!(output.content, "hello");

    // stdout and stderr with a nonzero exit code.
    let output = fx
        .run("Bash", json!({"command": "echo out; echo err >&2; exit 3"}))
        .await;
    assert!(output.is_error);
    assert_eq!(output.content, "out\n--- stderr ---\nerr\n(exit code 3)");

    // The command runs in the workspace mount with HOME and TMPDIR in the
    // scratch space.
    let output = fx
        .run(
            "Bash",
            json!({"command": "printf '%s\\n%s\\n%s' \"$PWD\" \"$HOME\" \"$TMPDIR\""}),
        )
        .await;
    assert!(!output.is_error);
    let canonical_workspace = fx.workspace().canonicalize().unwrap();
    let lines: Vec<&str> = output.content.lines().collect();
    assert_eq!(
        Path::new(lines[0]).canonicalize().unwrap(),
        canonical_workspace
    );
    assert_eq!(lines[1], fx.scratch.home_dir().display().to_string());
    assert_eq!(lines[2], fx.scratch.tmp_dir().display().to_string());

    // Timeout: the command is killed and reported as timed out.
    let output = fx
        .run("Bash", json!({"command": "sleep 5", "timeout_ms": 200}))
        .await;
    assert!(output.is_error);
    assert_eq!(output.content, "(timed out)");

    // Output cap: more than 1 MiB of stdout is truncated.
    let command = "dd if=/dev/zero bs=1024 count=2048 2>/dev/null | tr '\\0' 'x'";
    let output = fx.run("Bash", json!({"command": command})).await;
    assert!(!output.is_error, "{}", output.content);
    assert!(output.content.contains("[output truncated]"));
    assert!(
        output.content.len() <= 1024 * 1024 + 64,
        "len {}",
        output.content.len()
    );

    fx.session.shutdown().await.unwrap();
}

#[tokio::test]
async fn concurrent_requests_complete_out_of_order() {
    let fx = Fixture::new().await;
    std::fs::write(fx.workspace().join("quick.txt"), "quick read").unwrap();

    // The Bash command polls for a flag file, so it cannot finish before
    // this test creates the flag; completion order is forced, not timed.
    // The Bash request is issued first, then the Read completes while it
    // is still in flight, then the Write releases it.
    let slow_call = ToolCall {
        id: "slow".into(),
        name: "Bash".into(),
        input: json!({
            "command": "until [ -e flag ]; do sleep 0.05; done; echo done",
            "timeout_ms": 60000
        }),
    };
    let (slow_result, ()) = tokio::join!(
        run_tool(&fx.session, fx.workspace(), &fx.scratch, &slow_call),
        async {
            let output = fx.run("Read", json!({"path": "quick.txt"})).await;
            assert_eq!(output.content, "quick read");
            let output = fx
                .run("Write", json!({"path": "flag", "content": "go"}))
                .await;
            assert!(!output.is_error, "{}", output.content);
        }
    );
    let slow_output = slow_result.unwrap();
    assert!(!slow_output.is_error, "{}", slow_output.content);
    assert_eq!(slow_output.content, "done");

    fx.session.shutdown().await.unwrap();
}

/// Minimal HTTP/1.1 origin on loopback answering every connection with a
/// fixed response.
async fn spawn_origin(response: &'static str) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let n = match stream.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => n,
                    };
                    request.extend_from_slice(&chunk[..n]);
                    if request.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    addr
}

#[tokio::test]
async fn web_fetch_formats_status_and_body() {
    let fx = Fixture::new().await;
    let addr = spawn_origin(
        "HTTP/1.1 200 OK\r\nContent-Length: 13\r\nConnection: close\r\n\r\nfetched bytes",
    )
    .await;

    let output = fx
        .run("WebFetch", json!({"url": format!("http://{addr}/page")}))
        .await;
    assert!(!output.is_error, "{}", output.content);
    assert_eq!(output.content, "HTTP 200\nfetched bytes");

    // max_bytes truncates the body and adds a note.
    let output = fx
        .run(
            "WebFetch",
            json!({"url": format!("http://{addr}/page"), "max_bytes": 7}),
        )
        .await;
    assert!(!output.is_error);
    assert_eq!(output.content, "HTTP 200\nfetched\n[truncated]");

    fx.session.shutdown().await.unwrap();
}

#[tokio::test]
async fn web_fetch_error_status_is_a_tool_error() {
    let fx = Fixture::new().await;
    let addr = spawn_origin(
        "HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\nConnection: close\r\n\r\nnot found",
    )
    .await;

    let output = fx
        .run("WebFetch", json!({"url": format!("http://{addr}/missing")}))
        .await;
    assert!(output.is_error);
    assert_eq!(output.content, "HTTP 404\nnot found");

    fx.session.shutdown().await.unwrap();
}
