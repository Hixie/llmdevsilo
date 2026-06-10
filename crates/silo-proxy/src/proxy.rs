//! The intercepting HTTP proxy.
//!
//! The proxy listens on a loopback TCP port. The sandbox routes all traffic
//! through it using the standard `HTTP_PROXY`/`HTTPS_PROXY` convention, so it
//! sees two request shapes:
//!
//! - `CONNECT host:port`: the proxy applies the allowlist, replies
//!   `200 Connection Established`, performs a TLS server handshake using a
//!   leaf certificate minted from the session CA, and serves the decrypted
//!   stream as HTTP/1.1, forwarding each inner request upstream over TLS.
//! - Absolute-form plain HTTP (`GET http://host/path`): the proxy applies the
//!   same policy and forwards the request upstream as plain HTTP.
//!
//! Every inner request and every blocked attempt produces one
//! [`NetworkRecord`].

use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use rustls::ServerConfig;
use silo_core::config::ProxySettings;
use silo_core::error::ProxyError;
use silo_core::journal::{JournalEntry, JournalHandle, NetworkRecord};
use silo_core::traits::{EgressProxy, ProxyHandle};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_rustls::TlsAcceptor;

use crate::allowlist::DomainAllowlist;
use crate::ca::SessionCa;
use crate::credentials::CredentialStore;
use crate::dns::{DnsFilter, SystemResolver};
use crate::ipguard::IpGuard;
use crate::upstream::{UpstreamConnector, UpstreamError};
use crate::ProxyBuilder;

/// Shared per-session state used by every accepted connection.
struct ProxyState {
    allowlist: DomainAllowlist,
    credentials: CredentialStore,
    ca: Arc<SessionCa>,
    upstream: UpstreamConnector,
    journal: JournalHandle,
}

impl ProxyState {
    fn journal_block(&self, host: &str, port: u16, note: &str) {
        self.journal.append(JournalEntry::Network {
            record: NetworkRecord {
                host: host.to_string(),
                port,
                allowed: false,
                note: Some(note.to_string()),
                ..NetworkRecord::default()
            },
        });
    }
}

/// rustls certificate resolver that mints a leaf for the requested server
/// name from the session CA on demand.
struct CaResolver {
    ca: Arc<SessionCa>,
}

impl std::fmt::Debug for CaResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CaResolver")
    }
}

impl ResolvesServerCert for CaResolver {
    fn resolve(&self, client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let host = client_hello.server_name()?.to_string();
        self.ca.leaf_for(&host).ok()
    }
}

/// Running HTTP proxy. Built via [`ProxyBuilder`].
pub struct HttpProxy {
    settings: ProxySettings,
    journal: JournalHandle,
    port: u16,
    dns_enabled: bool,
    dns_port: u16,
    resolver_overrides: std::collections::HashMap<String, SocketAddr>,
    extra_upstream_root_cas: Vec<String>,
    allow_loopback_upstream: bool,
    handle: Option<ProxyHandle>,
    shutdown: Option<oneshot::Sender<()>>,
    tasks: Vec<JoinHandle<()>>,
}

impl HttpProxy {
    pub(crate) fn new(builder: ProxyBuilder) -> Self {
        HttpProxy {
            settings: builder.settings,
            journal: builder.journal,
            port: builder.port,
            dns_enabled: builder.dns_enabled,
            dns_port: builder.dns_port,
            resolver_overrides: builder.resolver_overrides,
            extra_upstream_root_cas: builder.extra_upstream_root_cas,
            allow_loopback_upstream: builder.allow_loopback_upstream,
            handle: None,
            shutdown: None,
            tasks: Vec::new(),
        }
    }
}

