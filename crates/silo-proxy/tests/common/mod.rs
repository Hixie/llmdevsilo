//! Shared test infrastructure: local TLS and plain HTTP origin servers that
//! echo the request method, path, and headers back as JSON.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;

use rcgen::{generate_simple_self_signed, CertifiedKey};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::ServerConfig;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

/// A running origin server.
pub struct Origin {
    pub addr: SocketAddr,
    /// PEM of the origin's self-signed certificate (used as an upstream trust
    /// root for the TLS variant).
    pub cert_pem: String,
}

/// Reads an HTTP/1.1 request from a stream and returns a JSON echo response
/// body. Returns the raw response bytes to write back.
fn build_echo_response(request: &str) -> Vec<u8> {
    let mut lines = request.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    let mut headers = BTreeMap::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    let body = serde_json::json!({
        "method": method,
        "path": path,
        "headers": headers,
    })
    .to_string();
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    response.into_bytes()
}

/// Reads request bytes up to the end of headers.
async fn read_head<S>(stream: &mut S) -> String
where
    S: AsyncReadExt + Unpin,
{
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                buf.push(byte[0]);
                if buf.len() >= 4 && &buf[buf.len() - 4..] == b"\r\n\r\n" {
                    break;
                }
            }
        }
    }
    String::from_utf8_lossy(&buf).to_string()
}

/// Starts a TLS origin presenting a self-signed certificate for the given
/// host names. Echoes each request as JSON. Returns the origin handle.
pub async fn start_tls_origin(sans: &[&str]) -> Origin {
    let names: Vec<String> = sans.iter().map(|s| s.to_string()).collect();
    let CertifiedKey { cert, key_pair } = generate_simple_self_signed(names).unwrap();
    let cert_pem = cert.pem();
    let cert_der = CertificateDer::from(cert.der().to_vec());
    let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_pair.serialize_der()));

    let config =
        ServerConfig::builder_with_provider(rustls::crypto::ring::default_provider().into())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der], key_der)
            .unwrap();
    let acceptor = TlsAcceptor::from(Arc::new(config));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(stream).await else {
                    return;
                };
                let request = read_head(&mut tls).await;
                let response = build_echo_response(&request);
                let _ = tls.write_all(&response).await;
                let _ = tls.flush().await;
                let _ = tls.shutdown().await;
            });
        }
    });

    Origin { addr, cert_pem }
}

/// Starts a plain HTTP origin. Echoes each request as JSON.
pub async fn start_plain_origin() -> Origin {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let request = read_head(&mut stream).await;
                let response = build_echo_response(&request);
                let _ = stream.write_all(&response).await;
                let _ = stream.flush().await;
            });
        }
    });
    Origin {
        addr,
        cert_pem: String::new(),
    }
}
