//! Scripted sandbox for tests and journal replay.
//!
//! The mock sandbox executes nothing: each tool call is checked against
//! the next scripted execution in the shared test script and the recorded
//! output is played back. Start and shutdown only journal lifecycle
//! notes; no scratch space, helper process, or filesystem access exists.

use async_trait::async_trait;
use silo_core::config::SandboxConfig;
use silo_core::conversation::AgentId;
use silo_core::error::SandboxError;
use silo_core::journal::{JournalEntry, JournalHandle};
use silo_core::replay::SharedScript;
use silo_core::sandbox::AccessReport;
use silo_core::tool::{ToolCall, ToolDef, ToolOutput};
use silo_core::traits::Sandbox;

pub fn create(
    config: &SandboxConfig,
    script: SharedScript,
    journal: JournalHandle,
) -> Result<Box<dyn Sandbox>, SandboxError> {
    Ok(Box::new(MockSandbox {
        config: config.clone(),
        script,
        journal,
    }))
}

struct MockSandbox {
    config: SandboxConfig,
    script: SharedScript,
    journal: JournalHandle,
}

#[async_trait]
impl Sandbox for MockSandbox {
    fn kind(&self) -> &'static str {
        "mock"
    }

    async fn start(&mut self) -> Result<(), SandboxError> {
        self.journal.append(JournalEntry::Lifecycle {
            message: "mock sandbox started".into(),
        });
        Ok(())
    }

    fn tool_defs(&self) -> Vec<ToolDef> {
        crate::tools::sandbox_tool_defs()
    }

    async fn run_tool(
        &self,
        _agent: &AgentId,
        call: &ToolCall,
    ) -> Result<ToolOutput, SandboxError> {
        self.script.next_tool(call)
    }

    fn access_report(&self) -> AccessReport {
        AccessReport {
            sandbox_kind: "mock".into(),
            workspace_mount: self
                .config
                .workspace_mount
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "/workspace".into()),
            scratch_dir: String::new(),
            readable_paths: self
                .config
                .read_allowlist
                .iter()
                .map(|path| path.display().to_string())
                .collect(),
            allowed_domains: self.config.proxy.allowed_domains.clone(),
            credential_domains: self
                .config
                .proxy
                .credentials
                .iter()
                .map(|credential| credential.host.clone())
                .collect(),
            notes: vec!["mock sandbox: nothing is executed".into()],
        }
    }

    async fn shutdown(&mut self) -> Result<(), SandboxError> {
        self.journal.append(JournalEntry::Lifecycle {
            message: "mock sandbox shut down".into(),
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use silo_core::clock::FakeClock;
    use silo_core::replay::{ScriptedToolExec, TestScript};
    use silo_core::secrets::CredentialInjection;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn journal() -> JournalHandle {
        JournalHandle::disabled(Arc::new(FakeClock::default()))
    }

    fn scripted(executions: Vec<ScriptedToolExec>) -> SharedScript {
        SharedScript::new(TestScript {
            tools: executions,
            ..TestScript::default()
        })
    }

    fn bash_call(command: &str) -> ToolCall {
        ToolCall {
            id: "t1".into(),
            name: "Bash".into(),
            input: json!({"command": command}),
        }
    }

    #[tokio::test]
    async fn expected_call_plays_back_the_scripted_output() {
        let script = scripted(vec![ScriptedToolExec {
            expect_name: "Bash".into(),
            expect_input: Some(json!({"command": "ls"})),
            output: ToolOutput::ok("file.txt"),
        }]);
        let mut sandbox = create(&SandboxConfig::default(), script.clone(), journal()).unwrap();
        sandbox.start().await.unwrap();
        let output = sandbox
            .run_tool(&"agent-0".to_string(), &bash_call("ls"))
            .await
            .unwrap();
        assert_eq!(output, ToolOutput::ok("file.txt"));
        assert!(script.finished());
        sandbox.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn wrong_tool_name_is_a_script_mismatch() {
        let script = scripted(vec![ScriptedToolExec {
            expect_name: "Read".into(),
            expect_input: None,
            output: ToolOutput::ok(""),
        }]);
        let sandbox = create(&SandboxConfig::default(), script, journal()).unwrap();
        let err = sandbox
            .run_tool(&"agent-0".to_string(), &bash_call("ls"))
            .await
            .unwrap_err();
        assert!(
            matches!(err, SandboxError::ScriptMismatch(_)),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn exhausted_script_is_a_script_mismatch() {
        let sandbox = create(&SandboxConfig::default(), scripted(vec![]), journal()).unwrap();
        let err = sandbox
            .run_tool(&"agent-0".to_string(), &bash_call("ls"))
            .await
            .unwrap_err();
        assert!(
            matches!(err, SandboxError::ScriptMismatch(_)),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn tool_defs_are_the_sandbox_tools() {
        let sandbox = create(&SandboxConfig::default(), scripted(vec![]), journal()).unwrap();
        assert_eq!(sandbox.kind(), "mock");
        let names: Vec<String> = sandbox.tool_defs().into_iter().map(|d| d.name).collect();
        assert_eq!(
            names,
            ["Read", "Write", "Edit", "Bash", "WebFetch", "WebSearch"]
        );
    }

    #[tokio::test]
    async fn access_report_reflects_the_config() {
        let config = SandboxConfig {
            workspace_mount: Some(PathBuf::from("/mnt/ws")),
            read_allowlist: vec![PathBuf::from("/usr/bin"), PathBuf::from("/opt/tools")],
            proxy: silo_core::config::ProxySettings {
                allowed_domains: vec!["crates.io".into(), "*.github.com".into()],
                credentials: vec![CredentialInjection {
                    host: "api.github.com".into(),
                    header: "Authorization".into(),
                    value_env: "GITHUB_TOKEN".into(),
                    format: "Bearer {secret}".into(),
                }],
            },
            ..SandboxConfig::default()
        };
        let sandbox = create(&config, scripted(vec![]), journal()).unwrap();
        let report = sandbox.access_report();
        assert_eq!(report.sandbox_kind, "mock");
        assert_eq!(report.workspace_mount, "/mnt/ws");
        assert_eq!(report.readable_paths, ["/usr/bin", "/opt/tools"]);
        assert_eq!(report.allowed_domains, ["crates.io", "*.github.com"]);
        assert_eq!(report.credential_domains, ["api.github.com"]);
        assert_eq!(report.notes, ["mock sandbox: nothing is executed"]);
    }

    #[tokio::test]
    async fn workspace_mount_defaults_when_unset() {
        let sandbox = create(&SandboxConfig::default(), scripted(vec![]), journal()).unwrap();
        assert_eq!(sandbox.access_report().workspace_mount, "/workspace");
    }

    #[tokio::test]
    async fn user_shell_is_unavailable() {
        let sandbox = create(&SandboxConfig::default(), scripted(vec![]), journal()).unwrap();
        let err = sandbox.user_shell(None).await.unwrap_err();
        assert!(matches!(err, SandboxError::Unavailable(_)), "got {err:?}");
    }
}