#[async_trait]
impl EgressProxy for HttpProxy {
    async fn start(&mut self) -> Result<ProxyHandle, ProxyError> {
        let allowlist = DomainAllowlist::new(&self.settings.allowed_domains);
        let credentials = CredentialStore::from_settings(&self.settings)?;
        let ca = Arc::new(SessionCa::generate()?);
        let ca_cert_pem = ca.ca_cert_pem().to_string();
        let guard = IpGuard::with_loopback_allowed(self.allow_loopback_upstream);
        let upstream = UpstreamConnector::new(
            guard,
            &self.extra_upstream_root_cas,
            self.resolver_overrides.clone(),
        )
        .map_err(ProxyError::Tls)?;

        let state = Arc::new(ProxyState {
            allowlist: allowlist.clone(),
            credentials,
            ca,
            upstream,
            journal: self.journal.clone(),
        });

        let listener = TcpListener::bind(("127.0.0.1", self.port)).await?;
        let http_addr = listener.local_addr()?;

        let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();
        let accept_state = state.clone();
        let accept_task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => break,
                    accepted = listener.accept() => {
                        let Ok((stream, _peer)) = accepted else { continue };
                        let conn_state = accept_state.clone();
                        tokio::spawn(async move {
                            if let Err(e) = serve_connection(conn_state, stream).await {
                                tracing::debug!("proxy connection ended: {e}");
                            }
                        });
                    }
                }
            }
        });
        self.tasks.push(accept_task);
        self.shutdown = Some(shutdown_tx);

        let dns_addr = if self.dns_enabled {
            let dns_guard = IpGuard::with_loopback_allowed(self.allow_loopback_upstream);
            let filter = Arc::new(DnsFilter::new(
                allowlist,
                dns_guard,
                Arc::new(SystemResolver),
                self.journal.clone(),
            ));
            let listen: SocketAddr = SocketAddr::from(([127, 0, 0, 1], self.dns_port));
            let (addr, dns_task) = filter.serve(listen).await?;
            self.tasks.push(dns_task);
            Some(addr)
        } else {
            None
        };

        let handle = ProxyHandle {
            http_addr,
            ca_cert_pem,
            dns_addr,
        };
        self.handle = Some(handle.clone());
        self.journal.append(JournalEntry::Lifecycle {
            message: format!("egress proxy listening on {http_addr}"),
        });
        Ok(handle)
    }

    fn handle(&self) -> Option<ProxyHandle> {
        self.handle.clone()
    }

    async fn shutdown(&mut self) -> Result<(), ProxyError> {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        for task in self.tasks.drain(..) {
            task.abort();
        }
        Ok(())
    }
}

/// Reads the first request line, dispatches CONNECT versus absolute-form.
async fn serve_connection(
    state: Arc<ProxyState>,
    mut stream: tokio::net::TcpStream,
) -> Result<(), ProxyError> {
    let head = read_request_head(&mut stream).await?;
    let Some((method, target)) = parse_request_line(&head) else {
        return Ok(());
    };
    if method.eq_ignore_ascii_case("CONNECT") {
        serve_connect(state, stream, &target).await
    } else {
        serve_plain(state, stream, head).await
    }
}

/// Reads bytes until the end of the HTTP header block (`\r\n\r\n`).
async fn read_request_head<R>(reader: &mut R) -> Result<Vec<u8>, ProxyError>
where
    R: AsyncReadExt + Unpin,
{
    let mut buf = Vec::with_capacity(1024);
    let mut byte = [0u8; 1];
    loop {
        let n = reader.read(&mut byte).await?;
        if n == 0 {
            break;
        }
        buf.push(byte[0]);
        if buf.len() >= 4 && &buf[buf.len() - 4..] == b"\r\n\r\n" {
            break;
        }
        if buf.len() > 64 * 1024 {
            return Err(ProxyError::Setup("request head too large".into()));
        }
    }
    Ok(buf)
}

/// Returns the method and request target from the first line.
fn parse_request_line(head: &[u8]) -> Option<(String, String)> {
    let text = std::str::from_utf8(head).ok()?;
    let line = text.lines().next()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();
    Some((method, target))
}

