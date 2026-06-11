//! Sandbox backends.
//!
//! Each backend confines the helper process (and everything it spawns) to:
//! read/write access to the workspace mount and a per-sandbox scratch
//! space, read/execute access to the configured host allowlist, and network
//! egress only through the harness's proxy. The mock backend executes
//! nothing: it validates tool calls against a test script and plays back
//! recorded outputs.
//!
//! Available backends by platform:
//! - any: `mock`
//! - macOS: `macos-sandbox-exec` (Seatbelt), `macos-linux-vm`
//!   (Virtualization.framework guest)
//! - Linux: `linux-gvisor` (runsc), `linux-microvm` (Firecracker-style)

use silo_core::config::SandboxConfig;
use silo_core::error::SandboxError;
use silo_core::journal::JournalHandle;
use silo_core::replay::SharedScript;
use silo_core::traits::{ProxyHandle, Sandbox};

pub mod scratch;
pub mod search;
pub mod session;
pub mod toolimpl;
pub mod tools;

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;
pub mod mock;

/// Creates the configured sandbox backend (not yet started).
///
/// `proxy` carries the egress proxy address and session CA certificate;
/// it is required by every real backend and ignored by the mock. `script`
/// is required by (and only used by) the mock backend.
pub async fn create_sandbox(
    config: &SandboxConfig,
    proxy: Option<ProxyHandle>,
    script: Option<SharedScript>,
    journal: JournalHandle,
) -> Result<Box<dyn Sandbox>, SandboxError> {
    use silo_core::config::SandboxKind;
    match &config.kind {
        SandboxKind::Mock => {
            let script = script.ok_or_else(|| {
                SandboxError::Setup("the mock sandbox requires a test script".into())
            })?;
            mock::create(config, script, journal)
        }
        #[cfg(target_os = "macos")]
        SandboxKind::MacosSandboxExec => {
            let proxy = proxy.ok_or_else(|| {
                SandboxError::Setup("sandbox-exec requires an egress proxy".into())
            })?;
            macos::sandbox_exec::create(config, proxy, journal).await
        }
        #[cfg(target_os = "macos")]
        SandboxKind::MacosLinuxVm => {
            let proxy = proxy.ok_or_else(|| {
                SandboxError::Setup("the Linux VM sandbox requires an egress proxy".into())
            })?;
            macos::linux_vm::create(config, proxy, journal).await
        }
        #[cfg(target_os = "linux")]
        SandboxKind::LinuxGvisor => {
            let proxy = proxy.ok_or_else(|| {
                SandboxError::Setup("the gVisor sandbox requires an egress proxy".into())
            })?;
            linux::gvisor::create(config, proxy, journal).await
        }
        #[cfg(target_os = "linux")]
        SandboxKind::LinuxMicrovm => {
            let proxy = proxy.ok_or_else(|| {
                SandboxError::Setup("the microVM sandbox requires an egress proxy".into())
            })?;
            linux::microvm::create(config, proxy, journal).await
        }
        #[allow(unreachable_patterns)]
        other => Err(SandboxError::Unavailable(format!(
            "sandbox backend {other:?} is not available on this platform"
        ))),
    }
}

/// Sends SIGTERM to a process group, waits up to two seconds for it to
/// disappear, then sends SIGKILL. Backends spawn user shells as group
/// leaders so the whole session is killable as a unit.
#[cfg(unix)]
pub(crate) async fn terminate_process_group(pgid: i32) {
    use nix::sys::signal::{killpg, Signal};
    use nix::unistd::Pid;
    use std::time::{Duration, Instant};

    let group = Pid::from_raw(pgid);
    if killpg(group, Signal::SIGTERM).is_err() {
        return;
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if killpg(group, None).is_err() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let _ = killpg(group, Signal::SIGKILL);
}
