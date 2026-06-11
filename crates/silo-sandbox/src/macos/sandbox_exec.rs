//! The macOS sandbox-exec (Seatbelt) backend.
//!
//! The backend generates an SBPL profile (see [`crate::macos::profile`]),
//! writes it to a private temporary directory, and launches the helper
//! process under `/usr/bin/sandbox-exec -f <profile>`. The helper connects
//! back over a Unix socket inside the scratch space; tool calls then flow
//! through [`crate::toolimpl`]. The profile confines the helper and every
//! process it spawns: writes land only in the workspace and the scratch
//! space, reads are limited to those plus the configured allowlist and the
//! OS baseline, and network egress is loopback-only, which funnels HTTP(S)
//! through the harness proxy via the proxy environment variables.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use silo_core::config::SandboxConfig;
use silo_core::conversation::AgentId;
use silo_core::error::SandboxError;
use silo_core::journal::{JournalEntry, JournalHandle};
use silo_core::sandbox::AccessReport;
use silo_core::tool::{ToolCall, ToolDef, ToolOutput};
use silo_core::traits::{ProxyHandle, Sandbox};
use tokio::process::{Child, Command};

use super::profile::{self, ProfileSpec};
use crate::scratch::ScratchSpace;
use crate::session::{self, HelperSession};
use crate::toolimpl;

const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";
const HELPER_NAME: &str = "silo-helper";
const SANDBOX_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";
const HELPER_ACCEPT_TIMEOUT: Duration = Duration::from_secs(15);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

pub async fn create(
    config: &SandboxConfig,
    proxy: ProxyHandle,
    journal: JournalHandle,
) -> Result<Box<dyn Sandbox>, SandboxError> {
    Ok(Box::new(SandboxExecBackend {
        config: config.clone(),
        proxy,
        journal,
        running: None,
    }))
}

struct Running {
    scratch: ScratchSpace,
    /// Canonical workspace path; tool paths resolve against it.
    workspace: PathBuf,
    profile_path: PathBuf,
    /// Owns the profile file; removed on drop.
    _profile_dir: tempfile::TempDir,
    session: HelperSession,
    child: Child,
    /// Environment shared by the helper and user shells.
    env: Vec<(String, String)>,
    /// Process group of a live user shell; `terminate_user_shell` signals
    /// it.
    shell_pgid: std::sync::Mutex<Option<i32>>,
}

struct SandboxExecBackend {
    config: SandboxConfig,
    proxy: ProxyHandle,
    journal: JournalHandle,
    running: Option<Running>,
}

fn canonicalize(path: &Path, role: &str) -> Result<PathBuf, SandboxError> {
    std::fs::canonicalize(path).map_err(|e| {
        SandboxError::Setup(format!(
            "cannot resolve {role} path {}: {e}",
            path.display()
        ))
    })
}

/// Finds the helper binary: the `SILO_HELPER_BIN` environment variable,
/// then next to the current executable (and one directory up, covering
/// test binaries in `target/<profile>/deps/`), then the `PATH`.
fn locate_helper() -> Result<PathBuf, SandboxError> {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf));
    locate_helper_in(
        std::env::var_os("SILO_HELPER_BIN"),
        exe_dir,
        std::env::var_os("PATH"),
    )
}

