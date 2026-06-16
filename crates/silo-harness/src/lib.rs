//! The harness: coordinates one frontend, one LLM backend (plus its
//! subagents), one sandbox, the egress proxy, and the journal, until the
//! frontend requests shutdown or a signal arrives.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};

use silo_core::clock::{FakeClock, RealClock, SharedClock};
use silo_core::config::{HarnessConfig, SandboxKind};
use silo_core::error::{FrontendError, HarnessError};
use silo_core::event::{EventBus, EventPayload};
use silo_core::journal::{JournalEntry, JournalHandle, JournalWriter};
use silo_core::replay::SharedScript;
use silo_core::tool::{ToolOwner, ToolRegistry};
use silo_core::traits::{EgressProxy, Frontend, FrontendContext, Sandbox};
use silo_workspace::{AttachedWorkspace, WorkspaceManager};

mod agent;
mod prompts;
mod shutdown;
mod uploads;
mod validate;

pub use validate::{validate_journal_path, validate_read_allowlist};

/// Result of a completed harness session.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HarnessOutcome {
    /// Final message (from the Exit tool or the shutdown request), if any.
    pub message: Option<String>,
    /// Path of the journal written for this session, if the harness created
    /// a journal file.
    pub journal_path: Option<std::path::PathBuf>,
    /// The last failure message when the session ended through the
    /// consecutive-LLM-failure path (the headless first failure, or the
    /// failure cap for other frontends); `None` otherwise.
    pub llm_failure: Option<String>,
    /// Why the scripted session failed, when a script was supplied: a mock
    /// LLM or sandbox mismatch ended the session, or script entries were
    /// left unconsumed at session end. Carries the mismatch detail (when
    /// there is one) and the remaining-entry summary. `None` when the
    /// script was consumed exactly, or when no script was supplied.
    pub script_failure: Option<String>,
}

/// Per-run settings that are not part of the persistent [`HarnessConfig`].
#[derive(Default)]
pub struct RunOptions {
    /// Shared test script; required when any mock component is configured.
    pub script: Option<SharedScript>,
    /// Use the fake clock (sequence numbers only, no wall-clock
    /// timestamps), so journals are byte-stable. Also disables OS signal
    /// handling.
    pub deterministic: bool,
    /// Use the mock proxy even with a real sandbox backend.
    pub mock_proxy: bool,
    /// Read allowlist entries accepted despite risk-scan hits.
    pub allow_risky_paths: Vec<PathBuf>,
    /// Journal destination override. When set, the harness writes to this
    /// handle (tests use an in-memory journal) and `journal_path` in the
    /// outcome is `None`.
    pub journal: Option<JournalHandle>,
    /// State directory override. Defaults to `silo_core::paths::state_dir()`.
    pub state_dir: Option<PathBuf>,
    /// Home directory used for the read-allowlist risk scan. Defaults to
    /// the user's home directory.
    pub risk_scan_home: Option<PathBuf>,
    /// Fired once the frontend has started (the run file exists for the
    /// interactive frontend).
    pub notify_started: Option<oneshot::Sender<()>>,
}

