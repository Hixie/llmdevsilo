//! The Linux gVisor (runsc) backend.
//!
//! The backend assembles an OCI bundle (see [`super::spec`]) whose rootfs
//! is built entirely from bind mounts: the OS directories and the read
//! allowlist read-only at their own paths, the workspace read/write at
//! `/workspace`, and the scratch space read/write at `/scratch`. The
//! helper binary is copied into the scratch space and runs as the
//! container's init process, connecting back over a Unix socket in the
//! scratch space.
//!
//! The container runs with `--network=none`: gVisor still provides an
//! in-sandbox loopback interface, but no external connectivity and no
//! DNS. The helper starts a relay on `127.0.0.1:3128` inside the sandbox
//! (driven by `SILO_PROXY_RELAY`) that pipes to `/scratch/proxy.sock`;
//! the harness side forwards that socket to the egress proxy's TCP
//! address. That relay is the only egress path, and proxy CONNECT
//! requests carry hostnames, so name resolution happens in the proxy.

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

use super::forward::{start_unix_to_tcp_forwarder, UnixToTcpForwarder};
use super::spec::{self, GvisorSpec};
use crate::scratch::ScratchSpace;
use crate::session::{self, HelperSession};
use crate::toolimpl;

const HELPER_NAME: &str = "silo-helper";
const HELPER_ACCEPT_TIMEOUT: Duration = Duration::from_secs(15);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

pub async fn create(
    config: &SandboxConfig,
    proxy: ProxyHandle,
    journal: JournalHandle,
) -> Result<Box<dyn Sandbox>, SandboxError> {
    Ok(Box::new(GvisorBackend {
        config: config.clone(),
        proxy,
        journal,
        running: None,
    }))
}

struct Running {
    scratch: ScratchSpace,
    container_id: String,
    runsc: PathBuf,
    /// Owns the bundle directory (config.json and rootfs); removed on
    /// drop.
    _bundle: tempfile::TempDir,
    _forwarder: UnixToTcpForwarder,
    session: HelperSession,
    child: Child,
}

