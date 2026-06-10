//! Interactive frontend: a TLS WebSocket server speaking
//! `silo_core::protocol`.
//!
//! `start` creates per-harness TLS material and a local auth token under
//! the state directory, binds the listener, writes the run file for local
//! client discovery, and begins accepting connections. Every authenticated
//! client receives the full event stream; prompts, uploads, and question
//! answers from any client are shared with all of them.

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{broadcast, mpsc, oneshot, watch, Mutex as TokioMutex};
use tokio::task::JoinHandle;
use tokio_rustls::TlsAcceptor;

use silo_core::config::FrontendConfig;
use silo_core::conversation::AgentId;
use silo_core::error::FrontendError;
use silo_core::event::{EventBus, EventPayload, FileOrigin};
use silo_core::protocol::{CostEntry, RunInfo};
use silo_core::sandbox::AccessReport;
use silo_core::tool::{ToolCall, ToolDef, ToolOutput};
use silo_core::traits::{Frontend, FrontendCommand, FrontendContext};

use crate::tools;

mod auth;
mod connection;
mod http;
mod tls;

pub fn create(config: &FrontendConfig) -> Result<Box<dyn Frontend>, FrontendError> {
    Ok(Box::new(InteractiveFrontend {
        config: config.clone(),
        started: None,
    }))
}

pub struct InteractiveFrontend {
    config: FrontendConfig,
    started: Option<Started>,
}

struct Started {
    shared: Arc<Shared>,
    prompt_rx: TokioMutex<mpsc::UnboundedReceiver<String>>,
    accept_task: JoinHandle<()>,
    cost_task: JoinHandle<()>,
    run_file: PathBuf,
}

/// Resolution of one pending AskUserQuestion.
enum QuestionOutcome {
    /// A client answered.
    Answered(String),
    /// A user interrupt cancelled the question.
    Interrupted,
}

/// State shared with the connection tasks.
struct Shared {
    harness_id: String,
    /// Address the listener is bound to; the landing page falls back to it
    /// when a request has no Host header.
    listen_addr: SocketAddr,
    bus: EventBus,
    commands: mpsc::Sender<FrontendCommand>,
    access: AccessReport,
    /// SHA-256 of the local token; tokens are compared by digest.
    token_digest: [u8; 32],
    keys: Mutex<auth::AuthorizedKeys>,
    pairing: Mutex<auth::PairingCodes>,
    /// Pending AskUserQuestion calls by question id. The first answer (or
    /// an interrupt) removes the entry.
    questions: Mutex<HashMap<String, oneshot::Sender<QuestionOutcome>>>,
    /// Latest CostReport per backend, maintained by the cost watcher task.
    cost: Mutex<BTreeMap<String, CostEntry>>,
    prompt_tx: mpsc::UnboundedSender<String>,
    shutdown_tx: watch::Sender<bool>,
    shutdown_message: Mutex<Option<String>>,
    conn_tasks: Mutex<Vec<JoinHandle<()>>>,
}

impl InteractiveFrontend {
    fn require_started(&self) -> Result<&Started, FrontendError> {
        self.started
            .as_ref()
            .ok_or_else(|| FrontendError::Setup("the interactive frontend is not running".into()))
    }
}