/// Splits `host:port` into host and port, defaulting the port to 443.
fn split_host_port(target: &str, default_port: u16) -> (String, u16) {
    if let Some(stripped) = target.strip_prefix('[') {
        // Bracketed IPv6 literal, e.g. [::1]:443.
        if let Some((host, rest)) = stripped.split_once(']') {
            let port = rest
                .strip_prefix(':')
                .and_then(|p| p.parse().ok())
                .unwrap_or(default_port);
            return (host.to_string(), port);
        }
    }
    match target.rsplit_once(':') {
        Some((host, port)) => (host.to_string(), port.parse().unwrap_or(default_port)),
        None => (target.to_string(), default_port),
    }
}

/// Handles a CONNECT tunnel: allowlist check, 200, TLS server handshake, then
/// HTTP/1.1 over the decrypted stream.
async fn serve_connect(
    state: Arc<ProxyState>,
    mut stream: tokio::net::TcpStream,
    target: &str,
) -> Result<(), ProxyError> {
    let (host, port) = split_host_port(target, 443);

    if let Err(note) = host_policy(&state, &host) {
        state.journal_block(&host, port, note);
        write_status(&mut stream, 403, "Forbidden").await?;
        return Ok(());
    }

    write_status(&mut stream, 200, "Connection Established").await?;

    let resolver = Arc::new(CaResolver {
        ca: state.ca.clone(),
    });
    let server_config =
        ServerConfig::builder_with_provider(rustls::crypto::ring::default_provider().into())
            .with_safe_default_protocol_versions()
            .map_err(|e| ProxyError::Tls(e.to_string()))?
            .with_no_client_auth()
            .with_cert_resolver(resolver);
    let acceptor = TlsAcceptor::from(Arc::new(server_config));
    let tls_stream = match acceptor.accept(stream).await {
        Ok(stream) => stream,
        Err(e) => {
            // The peer that failed the handshake owns the stream now; the
            // failure cannot be reported to it.
            state.journal_block(&host, port, "tls handshake failed");
            tracing::debug!("tls handshake with sandbox failed for {host}: {e}");
            return Ok(());
        }
    };

    serve_inner_http(state, TokioIo::new(tls_stream), host, port, true).await
}

/// Handles an absolute-form plain HTTP request and forwards it upstream. The
/// already-consumed request head is replayed into the inner HTTP server so
/// hyper parses the full request.
async fn serve_plain(
    state: Arc<ProxyState>,
    mut stream: tokio::net::TcpStream,
    head: Vec<u8>,
) -> Result<(), ProxyError> {
    let Some((_method, target)) = parse_request_line(&head) else {
        return Ok(());
    };
    let Some(rest) = target.strip_prefix("http://") else {
        // Only absolute-form plain HTTP is understood here.
        write_status(&mut stream, 400, "Bad Request").await?;
        return Ok(());
    };
    let authority = rest.split('/').next().unwrap_or(rest).to_string();
    let (host, port) = split_host_port(&authority, 80);

    if let Err(note) = host_policy(&state, &host) {
        state.journal_block(&host, port, note);
        write_status(&mut stream, 403, "Forbidden").await?;
        return Ok(());
    }

    let replayed = RewindStream::new(head, stream);
    serve_inner_http(state, TokioIo::new(replayed), host, port, false).await
}

/// An async stream that yields a prefix of bytes before reading from the
/// underlying stream. Used to replay an HTTP request head already consumed
/// from the socket.
struct RewindStream<S> {
    prefix: std::io::Cursor<Vec<u8>>,
    inner: S,
}

impl<S> RewindStream<S> {
    fn new(prefix: Vec<u8>, inner: S) -> Self {
        RewindStream {
            prefix: std::io::Cursor::new(prefix),
            inner,
        }
    }
}

impl<S: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for RewindStream<S> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let remaining = self.prefix.get_ref().len() as u64 - self.prefix.position();
        if remaining > 0 {
            let pos = self.prefix.position() as usize;
            let data = &self.prefix.get_ref()[pos..];
            let take = data.len().min(buf.remaining());
            buf.put_slice(&data[..take]);
            self.prefix.set_position((pos + take) as u64);
            return std::task::Poll::Ready(Ok(()));
        }
        std::pin::Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for RewindStream<S> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// Applies the connection-level policy to a CONNECT or absolute-form target
