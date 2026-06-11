//! The traits each implementation crate provides and the harness consumes.

use std::net::SocketAddr;
use std::path::PathBuf;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::conversation::{AgentId, CompletionRequest, CompletionResponse};
use crate::cost::{QuotaConfig, UsageSnapshot};
use crate::error::{FrontendError, LlmError, ProxyError, SandboxError};
use crate::event::EventBus;
use crate::sandbox::AccessReport;
use crate::tool::{ToolCall, ToolDef, ToolOutput};

/// One conversation-completing model backend. Implementations must be safe
/// to call concurrently (subagents run in parallel).
#[async_trait]
pub trait LlmBackend: Send + Sync {
    /// Stable identifier, e.g. "anthropic:claude-sonnet-4-6".
    fn id(&self) -> String;

    async fn complete(&self, request: &CompletionRequest) -> Result<CompletionResponse, LlmError>;

    /// Tokens and dollars consumed so far.
    fn usage(&self) -> UsageSnapshot;

    fn quota(&self) -> QuotaConfig;
}

/// A running sandbox. Created by `silo-sandbox`, given the attached
/// workspace mount and scratch space, and connected to a helper process
/// inside the sandbox that executes the tools.
#[async_trait]
pub trait Sandbox: Send + Sync {
    fn kind(&self) -> &'static str;

    /// Launches the sandbox and helper process. Must be called before
    /// `run_tool`.
    async fn start(&mut self) -> Result<(), SandboxError>;

    /// The tools this sandbox contributes: Read, Write, Edit, Bash,
    /// WebFetch, WebSearch.
    fn tool_defs(&self) -> Vec<ToolDef>;

    /// Executes one tool call inside the sandbox via the helper process.
    async fn run_tool(&self, agent: &AgentId, call: &ToolCall) -> Result<ToolOutput, SandboxError>;

    /// Called by the harness when an interrupt begins. Cancels in-flight
    /// tool executions so blocked `run_tool` calls return promptly with
    /// their partial output. The default does nothing.
    async fn interrupt(&self) -> Result<(), SandboxError> {
        Ok(())
    }

    /// What the sandboxed LLM can reach, for user inspection.
    fn access_report(&self) -> AccessReport;

    /// Runs an interactive user shell (or an arbitrary command) under the
    /// same sandbox policy as the LLM's tools. Returns the exit status.
    async fn user_shell(&self, command: Option<Vec<String>>) -> Result<i32, SandboxError> {
        let _ = command;
        Err(SandboxError::Unavailable(
            "this sandbox backend does not support interactive user sessions".into(),
        ))
    }

    /// Terminates a running `user_shell` session: the shell's process
    /// group receives SIGTERM, then SIGKILL when it does not exit
    /// promptly, and the blocked `user_shell` call returns. Callers (for
    /// example the `silo shell` signal handler) use this to stop the
    /// session and unwind through their cleanup path. The default does
    /// nothing.
    async fn terminate_user_shell(&self) -> Result<(), SandboxError> {
        Ok(())
    }

    /// Terminates the helper, tears down the sandbox, and removes the
    /// scratch space.
    async fn shutdown(&mut self) -> Result<(), SandboxError>;
}

/// Address and trust material of a running egress proxy.
#[derive(Clone, Debug)]
pub struct ProxyHandle {
    /// HTTP proxy address the sandbox must route through.
    pub http_addr: SocketAddr,
    /// PEM of the per-session CA certificate (public part only); written
    /// into the sandbox so TLS clients inside can trust the proxy.
    pub ca_cert_pem: String,
    /// DNS proxy address, when the backend uses one (gVisor/VM sandboxes).
    pub dns_addr: Option<SocketAddr>,
}

/// Harness-controlled egress proxy: domain allowlist, intranet blocking,
/// TLS interception with an ephemeral CA, credential injection, traffic
/// logging.
#[async_trait]
pub trait EgressProxy: Send + Sync {
    async fn start(&mut self) -> Result<ProxyHandle, ProxyError>;

    fn handle(&self) -> Option<ProxyHandle>;

    async fn shutdown(&mut self) -> Result<(), ProxyError>;
}

/// Commands flowing from the frontend to the harness outside the normal
/// request/response flow.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum FrontendCommand {
    Shutdown {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    /// Abort the in-progress turn; the harness unwinds at its next
    /// checkpoint and returns to awaiting input.
    Interrupt,
}

/// Everything a frontend needs from the harness.
pub struct FrontendContext {
    pub harness_id: String,
    pub bus: EventBus,
    pub commands: mpsc::Sender<FrontendCommand>,
    pub access: AccessReport,
    /// State directory for run files, auth tokens, and client keys.
    pub state_dir: PathBuf,
    /// Workspace path string, for display.
    pub workspace: String,
    /// The sandbox read allowlist as configured for this harness, for the
    /// run file. The access report's `readable_paths` holds the expanded
    /// per-platform view instead.
    pub configured_read_allowlist: Vec<String>,
}

/// One harness has exactly one frontend. The harness calls
/// `next_user_input` whenever the top-level agent needs a user turn, and
/// `run_tool` for frontend-owned tools (AskUserQuestion, SendUserFile,
/// Exit).
#[async_trait]
pub trait Frontend: Send + Sync {
    fn kind(&self) -> &'static str;

    /// Tools this frontend contributes for the top-level agent.
    fn tool_defs(&self) -> Vec<ToolDef>;

    /// Starts servers/IO. Must be called once before anything else.
    async fn start(&mut self, ctx: FrontendContext) -> Result<(), FrontendError>;

    /// Blocks until user input is available. For the headless frontend the
    /// first call returns the command-line prompt (with the Exit-tool
    /// instruction appended) and later calls return the canned
    /// non-interactive reminder.
    async fn next_user_input(&self) -> Result<String, FrontendError>;

    /// Executes a frontend-owned tool. AskUserQuestion blocks until the
    /// first client answers. For SendUserFile the harness resolves the file
    /// content from the sandbox first and passes it in the call input as
    /// `content_b64`.
    async fn run_tool(&self, agent: &AgentId, call: &ToolCall)
        -> Result<ToolOutput, FrontendError>;

    /// Called by the harness when an interrupt begins. Cancels pending user
    /// interactions, resolving any blocked `run_tool` calls (open
    /// AskUserQuestion calls resolve as interrupted). The default does
    /// nothing.
    async fn interrupt(&self) -> Result<(), FrontendError> {
        Ok(())
    }

    /// Announces shutdown to clients and stops servers/IO.
    async fn shutdown(&mut self, message: Option<String>) -> Result<(), FrontendError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interrupt_command_wire_format() {
        let command = FrontendCommand::Interrupt;
        let value = serde_json::to_value(&command).unwrap();
        assert_eq!(value, serde_json::json!({"command": "interrupt"}));
        let parsed: FrontendCommand = serde_json::from_value(value).unwrap();
        assert_eq!(parsed, command);
    }
}