struct GvisorBackend {
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

fn runsc_binary() -> PathBuf {
    match std::env::var_os("SILO_RUNSC") {
        Some(value) if !value.is_empty() => PathBuf::from(value),
        _ => PathBuf::from("runsc"),
    }
}

/// Finds a Linux helper binary to copy into the sandbox: the
/// `SILO_HELPER_BIN_LINUX` environment variable, then the same machine's
/// helper via `SILO_HELPER_BIN`, next to the current executable (and one
/// directory up), then the `PATH`.
fn locate_helper() -> Result<PathBuf, SandboxError> {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf));
    locate_helper_in(
        std::env::var_os("SILO_HELPER_BIN_LINUX").or_else(|| std::env::var_os("SILO_HELPER_BIN")),
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
                "the helper binary override points to {}, which is not a file",
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
        "cannot locate the silo-helper binary; set SILO_HELPER_BIN_LINUX or install it next to the harness".into(),
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

impl GvisorBackend {
    fn running(&self) -> Result<&Running, SandboxError> {
        self.running
            .as_ref()
            .ok_or_else(|| SandboxError::Unavailable("the sandbox is not started".into()))
    }
}

#[async_trait]
impl Sandbox for GvisorBackend {
    fn kind(&self) -> &'static str {
        "linux-gvisor"
    }

    async fn start(&mut self) -> Result<(), SandboxError> {
        if self.running.is_some() {
            return Err(SandboxError::Setup("the sandbox is already started".into()));
        }
        let workspace = self.config.workspace_mount.as_deref().ok_or_else(|| {
            SandboxError::Setup("the gVisor backend requires a workspace mount".into())
        })?;
        let workspace = canonicalize(workspace, "workspace")?;

        let scratch =
            ScratchSpace::create(self.config.scratch_root.as_deref(), &self.proxy.ca_cert_pem)?;
        let scratch_canonical = canonicalize(scratch.root(), "scratch")?;

        let mut read_allowlist = Vec::with_capacity(self.config.read_allowlist.len());
        for path in &self.config.read_allowlist {
            read_allowlist.push(canonicalize(path, "read allowlist")?);
        }

        // Copy the helper into the scratch space, where the bind mount
        // makes it visible (and executable) inside the sandbox.
        let helper_source = locate_helper()?;
        let bin_dir = scratch.root().join("bin");
        std::fs::create_dir_all(&bin_dir)?;
        let helper_dest = bin_dir.join(HELPER_NAME);
        std::fs::copy(&helper_source, &helper_dest).map_err(|e| {
            SandboxError::Setup(format!(
                "cannot copy {} into the scratch space: {e}",
                helper_source.display()
            ))
        })?;
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&helper_dest, std::fs::Permissions::from_mode(0o755))?;
        }

        // The proxy address inside the sandbox is fixed: the helper's
        // relay on loopback port 3128.
        let in_sandbox_proxy =
            std::net::SocketAddr::from(([127, 0, 0, 1], spec::IN_SANDBOX_PROXY_PORT));
        let env = spec::container_env(&scratch.sandbox_env(in_sandbox_proxy), &scratch_canonical);

        let oci_config = spec::generate_config(&GvisorSpec {
            workspace: workspace.clone(),
            scratch: scratch_canonical,
            read_allowlist,
            os_dirs: spec::default_os_dirs(|path| path.exists()),
            env,
        });
        let bundle = tempfile::Builder::new()
            .prefix("silo-gvisor-")
            .tempdir()
            .map_err(SandboxError::Io)?;
        std::fs::create_dir(bundle.path().join("rootfs"))?;
        let config_json = serde_json::to_vec_pretty(&oci_config)
            .map_err(|e| SandboxError::Setup(format!("cannot serialize the OCI config: {e}")))?;
        std::fs::write(bundle.path().join("config.json"), config_json)?;

        let helper_socket = scratch.root().join("helper.sock");
        let listener = session::listen_unix(&helper_socket, HELPER_ACCEPT_TIMEOUT).await?;

        let proxy_socket = scratch.root().join("proxy.sock");
        let proxy_listener = tokio::net::UnixListener::bind(&proxy_socket).map_err(|e| {
            SandboxError::Setup(format!(
                "cannot bind proxy socket {}: {e}",
                proxy_socket.display()
            ))
        })?;
        let forwarder = start_unix_to_tcp_forwarder(proxy_listener, self.proxy.http_addr);

        let runsc = runsc_binary();
        let container_id = format!("silo-{}", silo_core::short_id());
        let mut child = Command::new(&runsc)
            .arg("--network=none")
            .arg("--host-uds=all")
            .arg("run")
            .arg("--bundle")
            .arg(bundle.path())
            .arg(&container_id)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| SandboxError::Setup(format!("cannot spawn {}: {e}", runsc.display())))?;

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
                    "runsc exited before the helper connected: {status:?}"
                )));
            }
        };

        self.journal.append(JournalEntry::Lifecycle {
            message: format!(
                "gVisor sandbox started (container {container_id}, helper pid {}, version {})",
                session.helper_pid(),
                session.helper_version()
            ),
        });

        self.running = Some(Running {
            scratch,
            container_id,
            runsc,
            _bundle: bundle,
            _forwarder: forwarder,
            session,
            child,
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
        toolimpl::run_tool(
            &running.session,
            Path::new(spec::WORKSPACE_DEST),
            &running.scratch,
            call,
        )
        .await
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
        let scratch_dir = match &self.running {
            Some(_) => spec::SCRATCH_DEST.to_string(),
            None => String::new(),
        };
        let mut readable_paths: Vec<String> = self
            .config
            .read_allowlist
            .iter()
            .map(|path| path.display().to_string())
            .collect();
        readable_paths.extend(
            spec::default_os_dirs(|path| path.exists())
                .iter()
                .map(|path| path.display().to_string()),
        );
        AccessReport {
            sandbox_kind: "linux-gvisor".into(),
            workspace_mount: spec::WORKSPACE_DEST.into(),
            scratch_dir,
            readable_paths,
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
                "the sandbox has no external network interface; the only egress is a relay on \
                 127.0.0.1:3128 that pipes to the harness proxy, and DNS is not resolvable \
                 inside the sandbox"
                    .into(),
                "only bind-mounted paths are visible inside the sandbox; the rest of the host \
                 filesystem is not reachable, not even its metadata"
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
            None => vec!["/bin/sh".to_string(), "-i".to_string()],
        };
        let status = Command::new(&running.runsc)
            .arg("exec")
            .arg("--cwd")
            .arg(spec::WORKSPACE_DEST)
            .arg(&running.container_id)
            .args(&argv)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .await
            .map_err(|e| SandboxError::Setup(format!("cannot spawn runsc exec: {e}")))?;
        if let Some(code) = status.code() {
            return Ok(code);
        }
        {
            use std::os::unix::process::ExitStatusExt;
            if let Some(signal) = status.signal() {
                return Ok(128 + signal);
            }
        }
        Ok(-1)
    }

    async fn shutdown(&mut self) -> Result<(), SandboxError> {
        let Some(mut running) = self.running.take() else {
            return Ok(());
        };
        let _ = tokio::time::timeout(SHUTDOWN_TIMEOUT, running.session.shutdown()).await;
        // Best-effort container teardown; the child kill below covers the
        // case where runsc itself is stuck.
        let _ = tokio::time::timeout(
            SHUTDOWN_TIMEOUT,
            Command::new(&running.runsc)
                .arg("kill")
                .arg(&running.container_id)
                .arg("KILL")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status(),
        )
        .await;
        kill_and_reap(&mut running.child).await;
        let _ = tokio::time::timeout(
            SHUTDOWN_TIMEOUT,
            Command::new(&running.runsc)
                .arg("delete")
                .arg("-force")
                .arg(&running.container_id)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status(),
        )
        .await;
        running.scratch.cleanup()?;
        self.journal.append(JournalEntry::Lifecycle {
            message: "gVisor sandbox shut down".into(),
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
        let err = locate_helper_in(Some("/no/such/helper".into()), None, None).unwrap_err();
        assert!(matches!(err, SandboxError::Setup(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn tool_defs_and_kind_before_start() {
        let sandbox = create(&SandboxConfig::default(), proxy_handle(), journal())
            .await
            .unwrap();
        assert_eq!(sandbox.kind(), "linux-gvisor");
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
    async fn shutdown_before_start_is_a_no_op() {
        let mut sandbox = create(&SandboxConfig::default(), proxy_handle(), journal())
            .await
            .unwrap();
        sandbox.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn access_report_reflects_the_in_sandbox_view() {
        let config = SandboxConfig {
            kind: silo_core::config::SandboxKind::LinuxGvisor,
            read_allowlist: vec![PathBuf::from("/opt/tools")],
            workspace_mount: Some(PathBuf::from("/srv/ws")),
            ..SandboxConfig::default()
        };
        let sandbox = create(&config, proxy_handle(), journal()).await.unwrap();
        let report = sandbox.access_report();
        assert_eq!(report.sandbox_kind, "linux-gvisor");
        assert_eq!(report.workspace_mount, "/workspace");
        assert!(report.readable_paths.contains(&"/opt/tools".to_string()));
        let notes = report.notes.join("\n");
        assert!(notes.contains("reachable by the host user"));
        assert!(notes.contains("the scratch space is a host directory (mode 0700)"));
        assert!(notes.contains("127.0.0.1:3128"));
    }
}
