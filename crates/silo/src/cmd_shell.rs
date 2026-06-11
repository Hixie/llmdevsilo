//! `silo shell`: an interactive user session sandboxed identically to the
//! LLM's tools.
//!
//! The shell shares the workspace mount with any running harness, so the
//! harness's work can be inspected live. When the workspace is attached to
//! a live harness whose run file records its sandbox policy and no sandbox
//! flags are given, the shell mirrors that policy (sandbox kind, read
//! allowlist, allowed domains). Explicit flags always win. Credential
//! injection is never mirrored; only `--inject-credential` flags apply.
//!
//! SIGTERM and SIGINT terminate the sandboxed shell's process group, then
//! the session unwinds through sandbox shutdown and the workspace detach
//! guard, so `silo workspace unlock` can stop a shell cleanly.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;

use silo_core::clock::{RealClock, SharedClock};
use silo_core::config::{ProxySettings, SandboxConfig, SandboxKind};
use silo_core::journal::{JournalEntry, JournalHandle, JournalWriter};
use silo_core::protocol::RunInfo;
use silo_core::traits::Sandbox;
use silo_workspace::WorkspaceManager;

use crate::cli::{resolve_sandbox_kind, sandbox_kind_from_name, SandboxOpt, ShellArgs};

/// The effective sandbox policy for the shell session.
struct ShellPolicy {
    kind: SandboxKind,
    read_allowlist: Vec<PathBuf>,
    allowed_domains: Vec<String>,
    /// Harness id whose policy was mirrored, when mirroring applied.
    mirrored_from: Option<String>,
}

pub async fn execute(args: ShellArgs) -> anyhow::Result<u8> {
    let state_dir = silo_core::paths::state_dir();
    std::fs::create_dir_all(&state_dir)?;

    let manager = WorkspaceManager::new(state_dir.clone());
    let policy = resolve_policy(&args, &manager, &state_dir)?;

    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let mut accepted_risky = args.allow_risky_path.clone();
    if let Some(harness_id) = &policy.mirrored_from {
        // Mirrored entries were already accepted by the running harness;
        // they pass the risk scan by inheritance. The ones the scan would
        // have flagged are printed.
        let flagged: BTreeSet<String> =
            silo_core::risk::scan_allowlist(&policy.read_allowlist, &home, &state_dir)
                .into_iter()
                .map(|(entry, _)| entry.display().to_string())
                .collect();
        if !flagged.is_empty() {
            println!(
                "Read allowlist entries accepted by inheritance from running harness \
                 {harness_id}: {}",
                flagged.into_iter().collect::<Vec<_>>().join(", ")
            );
        }
        accepted_risky.extend(policy.read_allowlist.iter().cloned());
    }
    silo_harness::validate_read_allowlist(
        &policy.read_allowlist,
        &home,
        &state_dir,
        &accepted_risky,
    )?;

    let session_id = format!("shell-{}", silo_core::short_id());
    let attached = manager
        .attach_shared(&args.workspace)
        .with_context(|| format!("attaching workspace {}", args.workspace.display()))?;

    let clock: SharedClock = Arc::new(RealClock::default());
    let journal_path =
        silo_core::paths::journals_dir(&state_dir).join(format!("{session_id}.jsonl"));
    let journal = JournalHandle::new(JournalWriter::to_file(&journal_path, clock)?);
    journal.append(JournalEntry::Lifecycle {
        message: format!("user shell starting in {}", args.workspace.display()),
    });

    let credentials = args
        .inject_credential
        .iter()
        .map(|value| crate::cli::parse_inject_credential(value))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let proxy_settings = ProxySettings {
        allowed_domains: policy.allowed_domains.clone(),
        credentials,
    };
    let mut proxy = silo_proxy::create_proxy(proxy_settings.clone(), journal.clone());
    let proxy_handle = proxy.start().await?;

    let sandbox_config = SandboxConfig {
        kind: policy.kind,
        read_allowlist: policy.read_allowlist.clone(),
        proxy: proxy_settings,
        workspace_mount: Some(attached.mount_path.clone()),
        scratch_root: None,
    };
    let mut sandbox =
        silo_sandbox::create_sandbox(&sandbox_config, Some(proxy_handle), None, journal.clone())
            .await?;
    sandbox.start().await?;

    let command = if args.command.is_empty() {
        None
    } else {
        Some(args.command.clone())
    };
    let shell_result = run_user_shell(sandbox.as_ref(), command).await;

    if let Err(error) = sandbox.shutdown().await {
        eprintln!("warning: sandbox shutdown failed: {error}");
    }
    if let Err(error) = proxy.shutdown().await {
        eprintln!("warning: proxy shutdown failed: {error}");
    }
    attached.detach();
    journal.append(JournalEntry::Lifecycle {
        message: "user shell ended".into(),
    });

    let code = shell_result?;
    Ok(u8::try_from(code.clamp(0, 255)).unwrap_or(1))
}