fn locate_helper_in(
    env_value: Option<OsString>,
    exe_dir: Option<PathBuf>,
    path_var: Option<OsString>,
) -> Result<PathBuf, SandboxError> {
    if let Some(value) = env_value {
        if !value.is_empty() {
            let path = PathBuf::from(value);
            if path.is_file() {
                return Ok(path);
            }
            return Err(SandboxError::Setup(format!(
                "SILO_HELPER_BIN points to {}, which is not a file",
                path.display()
            )));
        }
    }
    if let Some(dir) = exe_dir {
        let mut candidates = vec![dir.join(HELPER_NAME)];
        if let Some(parent) = dir.parent() {
            candidates.push(parent.join(HELPER_NAME));
        }
        for candidate in candidates {
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    if let Some(path_var) = path_var {
        for dir in std::env::split_paths(&path_var) {
            if dir.as_os_str().is_empty() {
                continue;
            }
            let candidate = dir.join(HELPER_NAME);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    Err(SandboxError::Setup(
        "cannot locate the silo-helper binary; set SILO_HELPER_BIN or install it next to the harness".into(),
    ))
}

/// Kills `child` and waits for it, bounded by [`SHUTDOWN_TIMEOUT`].
async fn kill_and_reap(child: &mut Child) {
    if matches!(child.try_wait(), Ok(Some(_))) {
        return;
    }
    let _ = child.start_kill();
    let _ = tokio::time::timeout(SHUTDOWN_TIMEOUT, child.wait()).await;
}

impl SandboxExecBackend {
    fn running(&self) -> Result<&Running, SandboxError> {
        self.running
            .as_ref()
            .ok_or_else(|| SandboxError::Unavailable("the sandbox is not started".into()))
    }
}

#[async_trait]
impl Sandbox for SandboxExecBackend {
    fn kind(&self) -> &'static str {
        "macos-sandbox-exec"
    }

    async fn start(&mut self) -> Result<(), SandboxError> {
        if self.running.is_some() {
            return Err(SandboxError::Setup("the sandbox is already started".into()));
        }
        let workspace = self.config.workspace_mount.as_deref().ok_or_else(|| {
            SandboxError::Setup("the sandbox-exec backend requires a workspace mount".into())
        })?;
        let workspace = canonicalize(workspace, "workspace")?;

        let scratch =
            ScratchSpace::create(self.config.scratch_root.as_deref(), &self.proxy.ca_cert_pem)?;
        let scratch_canonical = canonicalize(scratch.root(), "scratch")?;

        let mut read_allowlist = Vec::with_capacity(self.config.read_allowlist.len());
        for path in &self.config.read_allowlist {
            read_allowlist.push(canonicalize(path, "read allowlist")?);
        }

        let helper = canonicalize(&locate_helper()?, "helper binary")?;

        let profile_text = profile::generate(&ProfileSpec {
            workspace: workspace.clone(),
            scratch: scratch_canonical,
            read_allowlist,
            read_files: vec![helper.clone()],
        })?;
        // The profile lives in its own 0700 temporary directory, outside
        // the sandbox-readable scratch; sandbox-exec reads it from outside
        // the sandbox.
        let profile_dir = tempfile::Builder::new()
            .prefix("silo-sbpl-")
            .tempdir()
            .map_err(SandboxError::Io)?;
        let profile_path = profile_dir.path().join("profile.sb");
        std::fs::write(&profile_path, &profile_text)?;

        let socket_path = scratch.root().join("helper.sock");
        let listener = session::listen_unix(&socket_path, HELPER_ACCEPT_TIMEOUT).await?;

        let mut env = scratch.sandbox_env(self.proxy.http_addr);
        env.push(("PATH".into(), SANDBOX_PATH.into()));

        let mut command = Command::new(SANDBOX_EXEC);
        command
            .arg("-f")
            .arg(&profile_path)
            .arg(&helper)
            .arg(format!("unix:{}", socket_path.display()))
            .env_clear()
            .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .current_dir(&workspace)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .map_err(|e| SandboxError::Setup(format!("cannot spawn {SANDBOX_EXEC}: {e}")))?;

        let session = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok(session) => session,
                Err(e) => {
                    kill_and_reap(&mut child).await;
                    return Err(e);
                }
            },
            status = child.wait() => {
                return Err(SandboxError::Setup(format!(
                    "sandbox-exec exited before the helper connected: {status:?}"
                )));
            }
        };

        self.journal.append(JournalEntry::Lifecycle {
            message: format!(
                "sandbox-exec sandbox started (helper pid {}, version {})",
                session.helper_pid(),
                session.helper_version()
            ),
        });

        self.running = Some(Running {
            scratch,
            workspace,
            profile_path,
            _profile_dir: profile_dir,
            session,
            child,
            env,
            shell_pgid: std::sync::Mutex::new(None),
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
        let running = self.running()?;
        toolimpl::run_tool(&running.session, &running.workspace, &running.scratch, call).await
    }

    /// Cancels in-flight helper executions so blocked `run_tool` calls
    /// return with their partial output.
    async fn interrupt(&self) -> Result<(), SandboxError> {
        if let Some(running) = &self.running {
            running.session.cancel_inflight().await;
        }
        Ok(())
    }

    fn access_report(&self) -> AccessReport {
        let (workspace_mount, scratch_dir) = match &self.running {
            Some(running) => (
                running.workspace.display().to_string(),
                running.scratch.root().display().to_string(),
            ),
            None => (
                self.config
                    .workspace_mount
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_default(),
                String::new(),
            ),
        };
        AccessReport {
            sandbox_kind: "macos-sandbox-exec".into(),
            workspace_mount,
            scratch_dir,
            readable_paths: profile::readable_paths(&self.config.read_allowlist),
            allowed_domains: self.config.proxy.allowed_domains.clone(),
            credential_domains: self
                .config
                .proxy
                .credentials
                .iter()
                .map(|credential| credential.host.clone())
                .collect(),
            notes: vec![
                "the locked workspace mount is technically reachable by the host user while the \
                 sandbox is attached; do not edit it from outside the sandbox"
                    .into(),
                "the scratch space is a host directory (mode 0700) reachable by the host user \
                 while the sandbox runs; do not access it from outside the sandbox"
                    .into(),
                "services listening on the host loopback interface are reachable from inside \
                 the sandbox"
                    .into(),
                "file metadata (existence, names, sizes, permissions) outside the read \
                 allowlist is readable from inside the sandbox; file contents are not"
                    .into(),
            ],
        }
    }

    async fn user_shell(&self, command: Option<Vec<String>>) -> Result<i32, SandboxError> {
        let running = self.running()?;
        let argv = match command {
            Some(argv) if !argv.is_empty() => argv,
            Some(_) => {
                return Err(SandboxError::Setup("user shell command is empty".into()));
            }
            None => {
                let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
                vec![shell, "-i".into()]
            }
        };

        let mut cmd = Command::new(SANDBOX_EXEC);
        cmd.arg("-f")
            .arg(&running.profile_path)
            .args(&argv)
            .env_clear()
            .envs(running.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .current_dir(&running.workspace)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        if let Ok(term) = std::env::var("TERM") {
            cmd.env("TERM", term);
        }
        // The shell runs as the leader of its own process group, so the
        // whole session can be terminated as a unit.
        cmd.process_group(0);
        let mut child = cmd
            .spawn()
            .map_err(|e| SandboxError::Setup(format!("cannot spawn user shell: {e}")))?;
        if let Some(pid) = child.id() {
            *running.shell_pgid.lock().expect("shell pgid poisoned") = Some(pid as i32);
        }
        let status = child.wait().await;
        *running.shell_pgid.lock().expect("shell pgid poisoned") = None;
        let status =
            status.map_err(|e| SandboxError::Setup(format!("cannot wait for user shell: {e}")))?;
        if let Some(code) = status.code() {
            return Ok(code);
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            if let Some(signal) = status.signal() {
                return Ok(128 + signal);
            }
        }
        Ok(-1)
    }

    async fn terminate_user_shell(&self) -> Result<(), SandboxError> {
        let Some(running) = &self.running else {
            return Ok(());
        };
        let pgid = *running.shell_pgid.lock().expect("shell pgid poisoned");
        if let Some(pgid) = pgid {
            crate::terminate_process_group(pgid).await;
        }
        Ok(())
    }

    async fn shutdown(&mut self) -> Result<(), SandboxError> {
        let Some(mut running) = self.running.take() else {
            return Ok(());
        };
        let _ = tokio::time::timeout(SHUTDOWN_TIMEOUT, running.session.shutdown()).await;
        kill_and_reap(&mut running.child).await;
        running.scratch.cleanup()?;
        self.journal.append(JournalEntry::Lifecycle {
            message: "sandbox-exec sandbox shut down".into(),
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use silo_core::clock::FakeClock;
    use std::sync::Arc;

    fn journal() -> JournalHandle {
        JournalHandle::disabled(Arc::new(FakeClock::default()))
    }

    fn proxy_handle() -> ProxyHandle {
        ProxyHandle {
            http_addr: "127.0.0.1:3128".parse().unwrap(),
            ca_cert_pem: "-----BEGIN CERTIFICATE-----\nFAKE\n-----END CERTIFICATE-----\n".into(),
            dns_addr: None,
        }
    }

    #[test]
    fn locate_helper_prefers_the_env_override() {
        let dir = tempfile::tempdir().unwrap();
        let helper = dir.path().join("silo-helper");
        std::fs::write(&helper, "#!/bin/sh\n").unwrap();
        let found = locate_helper_in(Some(helper.clone().into_os_string()), None, None).unwrap();
        assert_eq!(found, helper);
    }

    #[test]
    fn locate_helper_rejects_a_missing_env_override() {
        let err = locate_helper_in(Some("/no/such/helper".into()), None, None).unwrap_err();
        assert!(matches!(err, SandboxError::Setup(_)), "got {err:?}");
    }

    #[test]
    fn locate_helper_searches_exe_dir_its_parent_and_path() {
        let dir = tempfile::tempdir().unwrap();
        let deps = dir.path().join("deps");
        std::fs::create_dir(&deps).unwrap();
        let helper = dir.path().join("silo-helper");
        std::fs::write(&helper, "#!/bin/sh\n").unwrap();

        // Sibling of the parent directory (the target/<profile>/deps case).
        let found = locate_helper_in(None, Some(deps.clone()), None).unwrap();
        assert_eq!(found, helper);

        // Direct sibling.
        let found = locate_helper_in(None, Some(dir.path().to_path_buf()), None).unwrap();
        assert_eq!(found, helper);

        // PATH fallback.
        let path_var = std::env::join_paths([dir.path()]).unwrap();
        let found = locate_helper_in(None, None, Some(path_var)).unwrap();
        assert_eq!(found, helper);

        let err = locate_helper_in(None, None, None).unwrap_err();
        assert!(matches!(err, SandboxError::Setup(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn tool_defs_and_kind_before_start() {
        let sandbox = create(&SandboxConfig::default(), proxy_handle(), journal())
            .await
            .unwrap();
        assert_eq!(sandbox.kind(), "macos-sandbox-exec");
        let names: Vec<String> = sandbox.tool_defs().into_iter().map(|d| d.name).collect();
        assert_eq!(
            names,
            ["Read", "Write", "Edit", "Bash", "WebFetch", "WebSearch"]
        );
    }

    #[tokio::test]
    async fn run_tool_before_start_is_unavailable() {
        let sandbox = create(&SandboxConfig::default(), proxy_handle(), journal())
            .await
            .unwrap();
        let call = ToolCall {
            id: "t1".into(),
            name: "Bash".into(),
            input: serde_json::json!({"command": "true"}),
        };
        let err = sandbox
            .run_tool(&"agent-0".to_string(), &call)
            .await
            .unwrap_err();
        assert!(matches!(err, SandboxError::Unavailable(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn start_without_a_workspace_is_a_setup_error() {
        let mut sandbox = create(&SandboxConfig::default(), proxy_handle(), journal())
            .await
            .unwrap();
        let err = sandbox.start().await.unwrap_err();
        assert!(matches!(err, SandboxError::Setup(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn shutdown_before_start_is_a_no_op() {
        let mut sandbox = create(&SandboxConfig::default(), proxy_handle(), journal())
            .await
            .unwrap();
        sandbox.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn access_report_covers_config_and_required_notes() {
        let config = SandboxConfig {
            kind: silo_core::config::SandboxKind::MacosSandboxExec,
            read_allowlist: vec![PathBuf::from("/opt/tools")],
            workspace_mount: Some(PathBuf::from("/work/ws")),
            proxy: silo_core::config::ProxySettings {
                allowed_domains: vec!["crates.io".into()],
                credentials: vec![],
            },
            scratch_root: None,
        };
        let sandbox = create(&config, proxy_handle(), journal()).await.unwrap();
        let report = sandbox.access_report();
        assert_eq!(report.sandbox_kind, "macos-sandbox-exec");
        assert_eq!(report.workspace_mount, "/work/ws");
        assert!(report.readable_paths.contains(&"/opt/tools".to_string()));
        assert!(report.readable_paths.contains(&"/usr/lib".to_string()));
        assert_eq!(report.allowed_domains, ["crates.io"]);
        let notes = report.notes.join("\n");
        assert!(notes.contains("reachable by the host user"));
        assert!(notes.contains("the scratch space is a host directory (mode 0700)"));
        assert!(notes.contains("loopback"));
        assert!(notes.contains("metadata"));
    }
}
