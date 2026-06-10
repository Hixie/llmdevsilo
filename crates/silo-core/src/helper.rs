//! Wire protocol between the harness (sandbox module) and the helper
//! process running inside the sandbox.
//!
//! The helper is untrusted code: it runs under the full sandbox policy and
//! implements the Read/Write/Edit/Bash/WebFetch/WebSearch tools, so every
//! tool execution is subject to the sandbox restrictions. Messages are JSON
//! Lines over a stream (a Unix socket or TCP loopback connection, depending
//! on the backend).

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HelperRequest {
    pub id: u64,
    #[serde(flatten)]
    pub op: HelperOp,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum HelperOp {
    /// First message in both directions; carries version information.
    Hello,
    Exec {
        command: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        #[serde(default)]
        env: Vec<(String, String)>,
        timeout_ms: u64,
    },
    ReadFile {
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        offset: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<u64>,
    },
    WriteFile {
        path: String,
        content_b64: String,
        #[serde(default)]
        append: bool,
    },
    EditFile {
        path: String,
        old: String,
        new: String,
        #[serde(default)]
        replace_all: bool,
    },
    ListDir {
        path: String,
    },
    /// HTTP request issued from inside the sandbox (and therefore through
    /// the egress proxy).
    Fetch {
        url: String,
        method: String,
        #[serde(default)]
        headers: Vec<(String, String)>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        body_b64: Option<String>,
        max_bytes: u64,
    },
    Shutdown,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HelperResponse {
    pub id: u64,
    pub result: Result<HelperPayload, String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "payload", rename_all = "snake_case")]
pub enum HelperPayload {
    Hello {
        version: String,
        pid: u32,
    },
    Exec {
        exit_code: i32,
        stdout: String,
        stderr: String,
        timed_out: bool,
        truncated: bool,
    },
    File {
        content_b64: String,
        truncated: bool,
    },
    Written {
        bytes: u64,
    },
    Edited {
        replacements: u64,
    },
    Dir {
        entries: Vec<DirEntry>,
    },
    Fetched {
        status: u16,
        headers: Vec<(String, String)>,
        body_b64: String,
        truncated: bool,
    },
    Ack,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

pub async fn write_json_line<W, T>(writer: &mut W, value: &T) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let mut line = serde_json::to_string(value)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    line.push('\n');
    writer.write_all(line.as_bytes()).await?;
    writer.flush().await
}

/// Reads one JSON value from a line. Returns `None` on clean end-of-stream.
pub async fn read_json_line<R, T>(reader: &mut R) -> std::io::Result<Option<T>>
where
    R: AsyncBufReadExt + Unpin,
    T: DeserializeOwned,
{
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Ok(None);
        }
        if line.trim().is_empty() {
            continue;
        }
        let value = serde_json::from_str(&line)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        return Ok(Some(value));
    }
}

pub fn b64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

pub fn unb64(s: &str) -> Result<Vec<u8>, String> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .map_err(|e| format!("invalid base64: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frames_roundtrip() {
        let request = HelperRequest {
            id: 7,
            op: HelperOp::Exec {
                command: "echo hello".into(),
                cwd: None,
                env: vec![],
                timeout_ms: 1000,
            },
        };
        let mut buffer = Vec::new();
        write_json_line(&mut buffer, &request).await.unwrap();
        let mut reader = tokio::io::BufReader::new(buffer.as_slice());
        let parsed: HelperRequest = read_json_line(&mut reader).await.unwrap().unwrap();
        assert_eq!(parsed, request);
        let eof: Option<HelperRequest> = read_json_line(&mut reader).await.unwrap();
        assert!(eof.is_none());
    }

    #[test]
    fn base64_roundtrip() {
        assert_eq!(unb64(&b64(b"data")).unwrap(), b"data");
    }
}