/// host. Returns the journaling note for a rejection, or `Ok(())` when the
/// host may proceed. Hosts with credentials configured must still be
/// allowlisted. An IP-literal host that the IP guard blocks is rejected here,
/// before any connection is attempted.
fn host_policy(state: &ProxyState, host: &str) -> Result<(), &'static str> {
    if !state.allowlist.allows(host) {
        return Err("domain not allowlisted");
    }
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        if state.upstream.guard().is_blocked(ip) {
            return Err("blocked address");
        }
    }
    Ok(())
}

/// Serves HTTP/1.1 over `io`, forwarding each inner request upstream. `secure`
/// selects TLS versus plain HTTP for the upstream connection.
async fn serve_inner_http<I>(
    state: Arc<ProxyState>,
    io: I,
    host: String,
    port: u16,
    secure: bool,
) -> Result<(), ProxyError>
where
    I: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
{
    let service = hyper::service::service_fn(move |req: Request<hyper::body::Incoming>| {
        let state = state.clone();
        let host = host.clone();
        async move {
            Ok::<_, std::convert::Infallible>(handle_inner(state, host, port, secure, req).await)
        }
    });
    if let Err(e) = hyper::server::conn::http1::Builder::new()
        .serve_connection(io, service)
        .with_upgrades()
        .await
    {
        tracing::debug!("inner http connection ended: {e}");
    }
    Ok(())
}

/// Processes one inner request: rejects upgrades, applies credential
/// injection, forwards upstream, journals, and returns the upstream response.
async fn handle_inner(
    state: Arc<ProxyState>,
    host: String,
    port: u16,
    secure: bool,
    req: Request<hyper::body::Incoming>,
) -> Response<Full<Bytes>> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();

    if is_upgrade(req.headers()) {
        state.journal.append(JournalEntry::Network {
            record: NetworkRecord {
                host: host.clone(),
                port,
                method: Some(method.to_string()),
                path: Some(path),
                allowed: false,
                note: Some("upgrade blocked".into()),
                ..NetworkRecord::default()
            },
        });
        return status_response(403, "upgrade blocked");
    }

    match forward_request(&state, &host, port, secure, req).await {
        Ok((response, sent, received, status, injected)) => {
            state.journal.append(JournalEntry::Network {
                record: NetworkRecord {
                    host: host.clone(),
                    port,
                    method: Some(method.to_string()),
                    path: Some(path),
                    status: Some(status),
                    bytes_sent: sent,
                    bytes_received: received,
                    allowed: true,
                    credential_injected: injected,
                    note: None,
                },
            });
            response
        }
        Err(err) => {
            let note = match &err {
                UpstreamError::Blocked(_) => "blocked address",
                UpstreamError::Resolve(_) => "resolve failed",
                UpstreamError::Connect(_) => "connect failed",
                UpstreamError::Tls(_) => "upstream tls failed",
            };
            let allowed = !matches!(err, UpstreamError::Blocked(_));
            state.journal.append(JournalEntry::Network {
                record: NetworkRecord {
                    host: host.clone(),
                    port,
                    method: Some(method.to_string()),
                    path: Some(path),
                    allowed,
                    note: Some(note.to_string()),
                    ..NetworkRecord::default()
                },
            });
            status_response(502, &format!("upstream error: {err}"))
        }
    }
}

type ForwardResult = Result<(Response<Full<Bytes>>, u64, u64, u16, bool), UpstreamError>;