/// Runs one harness session to completion.
pub async fn run(
    config: HarnessConfig,
    mut options: RunOptions,
) -> Result<HarnessOutcome, HarnessError> {
    let state_dir = options
        .state_dir
        .clone()
        .unwrap_or_else(silo_core::paths::state_dir);
    std::fs::create_dir_all(&state_dir)?;

    let clock: SharedClock = if options.deterministic {
        Arc::new(FakeClock::default())
    } else {
        Arc::new(RealClock::default())
    };

    let (journal, journal_path) = match &options.journal {
        Some(handle) => (handle.clone(), None),
        None => {
            let path = config.logging.journal_path.clone().unwrap_or_else(|| {
                silo_core::paths::journals_dir(&state_dir)
                    .join(format!("{}.jsonl", config.harness_id))
            });
            validate::validate_journal_path(&path, &config.sandbox.read_allowlist)?;
            let writer = JournalWriter::to_file(&path, clock.clone())?;
            (JournalHandle::new(writer), Some(path))
        }
    };

    journal.append(JournalEntry::Meta {
        harness_id: config.harness_id.clone(),
        harness_version: env!("CARGO_PKG_VERSION").to_string(),
        config_summary: config.summary(),
    });
    journal.append(JournalEntry::Lifecycle {
        message: "harness starting".into(),
    });

    let home = options
        .risk_scan_home
        .clone()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."));
    let allowlist_warnings = validate_read_allowlist(
        &config.sandbox.read_allowlist,
        &home,
        &state_dir,
        &options.allow_risky_paths,
    )?;
    for warning in &allowlist_warnings {
        eprintln!("warning: {warning}");
    }

    let bus = EventBus::new(clock.clone(), journal.clone());

    let manager = WorkspaceManager::new(state_dir.clone());
    let attached = manager.attach(&config.workspace, &config.harness_id)?;

    let mut sandbox_config = config.sandbox.clone();
    sandbox_config.workspace_mount = Some(attached.mount_path.clone());

    // Proxy.
    let use_mock_proxy = options.mock_proxy || matches!(config.sandbox.kind, SandboxKind::Mock);
    let mut proxy: Box<dyn EgressProxy> = if use_mock_proxy {
        silo_proxy::create_mock_proxy(sandbox_config.proxy.clone(), journal.clone())
    } else {
        silo_proxy::create_proxy(sandbox_config.proxy.clone(), journal.clone())
    };
    let proxy_handle = match proxy.start().await {
        Ok(handle) => handle,
        Err(error) => {
            let error: HarnessError = error.into();
            let _ = finish(
                Some(&bus),
                &journal,
                None,
                None,
                Some(proxy.as_mut()),
                Some(attached),
                None,
            )
            .await;
            return Err(error);
        }
    };

    // Sandbox.
    let mut sandbox: Box<dyn Sandbox> = match silo_sandbox::create_sandbox(
        &sandbox_config,
        Some(proxy_handle),
        options.script.clone(),
        journal.clone(),
    )
    .await
    {
        Ok(sandbox) => sandbox,
        Err(error) => {
            let error: HarnessError = error.into();
            let _ = finish(
                Some(&bus),
                &journal,
                None,
                None,
                Some(proxy.as_mut()),
                Some(attached),
                None,
            )
            .await;
            return Err(error);
        }
    };
    if let Err(error) = sandbox.start().await {
        let error: HarnessError = error.into();
        let _ = finish(
            Some(&bus),
            &journal,
            None,
            Some(sandbox.as_mut()),
            Some(proxy.as_mut()),
            Some(attached),
            None,
        )
        .await;
        return Err(error);
    }
    let access = sandbox.access_report();

    // Frontend.
    let (commands_tx, commands_rx) = mpsc::channel(16);
    let shutdown = shutdown::ShutdownSignal::new(commands_rx, journal.clone());
    let signal_task = if options.deterministic {
        None
    } else {
        Some(shutdown::spawn_signal_listener(shutdown.clone()))
    };

    let mut frontend: Box<dyn Frontend> =
        match silo_frontend::create_frontend(&config.frontend, options.script.clone()) {
            Ok(frontend) => frontend,
            Err(error) => {
                let error: HarnessError = error.into();
                let _ = finish(
                    Some(&bus),
                    &journal,
                    None,
                    Some(sandbox.as_mut()),
                    Some(proxy.as_mut()),
                    Some(attached),
                    None,
                )
                .await;
                return Err(error);
            }
        };
    let frontend_context = FrontendContext {
        harness_id: config.harness_id.clone(),
        bus: bus.clone(),
        commands: commands_tx.clone(),
        access: access.clone(),
        state_dir: state_dir.clone(),
        workspace: config.workspace.display().to_string(),
        configured_read_allowlist: config
            .sandbox
            .read_allowlist
            .iter()
            .map(|path| path.display().to_string())
            .collect(),
    };
    if let Err(error) = frontend.start(frontend_context).await {
        let error: HarnessError = error.into();
        let _ = finish(
            Some(&bus),
            &journal,
            Some(frontend.as_mut()),
            Some(sandbox.as_mut()),
            Some(proxy.as_mut()),
            Some(attached),
            None,
        )
        .await;
        return Err(error);
    }

    // Tool registry.
    let mut registry = ToolRegistry::new();
    registry.add_all(sandbox.tool_defs(), ToolOwner::Sandbox);
    registry.add_all(frontend.tool_defs(), ToolOwner::Frontend);
    registry.add(silo_llm::common::agent_tool_def(), ToolOwner::Harness);
    registry.add(silo_llm::common::await_agent_tool_def(), ToolOwner::Harness);
    let tool_names: Vec<String> = registry
        .entries()
        .iter()
        .map(|entry| entry.def.name.clone())
        .collect();
    let system = config
        .llm
        .system_prompt
        .clone()
        .unwrap_or_else(|| prompts::default_system_prompt(&access, &tool_names));

    // LLM backend.
    let backend = match silo_llm::create_backend(&config.llm, options.script.clone()).await {
        Ok(backend) => backend,
        Err(error) => {
            let error: HarnessError = error.into();
            let _ = finish(
                Some(&bus),
                &journal,
                Some(frontend.as_mut()),
                Some(sandbox.as_mut()),
                Some(proxy.as_mut()),
                Some(attached),
                None,
            )
            .await;
            return Err(error);
        }
    };

    bus.emit(EventPayload::HarnessStarted {
        harness_id: config.harness_id.clone(),
        workspace: config.workspace.display().to_string(),
        sandbox: sandbox.kind().to_string(),
        llm: backend.id(),
    });
    bus.emit(EventPayload::AccessReportUpdated {
        report: access.clone(),
    });
    if let Some(notify) = options.notify_started.take() {
        let _ = notify.send(());
    }

    // Subagents run on background tasks that clone the session context, so
    // the components they reach are shared through `Arc` for the session.
    // The sandbox and frontend are moved out of their boxes into `Arc`s for
    // the session and reclaimed afterwards: `drive` joins every background
    // task before it returns, so once the session loop ends no task holds a
    // clone and the `Arc` refcount is back to one.
    let sandbox: Arc<dyn Sandbox> = Arc::from(sandbox);
    let frontend: Arc<dyn Frontend> = Arc::from(frontend);

    // Session: the top-level agent loop runs alongside the upload
    // listener; the listener never completes on its own.
    let session_result: Result<agent::SessionEnd, HarnessError> = {
        let ctx = agent::SessionCtx::new(
            bus.clone(),
            journal.clone(),
            backend.clone(),
            sandbox.clone(),
            frontend.clone(),
            Arc::new(registry),
            shutdown.clone(),
            Arc::from(system.as_str()),
            config.llm.max_tokens,
        );
        tokio::select! {
            result = agent::top_level_loop(&ctx) => result,
            () = uploads::listen(&bus, sandbox.as_ref(), &journal) => {
                Err(HarnessError::Other("the upload listener stopped unexpectedly".into()))
            }
        }
    };

    if let Some(task) = signal_task {
        task.abort();
    }

    match session_result {
        Ok(end) => {
            // A session ended by consecutive LLM failures or a script
            // mismatch carries the failure in the outcome; frontends get
            // no final message for it.
            let final_message = if end.llm_failure.is_some() || end.script_mismatch.is_some() {
                None
            } else {
                end.message.clone()
            };
            let finish_result = finish_shared(
                &bus,
                &journal,
                frontend,
                sandbox,
                proxy.as_mut(),
                attached,
                final_message,
            )
            .await;
            // Scripted sessions are self-checking: a mismatch ending is a
            // script failure, and so is any script entry left unconsumed
            // at session end. The check runs after `finish` so a trailing
            // ExpectShutdown step has been consumed. A script mismatch
            // raised by `finish` itself (for example a trailing
            // ExpectShutdown whose message check fails because the failure
            // suppressed the final message) folds into the same script
            // failure rather than masking it as a generic error.
            let finish_script_mismatch = matches!(
                &finish_result,
                Err(HarnessError::Frontend(FrontendError::ScriptMismatch(_)))
            );
            // Propagate a finish error unless it is the expected script
            // mismatch on a scripted run, which is folded into the script
            // failure below.
            if !(options.script.is_some() && finish_script_mismatch) {
                finish_result?;
            }
            let script_failure = options.script.as_ref().and_then(|script| {
                if let Some(detail) = &end.script_mismatch {
                    Some(format!(
                        "{detail}; remaining: {}",
                        script.remaining_summary()
                    ))
                } else if finish_script_mismatch {
                    Some(format!(
                        "frontend shutdown step did not match; remaining: {}",
                        script.remaining_summary()
                    ))
                } else if script.finished() {
                    None
                } else {
                    Some(format!(
                        "script entries left unconsumed; remaining: {}",
                        script.remaining_summary()
                    ))
                }
            });
            Ok(HarnessOutcome {
                message: end.message,
                journal_path,
                llm_failure: end.llm_failure,
                script_failure,
            })
        }
        Err(error) => {
            bus.emit(EventPayload::Error {
                context: "harness".into(),
                message: error.to_string(),
            });
            let _ = finish_shared(
                &bus,
                &journal,
                frontend,
                sandbox,
                proxy.as_mut(),
                attached,
                None,
            )
            .await;
            Err(error)
        }
    }
}

