//! Fetch tests against a local plain-HTTP origin on loopback. The serve
//! loop gets an explicit `FetchConfig` with no proxy, so no environment
//! variables are read or written.

use std::net::SocketAddr;

use silo_core::helper::{
    b64, read_json_line, unb64, write_json_line, HelperOp, HelperPayload, HelperRequest,
    HelperResponse,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

/// Minimal HTTP/1.1 origin: captures each raw request (headers plus body
/// per Content-Length) and answers with a fixed response.
async fn spawn_origin(response: &'static str) -> (SocketAddr, mpsc::Receiver<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::channel::<Vec<u8>>(8);
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut chunk = [0u8; 4096];
                let header_end = loop {
                    let n = match stream.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => n,
                    };
                    request.extend_from_slice(&chunk[..n]);
                    if let Some(pos) = find_header_end(&request) {
                        break pos;
                    }
                };
                let content_length = parse_content_length(&request[..header_end]);
                while request.len() < header_end + content_length {
                    let n = match stream.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    request.extend_from_slice(&chunk[..n]);
                }
                let _ = tx.send(request).await;
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    (addr, rx)
}

fn find_header_end(bytes: &[u8]) -> Option<usize> {
    bytes
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|pos| pos + 4)
}

fn parse_content_length(headers: &[u8]) -> usize {
    let text = String::from_utf8_lossy(headers);
    for line in text.lines() {
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                return value.trim().parse().unwrap_or(0);
            }
        }
    }
    0
}

struct Client {
    reader: BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
    writer: tokio::io::WriteHalf<tokio::io::DuplexStream>,
}

impl Client {
    fn start() -> Client {
        let (client_side, server_side) = tokio::io::duplex(1 << 20);
        let config = silo_helper::FetchConfig {
            proxy_url: None,
            ca_cert_path: None,
        };
        tokio::spawn(async move {
            let _ = silo_helper::serve_stream_with_config(server_side, config).await;
        });
        let (reader, writer) = tokio::io::split(client_side);
        Client {
            reader: BufReader::new(reader),
            writer,
        }
    }

    async fn fetch(&mut self, op: HelperOp) -> Result<HelperPayload, String> {
        write_json_line(&mut self.writer, &HelperRequest { id: 1, op })
            .await
            .unwrap();
        let response: HelperResponse = read_json_line(&mut self.reader).await.unwrap().unwrap();
        response.result
    }
}

fn fetched(payload: HelperPayload) -> (u16, Vec<(String, String)>, Vec<u8>, bool) {
    match payload {
        HelperPayload::Fetched {
            status,
            headers,
            body_b64,
            truncated,
        } => (status, headers, unb64(&body_b64).unwrap(), truncated),
        other => panic!("expected Fetched payload, got {other:?}"),
    }
}

#[tokio::test]
async fn fetch_forwards_method_headers_and_body() {
    let (addr, mut requests) = spawn_origin(
        "HTTP/1.1 201 Created\r\n\
         Content-Type: text/plain\r\n\
         X-Origin: test-origin\r\n\
         Content-Length: 12\r\n\
         Connection: close\r\n\
         \r\n\
         origin reply",
    )
    .await;

    let mut client = Client::start();
    let result = client
        .fetch(HelperOp::Fetch {
            url: format!("http://{addr}/echo"),
            method: "POST".into(),
            headers: vec![("x-probe".into(), "probe-value".into())],
            body_b64: Some(b64(b"request body bytes")),
            max_bytes: 1024,
        })
        .await
        .unwrap();

    let (status, headers, body, truncated) = fetched(result);
    assert_eq!(status, 201);
    assert!(headers
        .iter()
        .any(|(name, value)| name == "x-origin" && value == "test-origin"));
    assert_eq!(body, b"origin reply");
    assert!(!truncated);

    let raw = requests.recv().await.unwrap();
    let raw = String::from_utf8_lossy(&raw);
    assert!(
        raw.starts_with("POST /echo HTTP/1.1\r\n"),
        "request was: {raw}"
    );
    assert!(raw.contains("x-probe: probe-value"), "request was: {raw}");
    assert!(raw.ends_with("request body bytes"), "request was: {raw}");
}

#[tokio::test]
async fn fetch_truncates_body_at_max_bytes() {
    let (addr, _requests) = spawn_origin(
        "HTTP/1.1 200 OK\r\n\
         Content-Length: 26\r\n\
         Connection: close\r\n\
         \r\n\
         abcdefghijklmnopqrstuvwxyz",
    )
    .await;

    let mut client = Client::start();
    let result = client
        .fetch(HelperOp::Fetch {
            url: format!("http://{addr}/"),
            method: "GET".into(),
            headers: vec![],
            body_b64: None,
            max_bytes: 5,
        })
        .await
        .unwrap();

    let (status, _, body, truncated) = fetched(result);
    assert_eq!(status, 200);
    assert_eq!(body, b"abcde");
    assert!(truncated);
}

#[tokio::test]
async fn fetch_reports_connection_failure_per_request() {
    // Bind and drop a listener to get a port with nothing listening.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    let mut client = Client::start();
    let err = client
        .fetch(HelperOp::Fetch {
            url: format!("http://{addr}/"),
            method: "GET".into(),
            headers: vec![],
            body_b64: None,
            max_bytes: 1024,
        })
        .await
        .unwrap_err();
    assert!(err.contains("fetch failed"), "unexpected error: {err}");

    // The serve loop is still alive.
    let result = client.fetch(HelperOp::Hello).await;
    assert!(result.is_ok());
}
