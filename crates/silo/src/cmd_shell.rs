//! `silo shell`: an interactive user session sandboxed identically to the
//! LLM's tools.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;

use silo_core::clock::{RealClock, SharedClock};
use silo_core::config::{ProxySettings, SandboxConfig};
use silo_core::journal::{JournalEntry, JournalHandle, JournalWriter};
use silo_workspace::WorkspaceManager;

use crate::cli::ShellArgs;

pub async fn execute(args: ShellArgs) -> anyhow::Result<u8> {
    let state_dir = silo_core::paths::state_dir();
    std::fs::create_dir_all(&state_dir)?;

    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    silo_harness::validate_read_allowlist(
        &args.allow_read,
        &home,
        &state_dir,
        &args.allow_risky_path,
    )?;

    let session_id = format!("shell-{}", silo_core::short_id());
    let manager = WorkspaceManager::new(state_dir.clone());
    let attached = manager
        .attach(&args.workspace, &session_id)
        .with_context(|| format!("attaching workspace {}", args.workspace.display()))?;

    let clock: SharedClock = Arc::new(RealClock::default());
    let journal_path =
        silo_core::paths::journals_dir(&state_dir).join(format!("{session_id}.jsonl"));
    let journal = JournalHandle::new(JournalWriter::to_file(&journal_path, clock)?);
    journal.append(JournalEntry::Lifecycle {
        message: format!("user shell starting in {}", args.workspace.display()),
    });

    let proxy_settings = ProxySettings {
        allowed_domains: args.allow_domain.clone(),
        credentials: Vec::new(),
    };
    let mut proxy = silo_proxy::create_proxy(proxy_settings.clone(), journal.clone());
    let proxy_handle = proxy.start().await?;

    let sandbox_config = SandboxConfig {
        kind: crate::cli::resolve_sandbox_kind(args.sandbox),
        read_allowlist: args.allow_read.clone(),
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
    let shell_result = sandbox.user_shell(command).await;

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