/// Post-session shutdown: reclaims exclusive access to the sandbox and
/// frontend (every background task and the session context have been
/// dropped by now, so each `Arc` holds the only reference) and runs the
/// shared [`finish`] routine. The `Arc`s are owned here and dropped only
/// after `finish` returns, so the `get_mut` borrows never overlap their
/// `Drop`.
async fn finish_shared(
    bus: &EventBus,
    journal: &JournalHandle,
    frontend: Arc<dyn Frontend>,
    sandbox: Arc<dyn Sandbox>,
    proxy: &mut dyn EgressProxy,
    attached: AttachedWorkspace,
    message: Option<String>,
) -> Result<(), HarnessError> {
    // Hold the Arcs in `Option`s so they can be `take`n (and their `Drop`
    // run) explicitly after the borrows are done, sidestepping the
    // conservative interaction between `Arc`'s significant destructor and a
    // `get_mut` borrow held across an await.
    let mut frontend = Some(frontend);
    let mut sandbox = Some(sandbox);
    let frontend_mut = frontend
        .as_mut()
        .and_then(Arc::get_mut)
        .map(|f| f as &mut dyn Frontend);
    let sandbox_mut = sandbox
        .as_mut()
        .and_then(Arc::get_mut)
        .map(|s| s as &mut dyn Sandbox);
    if frontend_mut.is_none() || sandbox_mut.is_none() {
        // A background task outlived the session loop, against the
        // invariant that `drive` joins every child before returning.
        tracing::error!("a subagent task outlived the session; shutdown may be incomplete");
    }
    let result = finish(
        Some(bus),
        journal,
        frontend_mut,
        sandbox_mut,
        Some(proxy),
        Some(attached),
        message,
    )
    .await;
    frontend.take();
    sandbox.take();
    result
}

