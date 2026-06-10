//! Shutdown coordination for one harness session.
//!
//! A shutdown can be requested by a frontend command (the Exit tool or a
//! client), by an OS signal, or by the harness itself. The first request
//! wins; its message becomes the session's final message. The agent loop
//! polls [`ShutdownSignal::check`] before every LLM call and every tool
//! call, and the top-level loop awaits [`ShutdownSignal::wait`] while
//! waiting for user input, so a mid-turn request interrupts the session at
//! the next await point.

use std::sync::Arc;

use tokio::sync::{mpsc, watch, Mutex};

use silo_core::journal::{JournalEntry, JournalHandle};
use silo_core::traits::FrontendCommand;

#[derive(Clone)]
pub(crate) struct ShutdownSignal {
    /// `Some(message)` once a shutdown has been requested.
    state: watch::Sender<Option<Option<String>>>,
    /// Frontend command channel. Set to `None` once disconnected.
    commands: Arc<Mutex<Option<mpsc::Receiver<FrontendCommand>>>>,
    journal: JournalHandle,
}

impl ShutdownSignal {
    pub fn new(commands: mpsc::Receiver<FrontendCommand>, journal: JournalHandle) -> Self {
        let (state, _) = watch::channel(None);
        ShutdownSignal {
            state,
            commands: Arc::new(Mutex::new(Some(commands))),
            journal,
        }
    }

    /// Records a shutdown request. The first request's message is kept;
    /// later requests are ignored.
    pub fn request(&self, message: Option<String>) {
        self.state.send_if_modified(|state| {
            if state.is_none() {
                *state = Some(message);
                true
            } else {
                false
            }
        });
    }

    fn apply(&self, command: FrontendCommand) {
        if let Ok(value) = serde_json::to_value(&command) {
            self.journal
                .append(JournalEntry::FrontendCommand { command: value });
        }
        match command {
            FrontendCommand::Shutdown { message } => self.request(message),
        }
    }

    /// Drains any pending frontend commands, then reports whether a
    /// shutdown has been requested (and with which message).
    pub async fn check(&self) -> Option<Option<String>> {
        let mut guard = self.commands.lock().await;
        let mut disconnected = false;
        if let Some(rx) = guard.as_mut() {
            loop {
                match rx.try_recv() {
                    Ok(command) => self.apply(command),
                    Err(mpsc::error::TryRecvError::Empty) => break,
                    Err(mpsc::error::TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }
        if disconnected {
            *guard = None;
        }
        drop(guard);
        self.state.borrow().clone()
    }

    /// Waits until a shutdown is requested and returns its message.
    pub async fn wait(&self) -> Option<String> {
        let mut state_rx = self.state.subscribe();
        loop {
            if let Some(message) = state_rx.borrow_and_update().clone() {
                return message;
            }
            let mut guard = self.commands.lock().await;
            if let Some(rx) = guard.as_mut() {
                tokio::select! {
                    changed = state_rx.changed() => {
                        if changed.is_err() {
                            return None;
                        }
                    }
                    command = rx.recv() => match command {
                        Some(command) => self.apply(command),
                        None => *guard = None,
                    }
                }
            } else {
                drop(guard);
                if state_rx.changed().await.is_err() {
                    return None;
                }
            }
        }
    }
}

/// Requests shutdown when the process receives SIGINT (Ctrl-C) or SIGTERM.
pub(crate) fn spawn_signal_listener(signal: ShutdownSignal) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let message = wait_for_signal().await;
        signal.request(Some(message));
    })
}

#[cfg(unix)]
async fn wait_for_signal() -> String {
    use tokio::signal::unix::{signal, SignalKind};
    match signal(SignalKind::terminate()) {
        Ok(mut term) => {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => "interrupted by signal".to_string(),
                _ = term.recv() => "terminated by signal".to_string(),
            }
        }
        Err(_) => {
            let _ = tokio::signal::ctrl_c().await;
            "interrupted by signal".to_string()
        }
    }
}

#[cfg(not(unix))]
async fn wait_for_signal() -> String {
    let _ = tokio::signal::ctrl_c().await;
    "interrupted by signal".to_string()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use silo_core::clock::FakeClock;

    use super::*;

    fn signal_with_channel() -> (ShutdownSignal, mpsc::Sender<FrontendCommand>) {
        let (tx, rx) = mpsc::channel(4);
        let journal = JournalHandle::disabled(Arc::new(FakeClock::default()));
        (ShutdownSignal::new(rx, journal), tx)
    }

    #[tokio::test]
    async fn first_request_wins() {
        let (signal, _tx) = signal_with_channel();
        assert_eq!(signal.check().await, None);
        signal.request(Some("first".into()));
        signal.request(Some("second".into()));
        assert_eq!(signal.check().await, Some(Some("first".into())));
    }

    #[tokio::test]
    async fn check_drains_frontend_commands() {
        let (signal, tx) = signal_with_channel();
        tx.send(FrontendCommand::Shutdown {
            message: Some("done".into()),
        })
        .await
        .unwrap();
        assert_eq!(signal.check().await, Some(Some("done".into())));
    }

    #[tokio::test]
    async fn wait_returns_after_a_command_arrives() {
        let (signal, tx) = signal_with_channel();
        let waiter = signal.clone();
        let handle = tokio::spawn(async move { waiter.wait().await });
        tx.send(FrontendCommand::Shutdown {
            message: Some("bye".into()),
        })
        .await
        .unwrap();
        assert_eq!(handle.await.unwrap(), Some("bye".into()));
    }

    #[tokio::test]
    async fn disconnected_channel_is_tolerated() {
        let (signal, tx) = signal_with_channel();
        drop(tx);
        assert_eq!(signal.check().await, None);
        signal.request(None);
        assert_eq!(signal.check().await, Some(None));
    }
}
