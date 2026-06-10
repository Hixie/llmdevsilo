use thiserror::Error;

#[derive(Debug, Error)]
pub enum LlmError {
    #[error("llm api error: {0}")]
    Api(String),
    #[error("llm transport error: {0}")]
    Transport(String),
    #[error("llm configuration error: {0}")]
    Config(String),
    #[error("llm quota exceeded: {0}")]
    QuotaExceeded(String),
    #[error("llm response malformed: {0}")]
    Malformed(String),
    #[error("llm script mismatch: {0}")]
    ScriptMismatch(String),
}

#[derive(Debug, Error)]
pub enum SandboxError {
    #[error("sandbox unavailable: {0}")]
    Unavailable(String),
    #[error("sandbox setup failed: {0}")]
    Setup(String),
    #[error("sandbox helper error: {0}")]
    Helper(String),
    #[error("sandbox rejected request: {0}")]
    Rejected(String),
    #[error("sandbox io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("sandbox script mismatch: {0}")]
    ScriptMismatch(String),
}

#[derive(Debug, Error)]
pub enum ProxyError {
    #[error("proxy setup failed: {0}")]
    Setup(String),
    #[error("proxy tls error: {0}")]
    Tls(String),
    #[error("proxy io error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Error)]
pub enum FrontendError {
    #[error("frontend setup failed: {0}")]
    Setup(String),
    #[error("frontend authentication error: {0}")]
    Auth(String),
    #[error("frontend closed: {0}")]
    Closed(String),
    #[error("frontend io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("frontend script mismatch: {0}")]
    ScriptMismatch(String),
}

#[derive(Debug, Error)]
pub enum WorkspaceError {
    #[error("workspace is locked: {0}")]
    Locked(String),
    #[error("workspace is not locked: {0}")]
    NotLocked(String),
    #[error("workspace setup failed: {0}")]
    Setup(String),
    #[error("workspace is damaged: {0}")]
    Damaged(String),
    #[error("workspace io error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Error)]
pub enum HarnessError {
    #[error(transparent)]
    Llm(#[from] LlmError),
    #[error(transparent)]
    Sandbox(#[from] SandboxError),
    #[error(transparent)]
    Proxy(#[from] ProxyError),
    #[error(transparent)]
    Frontend(#[from] FrontendError),
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
    #[error("configuration error: {0}")]
    Config(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}