/// Runs the user shell while listening for SIGTERM and SIGINT. A signal
/// terminates the shell's process group via the sandbox; the shell call
/// then returns and the caller unwinds through its cleanup path.
#[cfg(unix)]
async fn run_user_shell(
    sandbox: &dyn Sandbox,
    command: Option<Vec<String>>,
) -> anyhow::Result<i32> {
    use tokio::signal::unix::{signal, SignalKind};

    let mut sigterm = signal(SignalKind::terminate()).context("installing the SIGTERM handler")?;
    let mut sigint = signal(SignalKind::interrupt()).context("installing the SIGINT handler")?;
    let shell = sandbox.user_shell(command);
    tokio::pin!(shell);
    loop {
        tokio::select! {
            result = &mut shell => return Ok(result?),
            _ = sigterm.recv() => {
                eprintln!("silo shell: received SIGTERM; terminating the sandboxed shell");
                let _ = sandbox.terminate_user_shell().await;
            }
            _ = sigint.recv() => {
                let _ = sandbox.terminate_user_shell().await;
            }
        }
    }
}

#[cfg(not(unix))]
async fn run_user_shell(
    sandbox: &dyn Sandbox,
    command: Option<Vec<String>>,
) -> anyhow::Result<i32> {
    Ok(sandbox.user_shell(command).await?)
}

/// Decides the shell's sandbox policy: mirror the running harness when no
/// sandbox flags were given and a live harness's run file records its
/// policy; otherwise use the flags.
fn resolve_policy(
    args: &ShellArgs,
    manager: &WorkspaceManager,
    state_dir: &std::path::Path,
) -> anyhow::Result<ShellPolicy> {
    let from_flags = |args: &ShellArgs| ShellPolicy {
        kind: resolve_sandbox_kind(args.sandbox.unwrap_or(SandboxOpt::Auto)),
        read_allowlist: args.allow_read.clone(),
        allowed_domains: args.allow_domain.clone(),
        mirrored_from: None,
    };

    let running = running_harness_policy(args, manager, state_dir);
    let explicit_flags =
        !args.allow_read.is_empty() || !args.allow_domain.is_empty() || args.sandbox.is_some();

    let Some((harness_id, info)) = running else {
        return Ok(from_flags(args));
    };
    if explicit_flags {
        println!(
            "Note: explicit sandbox flags given; this shell's access policy \
             differs from running harness {harness_id}'s."
        );
        return Ok(from_flags(args));
    }
    let kind_name = info.sandbox_kind.as_deref().unwrap_or_default();
    let Some(kind) = sandbox_kind_from_name(kind_name) else {
        eprintln!(
            "warning: running harness {harness_id} uses unknown sandbox kind \
             {kind_name:?}; not mirroring its policy"
        );
        return Ok(from_flags(args));
    };
    println!(
        "Mirroring running harness {harness_id}'s sandbox policy \
         ({kind_name}, {} readable path(s), {} allowed domain(s)); \
         credential injection is not mirrored.",
        info.read_allowlist.len(),
        info.allowed_domains.len()
    );
    Ok(ShellPolicy {
        kind,
        read_allowlist: info.read_allowlist.iter().map(PathBuf::from).collect(),
        allowed_domains: info.allowed_domains.clone(),
        mirrored_from: Some(harness_id),
    })
}

/// Finds the live harness attached to the workspace, when its run file
/// exists and records the sandbox policy. Dead harnesses and run files
/// without the policy fields yield `None`.
fn running_harness_policy(
    args: &ShellArgs,
    manager: &WorkspaceManager,
    state_dir: &std::path::Path,
) -> Option<(String, RunInfo)> {
    let status = manager.status(&args.workspace).ok()?;
    let harness_id = status.attached_harness?;
    let run_file = silo_core::paths::runs_dir(state_dir).join(format!("{harness_id}.json"));
    let text = std::fs::read_to_string(run_file).ok()?;
    let info: RunInfo = serde_json::from_str(&text).ok()?;
    if !crate::cmd_harnesses::pid_alive(info.pid) || info.sandbox_kind.is_none() {
        return None;
    }
    Some((harness_id, info))
}
