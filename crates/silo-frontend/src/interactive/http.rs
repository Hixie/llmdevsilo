//! Plain-HTTP handling on the interactive TLS listener.
//!
//! After the TLS handshake the server reads the HTTP request head. A
//! WebSocket upgrade request is replayed into the WebSocket handshake
//! through [`PrefixedStream`]; any other request gets a single landing
//! page response, which lets a browser record the certificate exception
//! before the web client connects.

use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf};

/// Maximum size of an HTTP request head.
const HEAD_LIMIT: usize = 8 * 1024;

/// An `AsyncRead + AsyncWrite` stream that first yields a buffered prefix,
/// then reads from the inner stream. Writes go straight to the inner
/// stream.
pub(crate) struct PrefixedStream<S> {
    prefix: Vec<u8>,
    pos: usize,
    inner: S,
}

impl<S> PrefixedStream<S> {
    pub(crate) fn new(prefix: Vec<u8>, inner: S) -> Self {
        PrefixedStream {
            prefix,
            pos: 0,
            inner,
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for PrefixedStream<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = &mut *self;
        if this.pos < this.prefix.len() {
            let remaining = &this.prefix[this.pos..];
            let n = remaining.len().min(buf.remaining());
            buf.put_slice(&remaining[..n]);
            this.pos += n;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for PrefixedStream<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// Reads from `stream` until the end of an HTTP request head (the first
/// `\r\n\r\n`). Returns everything read so far, which may extend past the
/// head. Returns `None` on read errors, on end of stream, and when no head
/// terminator appears within [`HEAD_LIMIT`] bytes.
pub(crate) async fn read_request_head<S: AsyncRead + Unpin>(stream: &mut S) -> Option<Vec<u8>> {
    let mut head = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        // The terminator may straddle the chunk boundary, so the search
        // restarts up to three bytes before the new data.
        let search_from = head.len().saturating_sub(3);
        head.extend_from_slice(&chunk[..n]);
        if head[search_from..]
            .windows(4)
            .any(|window| window == b"\r\n\r\n")
        {
            return Some(head);
        }
        if head.len() >= HEAD_LIMIT {
            return None;
        }
    }
}

/// Reports whether the request head carries an `Upgrade` header whose
/// comma-separated token list contains `websocket` (case-insensitive).
pub(crate) fn is_websocket_upgrade(head: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(head) else {
        return false;
    };
    for line in text.split("\r\n").skip(1) {
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("upgrade")
            && value
                .split(',')
                .any(|token| token.trim().eq_ignore_ascii_case("websocket"))
        {
            return true;
        }
    }
    false
}

/// Extracts the `Host` header value from a request head.
fn host_header(head: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(head).ok()?;
    for line in text.split("\r\n").skip(1) {
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("host") {
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

fn html_escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            _ => escaped.push(c),
        }
    }
    escaped
}

/// Builds the complete HTTP/1.1 landing page response for a non-WebSocket
/// request. The WebSocket URL uses the request's `Host` header, falling
/// back to the listen address.
pub(crate) fn landing_page_response(
    harness_id: &str,
    head: &[u8],
    listen_addr: SocketAddr,
) -> String {
    let host = host_header(head).unwrap_or_else(|| listen_addr.to_string());
    let id = html_escape(harness_id);
    let ws_url = html_escape(&format!("wss://{host}"));
    let body = format!(
        "<!DOCTYPE html>\n\
         <html lang=\"en\">\n\
         <head><meta charset=\"utf-8\"><title>Silo harness {id}</title></head>\n\
         <body>\n\
         <h1>Silo harness {id}</h1>\n\
         <p>This harness's certificate is now trusted by this browser, so the\n\
         web client can connect.</p>\n\
         <p>Harness id: <code>{id}</code></p>\n\
         <p>WebSocket URL: <code>{ws_url}</code></p>\n\
         </body>\n\
         </html>\n"
    );
    format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        body.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn prefixed_stream_yields_the_prefix_then_the_inner_stream() {
        let inner: &[u8] = b" world";
        let mut stream = PrefixedStream::new(b"hello".to_vec(), inner);
        let mut out = Vec::new();
        stream.read_to_end(&mut out).await.unwrap();
        assert_eq!(out, b"hello world");
    }

    #[tokio::test]
    async fn prefixed_stream_handles_small_read_buffers() {
        let inner: &[u8] = b"cd";
        let mut stream = PrefixedStream::new(b"ab".to_vec(), inner);
        let mut out = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            let n = stream.read(&mut byte).await.unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&byte[..n]);
        }
        assert_eq!(out, b"abcd");
    }

    #[tokio::test]
    async fn prefixed_stream_with_an_empty_prefix_reads_the_inner_stream() {
        let inner: &[u8] = b"data";
        let mut stream = PrefixedStream::new(Vec::new(), inner);
        let mut out = Vec::new();
        stream.read_to_end(&mut out).await.unwrap();
        assert_eq!(out, b"data");
    }

    #[tokio::test]
    async fn prefixed_stream_writes_pass_through_to_the_inner_stream() {
        use tokio::io::AsyncWriteExt;
        let mut stream = PrefixedStream::new(b"unread prefix".to_vec(), Vec::new());
        stream.write_all(b"payload").await.unwrap();
        stream.flush().await.unwrap();
        assert_eq!(stream.inner, b"payload");
    }

    #[tokio::test]
    async fn request_head_is_read_up_to_the_blank_line() {
        let request: &[u8] = b"GET / HTTP/1.1\r\nHost: x\r\n\r\nrest";
        let mut cursor = request;
        let head = read_request_head(&mut cursor).await.unwrap();
        assert_eq!(head, request);
    }

    #[tokio::test]
    async fn request_head_without_a_terminator_is_rejected() {
        let mut truncated: &[u8] = b"GET / HTTP/1.1\r\nHost: x\r\n";
        assert!(read_request_head(&mut truncated).await.is_none());

        let garbage = vec![b'x'; HEAD_LIMIT + 1];
        let mut garbage = garbage.as_slice();
        assert!(read_request_head(&mut garbage).await.is_none());
    }

    #[tokio::test]
    async fn request_head_terminator_straddling_reads_is_found() {
        // A reader that yields one byte at a time forces the terminator
        // across chunk boundaries.
        struct OneByte<'a>(&'a [u8]);
        impl AsyncRead for OneByte<'_> {
            fn poll_read(
                mut self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
                buf: &mut ReadBuf<'_>,
            ) -> Poll<std::io::Result<()>> {
                if let Some((first, rest)) = self.0.split_first() {
                    buf.put_slice(&[*first]);
                    self.0 = rest;
                }
                Poll::Ready(Ok(()))
            }
        }
        let request = b"GET / HTTP/1.1\r\nHost: x\r\n\r\n";
        let mut reader = OneByte(request);
        let head = read_request_head(&mut reader).await.unwrap();
        assert_eq!(head, request);
    }

    #[test]
    fn websocket_upgrades_are_detected_case_insensitively() {
        assert!(is_websocket_upgrade(
            b"GET / HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\n\r\n"
        ));
        assert!(is_websocket_upgrade(
            b"GET / HTTP/1.1\r\nUPGRADE: WebSocket\r\n\r\n"
        ));
        assert!(is_websocket_upgrade(
            b"GET / HTTP/1.1\r\nUpgrade: h2c, websocket\r\n\r\n"
        ));
        assert!(!is_websocket_upgrade(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n"));
        assert!(!is_websocket_upgrade(
            b"GET / HTTP/1.1\r\nUpgrade: h2c\r\n\r\n"
        ));
        // The request line is not a header.
        assert!(!is_websocket_upgrade(
            b"GET /upgrade:websocket HTTP/1.1\r\n\r\n"
        ));
        assert!(!is_websocket_upgrade(&[0xff, 0xfe, 0xfd]));
    }

    #[test]
    fn landing_page_uses_the_host_header_and_escapes_it() {
        let listen: SocketAddr = "127.0.0.1:4444".parse().unwrap();
        let response = landing_page_response(
            "harness1",
            b"GET / HTTP/1.1\r\nHost: example.test:9000\r\n\r\n",
            listen,
        );
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(response.contains("Content-Type: text/html; charset=utf-8\r\n"));
        assert!(response.contains("Connection: close\r\n"));
        assert!(response.contains("<title>Silo harness harness1</title>"));
        assert!(response.contains("wss://example.test:9000"));

        let body = response.split("\r\n\r\n").nth(1).unwrap();
        let length: usize = response
            .split("\r\n")
            .find_map(|line| line.strip_prefix("Content-Length: "))
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(body.len(), length);

        let fallback = landing_page_response("harness1", b"POST / HTTP/1.1\r\n\r\n", listen);
        assert!(fallback.contains("wss://127.0.0.1:4444"));

        let hostile = landing_page_response(
            "harness1",
            b"GET / HTTP/1.1\r\nHost: <script>alert(1)</script>\r\n\r\n",
            listen,
        );
        assert!(!hostile.contains("<script>"));
        assert!(hostile.contains("&lt;script&gt;"));
    }
}