#[async_trait]
impl Frontend for InteractiveFrontend {
    fn kind(&self) -> &'static str {
        "interactive"
    }

    fn tool_defs(&self) -> Vec<ToolDef> {
        vec![tools::ask_user_question_def(), tools::send_user_file_def()]
    }

    async fn start(&mut self, ctx: FrontendContext) -> Result<(), FrontendError> {
        if self.started.is_some() {
            return Err(FrontendError::Setup(
                "the interactive frontend is already started".into(),
            ));
        }
        let harness_dir = silo_core::paths::harness_dir(&ctx.state_dir, &ctx.harness_id);
        std::fs::create_dir_all(&harness_dir)?;
        let material = match (&self.config.tls_cert_path, &self.config.tls_key_path) {
            (Some(cert_path), Some(key_path)) => tls::load_pair(cert_path, key_path)?,
            (None, None) => tls::load_or_create(&harness_dir)?,
            (Some(_), None) => {
                return Err(FrontendError::Setup(
                    "tls_cert_path is set without tls_key_path; supply both or neither".into(),
                ))
            }
            (None, Some(_)) => {
                return Err(FrontendError::Setup(
                    "tls_key_path is set without tls_cert_path; supply both or neither".into(),
                ))
            }
        };
        let token_path = harness_dir.join("local-token");
        let token = auth::load_or_create_token(&token_path)?;
        let keys = auth::AuthorizedKeys::load(&harness_dir.join("authorized-keys.json"))?;

        let listen_addr = self
            .config
            .listen_addr
            .unwrap_or_else(|| SocketAddr::from(([127, 0, 0, 1], 0)));
        let listener = tokio::net::TcpListener::bind(listen_addr).await?;
        let local_addr = listener.local_addr()?;

        let runs_dir = silo_core::paths::runs_dir(&ctx.state_dir);
        std::fs::create_dir_all(&runs_dir)?;
        let run_file = runs_dir.join(format!("{}.json", ctx.harness_id));
        let run_info = RunInfo {
            harness_id: ctx.harness_id.clone(),
            addr: local_addr.to_string(),
            cert_fingerprint_sha256: material.fingerprint_hex.clone(),
            local_token_path: token_path.to_string_lossy().into_owned(),
            pid: std::process::id(),
            workspace: ctx.workspace.clone(),
        };
        let run_json = serde_json::to_string_pretty(&run_info)
            .map_err(|e| FrontendError::Setup(format!("unserializable run info: {e}")))?;
        std::fs::write(&run_file, run_json)?;

        let (prompt_tx, prompt_rx) = mpsc::unbounded_channel();
        let (shutdown_tx, _) = watch::channel(false);
        let shared = Arc::new(Shared {
            harness_id: ctx.harness_id,
            listen_addr: local_addr,
            bus: ctx.bus,
            commands: ctx.commands,
            access: ctx.access,
            token_digest: auth::sha256_digest(token.as_bytes()),
            keys: Mutex::new(keys),
            pairing: Mutex::new(auth::PairingCodes::new()),
            questions: Mutex::new(HashMap::new()),
            cost: Mutex::new(BTreeMap::new()),
            prompt_tx,
            shutdown_tx,
            shutdown_message: Mutex::new(None),
            conn_tasks: Mutex::new(Vec::new()),
        });

        let acceptor = TlsAcceptor::from(Arc::new(tls::server_config(&material)?));
        let cost_task = tokio::spawn(cost_watcher(shared.clone()));
        let accept_task = tokio::spawn(connection::accept_loop(listener, acceptor, shared.clone()));

        if self.config.issue_pairing_code {
            let code = shared
                .pairing
                .lock()
                .expect("pairing codes poisoned")
                .mint();
            println!("Interactive frontend listening on {local_addr}");
            println!(
                "Certificate fingerprint (SHA-256): {}",
                material.fingerprint_hex
            );
            println!(
                "Pairing code (single use, valid for {} seconds): {code}",
                auth::PAIRING_CODE_TTL.as_secs()
            );
        }

        self.started = Some(Started {
            shared,
            prompt_rx: TokioMutex::new(prompt_rx),
            accept_task,
            cost_task,
            run_file,
        });
        Ok(())
    }

    async fn next_user_input(&self) -> Result<String, FrontendError> {
        let started = self.require_started()?;
        let mut shutdown_rx = started.shared.shutdown_tx.subscribe();
        if *shutdown_rx.borrow_and_update() {
            return Err(FrontendError::Closed(
                "the frontend is shutting down".into(),
            ));
        }
        let mut prompt_rx = started.prompt_rx.lock().await;
        tokio::select! {
            _ = shutdown_rx.changed() => {
                Err(FrontendError::Closed("the frontend is shutting down".into()))
            }
            prompt = prompt_rx.recv() => prompt.ok_or_else(|| {
                FrontendError::Closed("the prompt queue is closed".into())
            }),
        }
    }

    async fn run_tool(
        &self,
        agent: &AgentId,
        call: &ToolCall,
    ) -> Result<ToolOutput, FrontendError> {
        let started = self.require_started()?;
        let shared = &started.shared;
        match call.name.as_str() {
            "AskUserQuestion" => {
                let question = match tools::parse_question(&call.input) {
                    Ok(question) => question,
                    Err(error) => return Ok(ToolOutput::error(error)),
                };
                let mut shutdown_rx = shared.shutdown_tx.subscribe();
                if *shutdown_rx.borrow_and_update() {
                    return Err(FrontendError::Closed(
                        "the frontend is shutting down".into(),
                    ));
                }
                let id = silo_core::short_id();
                let (answer_tx, answer_rx) = oneshot::channel();
                shared
                    .questions
                    .lock()
                    .expect("questions poisoned")
                    .insert(id.clone(), answer_tx);
                shared.bus.emit(EventPayload::QuestionAsked {
                    id: id.clone(),
                    agent: agent.clone(),
                    question,
                });
                tokio::select! {
                    answer = answer_rx => match answer {
                        Ok(QuestionOutcome::Answered(answer)) => Ok(ToolOutput::ok(answer)),
                        Ok(QuestionOutcome::Interrupted) => {
                            Ok(ToolOutput::error(silo_core::tool::INTERRUPTED_BY_USER))
                        }
                        Err(_) => Err(FrontendError::Closed(
                            "the question was cancelled by shutdown".into(),
                        )),
                    },
                    _ = shutdown_rx.changed() => {
                        shared
                            .questions
                            .lock()
                            .expect("questions poisoned")
                            .remove(&id);
                        Err(FrontendError::Closed("the frontend is shutting down".into()))
                    }
                }
            }
            "SendUserFile" => {
                let file = match tools::parse_send_user_file(&call.input) {
                    Ok(file) => file,
                    Err(error) => return Ok(ToolOutput::error(error)),
                };
                shared.bus.emit(EventPayload::FileShared {
                    name: file.name.clone(),
                    content_b64: file.content_b64,
                    origin: FileOrigin::Llm {
                        agent: agent.clone(),
                    },
                });
                Ok(ToolOutput::ok(format!("sent {} to the user", file.name)))
            }
            other => Ok(ToolOutput::error(format!("unknown frontend tool: {other}"))),
        }
    }

    /// Resolves every pending question as interrupted and emits a
    /// QuestionAnswered event for each, so all clients drop the question
    /// UI. The blocked AskUserQuestion calls return the interrupted tool
    /// result.
    async fn interrupt(&self) -> Result<(), FrontendError> {
        let started = self.require_started()?;
        let shared = &started.shared;
        let pending: Vec<(String, oneshot::Sender<QuestionOutcome>)> = shared
            .questions
            .lock()
            .expect("questions poisoned")
            .drain()
            .collect();
        for (id, sender) in pending {
            if sender.send(QuestionOutcome::Interrupted).is_ok() {
                shared.bus.emit(EventPayload::QuestionAnswered {
                    id,
                    client_id: None,
                    answer: silo_core::event::INTERRUPTED_ANSWER.into(),
                });
            }
        }
        Ok(())
    }

    async fn shutdown(&mut self, message: Option<String>) -> Result<(), FrontendError> {
        let Some(started) = self.started.take() else {
            return Ok(());
        };
        *started
            .shared
            .shutdown_message
            .lock()
            .expect("shutdown message poisoned") = message;
        let _ = started.shared.shutdown_tx.send(true);
        started.accept_task.abort();
        started.cost_task.abort();
        // Dropping the answer senders resolves pending AskUserQuestion
        // calls with a Closed error.
        started
            .shared
            .questions
            .lock()
            .expect("questions poisoned")
            .clear();
        let conn_tasks: Vec<JoinHandle<()>> = std::mem::take(
            &mut *started
                .shared
                .conn_tasks
                .lock()
                .expect("connection task list poisoned"),
        );
        for mut task in conn_tasks {
            // Connection tasks exit promptly once the shutdown signal is
            // observed; the timeout guards against an unresponsive peer.
            if tokio::time::timeout(Duration::from_secs(5), &mut task)
                .await
                .is_err()
            {
                task.abort();
            }
        }
        let _ = std::fs::remove_file(&started.run_file);
        Ok(())
    }
}

/// Caches the latest CostReport event per backend so RequestCost can be
/// answered without replaying history.
async fn cost_watcher(shared: Arc<Shared>) {
    let mut events_rx = shared.bus.subscribe();
    let mut next = 0u64;
    for event in shared.bus.since(0) {
        next = event.seq + 1;
        apply_cost(&shared, &event.payload);
    }
    let mut shutdown_rx = shared.shutdown_tx.subscribe();
    if *shutdown_rx.borrow_and_update() {
        return;
    }
    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => break,
            received = events_rx.recv() => match received {
                Ok(event) => {
                    if event.seq >= next {
                        next = event.seq + 1;
                        apply_cost(&shared, &event.payload);
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    for event in shared.bus.since(next) {
                        next = event.seq + 1;
                        apply_cost(&shared, &event.payload);
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
        }
    }
}

fn apply_cost(shared: &Shared, payload: &EventPayload) {
    if let EventPayload::CostReport {
        backend,
        usage,
        quota,
    } = payload
    {
        shared.cost.lock().expect("cost cache poisoned").insert(
            backend.clone(),
            CostEntry {
                backend: backend.clone(),
                usage: *usage,
                quota: *quota,
            },
        );
    }
}