/// Shuts down every started component, detaches the workspace, and writes
/// the final journal entry. All steps run even when one fails; the first
/// failure is returned. The components are passed by mutable reference, so
/// the same routine serves both the setup error paths (which still own the
/// boxed components) and the post-session path (where the sandbox and
/// frontend are shared through `Arc` and reclaimed once every background
/// task has finished).
async fn finish(
    bus: Option<&EventBus>,
    journal: &JournalHandle,
    frontend: Option<&mut dyn Frontend>,
    sandbox: Option<&mut dyn Sandbox>,
    proxy: Option<&mut dyn EgressProxy>,
    attached: Option<AttachedWorkspace>,
    message: Option<String>,
) -> Result<(), HarnessError> {
    if let Some(bus) = bus {
        bus.emit(EventPayload::Shutdown {
            message: message.clone(),
        });
    }
    let mut first_error: Option<HarnessError> = None;
    if let Some(frontend) = frontend {
        if let Err(error) = frontend.shutdown(message.clone()).await {
            tracing::warn!("frontend shutdown failed: {error}");
            first_error.get_or_insert(error.into());
        }
    }
    if let Some(sandbox) = sandbox {
        if let Err(error) = sandbox.shutdown().await {
            tracing::warn!("sandbox shutdown failed: {error}");
            first_error.get_or_insert(error.into());
        }
    }
    if let Some(proxy) = proxy {
        if let Err(error) = proxy.shutdown().await {
            tracing::warn!("proxy shutdown failed: {error}");
            first_error.get_or_insert(error.into());
        }
    }
    if let Some(attached) = attached {
        attached.detach();
    }
    journal.append(JournalEntry::Lifecycle {
        message: "shutdown".into(),
    });
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}