/// Opens the upstream connection, rewrites headers, sends the request, and
/// reads the response. Returns the response plus byte counts, status, and
/// whether a credential was injected.
async fn forward_request(
    state: &Arc<ProxyState>,
    host: &str,
    port: u16,
    secure: bool,
    req: Request<hyper::body::Incoming>,
) -> ForwardResult {
    let (parts, body) = req.into_parts();
    let body_bytes = body
        .collect()
        .await
        .map(|c| c.to_bytes())
        .unwrap_or_default();
    let bytes_sent = body_bytes.len() as u64;

    let mut headers = parts.headers.clone();
    let injected = state.credentials.apply(host, &mut headers);

    let path_and_query = parts
        .uri
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| "/".to_string());

    let mut builder = Request::builder().method(parts.method).uri(path_and_query);
    if let Some(hmap) = builder.headers_mut() {
        *hmap = headers;
        hmap.insert(
            http::header::HOST,
            http::HeaderValue::from_str(host)
                .unwrap_or_else(|_| http::HeaderValue::from_static("")),
        );
    }
    let out_req = builder
        .body(Full::new(body_bytes))
        .map_err(|e| UpstreamError::Connect(e.to_string()))?;

    let (status, resp_headers, resp_body) = if secure {
        let tls = state.upstream.connect_tls(host, port).await?;
        send_over(TokioIo::new(tls), out_req).await?
    } else {
        let tcp = state.upstream.connect_tcp(host, port).await?;
        send_over(TokioIo::new(tcp), out_req).await?
    };

    let bytes_received = resp_body.len() as u64;
    let mut response = Response::builder().status(status);
    if let Some(hmap) = response.headers_mut() {
        *hmap = resp_headers;
    }
    let response = response
        .body(Full::new(resp_body))
        .map_err(|e| UpstreamError::Connect(e.to_string()))?;
    Ok((response, bytes_sent, bytes_received, status, injected))
}

/// Drives one HTTP/1.1 request/response exchange over an established
/// connection.
async fn send_over<I>(
    io: I,
    req: Request<Full<Bytes>>,
) -> Result<(u16, http::HeaderMap, Bytes), UpstreamError>
where
    I: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
{
    let (mut sender, conn) = hyper::client::conn::http1::handshake(io)
        .await
        .map_err(|e| UpstreamError::Connect(e.to_string()))?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let response = sender
        .send_request(req)
        .await
        .map_err(|e| UpstreamError::Connect(e.to_string()))?;
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let body = response
        .into_body()
        .collect()
        .await
        .map(|c| c.to_bytes())
        .map_err(|e| UpstreamError::Connect(e.to_string()))?;
    Ok((status, headers, body))
}

fn is_upgrade(headers: &http::HeaderMap) -> bool {
    headers.contains_key(http::header::UPGRADE)
        || headers
            .get(http::header::CONNECTION)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.to_ascii_lowercase().contains("upgrade"))
            .unwrap_or(false)
}

fn status_response(code: u16, message: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(code)
        .body(Full::new(Bytes::from(message.to_string())))
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::new())))
}

/// Writes a minimal HTTP/1.1 status line and terminating headers.
async fn write_status<W>(stream: &mut W, code: u16, reason: &str) -> Result<(), ProxyError>
where
    W: AsyncWriteExt + Unpin,
{
    let response = format!("HTTP/1.1 {code} {reason}\r\nContent-Length: 0\r\n\r\n");
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_request_line() {
        let (m, t) = parse_request_line(b"CONNECT example.com:443 HTTP/1.1\r\n\r\n").unwrap();
        assert_eq!(m, "CONNECT");
        assert_eq!(t, "example.com:443");
    }

    #[test]
    fn splits_host_and_port() {
        assert_eq!(
            split_host_port("example.com:443", 443),
            ("example.com".into(), 443)
        );
        assert_eq!(
            split_host_port("example.com", 80),
            ("example.com".into(), 80)
        );
        assert_eq!(split_host_port("[::1]:8443", 443), ("::1".into(), 8443));
    }

    #[test]
    fn detects_upgrade_requests() {
        let mut headers = http::HeaderMap::new();
        assert!(!is_upgrade(&headers));
        headers.insert(http::header::UPGRADE, "websocket".parse().unwrap());
        assert!(is_upgrade(&headers));

        let mut connection = http::HeaderMap::new();
        connection.insert(http::header::CONNECTION, "Upgrade".parse().unwrap());
        assert!(is_upgrade(&connection));
    }
}
