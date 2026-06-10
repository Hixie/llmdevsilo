//! Protocol-level tests for the helper serve loop, using an in-process
//! duplex stream and raw JSON-line frames.

use silo_core::helper::{
    b64, read_json_line, unb64, write_json_line, HelperOp, HelperPayload, HelperRequest,
    HelperResponse,
};
use tokio::io::{AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf};

struct RawClient {
    reader: BufReader<ReadHalf<DuplexStream>>,
    writer: WriteHalf<DuplexStream>,
}

impl RawClient {
    fn start() -> RawClient {
        let (client_side, server_side) = tokio::io::duplex(1 << 20);
        tokio::spawn(async move {
            let _ = silo_helper::serve_stream_with_config(
                server_side,
                silo_helper::FetchConfig::default(),
            )
            .await;
        });
        let (reader, writer) = tokio::io::split(client_side);
        RawClient {
            reader: BufReader::new(reader),
            writer,
        }
    }

    async fn send(&mut self, id: u64, op: HelperOp) {
        write_json_line(&mut self.writer, &HelperRequest { id, op })
            .await
            .unwrap();
    }

    async fn send_raw(&mut self, line: &str) {
        self.writer.write_all(line.as_bytes()).await.unwrap();
        self.writer.flush().await.unwrap();
    }

    async fn recv(&mut self) -> HelperResponse {
        read_json_line(&mut self.reader).await.unwrap().unwrap()
    }

    async fn recv_eof(&mut self) -> bool {
        read_json_line::<_, HelperResponse>(&mut self.reader)
            .await
            .unwrap()
            .is_none()
    }
}

#[tokio::test]
async fn hello_reports_version_and_pid() {
    let mut client = RawClient::start();
    client.send(1, HelperOp::Hello).await;
    let response = client.recv().await;
    assert_eq!(response.id, 1);
    match response.result.unwrap() {
        HelperPayload::Hello { version, pid } => {
            assert_eq!(version, env!("CARGO_PKG_VERSION"));
            assert_eq!(pid, std::process::id());
        }
        other => panic!("expected Hello payload, got {other:?}"),
    }
}

#[tokio::test]
async fn malformed_input_does_not_kill_the_serve_loop() {
    let mut client = RawClient::start();

    // Not JSON at all: skipped silently.
    client.send_raw("this is not json\n").await;
    // JSON, has an id, but not a valid request: answered with an error.
    client.send_raw("{\"id\": 9, \"op\": \"exec\"}\n").await;
    let response = client.recv().await;
    assert_eq!(response.id, 9);
    let err = response.result.unwrap_err();
    assert!(err.contains("malformed request"), "unexpected error: {err}");
    // JSON without an id: skipped silently.
    client.send_raw("{\"op\": \"hello\"}\n").await;

    // The loop is still alive and serves valid requests.
    client.send(10, HelperOp::Hello).await;
    let response = client.recv().await;
    assert_eq!(response.id, 10);
    assert!(response.result.is_ok());
}

#[tokio::test]
async fn per_request_errors_keep_the_loop_alive() {
    let dir = tempfile::tempdir().unwrap();
    let mut client = RawClient::start();

    client
        .send(
            1,
            HelperOp::WriteFile {
                path: dir.path().join("f.txt").display().to_string(),
                content_b64: "!!! not base64 !!!".into(),
                append: false,
            },
        )
        .await;
    let response = client.recv().await;
    assert_eq!(response.id, 1);
    assert!(response.result.unwrap_err().contains("base64"));

    client
        .send(
            2,
            HelperOp::ReadFile {
                path: "/nonexistent/missing.txt".into(),
                offset: None,
                limit: None,
            },
        )
        .await;
    let response = client.recv().await;
    assert_eq!(response.id, 2);
    assert!(response.result.is_err());

    client.send(3, HelperOp::Hello).await;
    assert!(client.recv().await.result.is_ok());
}

#[tokio::test]
async fn write_then_read_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nested/dir/file.txt").display().to_string();
    let mut client = RawClient::start();

    client
        .send(
            1,
            HelperOp::WriteFile {
                path: path.clone(),
                content_b64: b64(b"helper roundtrip"),
                append: false,
            },
        )
        .await;
    let response = client.recv().await;
    assert_eq!(
        response.result.unwrap(),
        HelperPayload::Written { bytes: 16 }
    );

    client
        .send(
            2,
            HelperOp::ReadFile {
                path,
                offset: None,
                limit: None,
            },
        )
        .await;
    match client.recv().await.result.unwrap() {
        HelperPayload::File {
            content_b64,
            truncated,
        } => {
            assert_eq!(unb64(&content_b64).unwrap(), b"helper roundtrip");
            assert!(!truncated);
        }
        other => panic!("expected File payload, got {other:?}"),
    }
}

#[tokio::test]
async fn cancel_ends_a_slow_exec_promptly() {
    let mut client = RawClient::start();
    client
        .send(
            1,
            HelperOp::Exec {
                command: "sleep 30".into(),
                cwd: None,
                env: vec![],
                timeout_ms: 120_000,
            },
        )
        .await;
    client.send(2, HelperOp::Cancel { id: 1 }).await;

    let started = std::time::Instant::now();
    let mut exec_response = None;
    let mut cancel_response = None;
    while exec_response.is_none() || cancel_response.is_none() {
        let response = client.recv().await;
        match response.id {
            1 => exec_response = Some(response),
            2 => cancel_response = Some(response),
            other => panic!("unexpected response id {other}"),
        }
    }
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "cancel did not end the exec promptly"
    );
    match exec_response.unwrap().result.unwrap() {
        HelperPayload::Exec {
            exit_code,
            timed_out,
            cancelled,
            ..
        } => {
            assert_eq!(exit_code, -1);
            assert!(cancelled);
            assert!(!timed_out);
        }
        other => panic!("expected Exec payload, got {other:?}"),
    }
    assert_eq!(cancel_response.unwrap().result.unwrap(), HelperPayload::Ack);
}

#[tokio::test]
async fn cancel_of_an_unknown_id_answers_without_killing_the_session() {
    let mut client = RawClient::start();
    client.send(1, HelperOp::Cancel { id: 999 }).await;
    let response = client.recv().await;
    assert_eq!(response.id, 1);
    let err = response.result.unwrap_err();
    assert!(err.contains("999"), "unexpected error: {err}");

    // The serve loop is still alive.
    client.send(2, HelperOp::Hello).await;
    let response = client.recv().await;
    assert_eq!(response.id, 2);
    assert!(response.result.is_ok());
}

#[tokio::test]
async fn shutdown_acks_then_ends_the_stream() {
    let mut client = RawClient::start();
    client.send(7, HelperOp::Shutdown).await;
    let response = client.recv().await;
    assert_eq!(response.id, 7);
    assert_eq!(response.result.unwrap(), HelperPayload::Ack);
    assert!(client.recv_eof().await);
}
