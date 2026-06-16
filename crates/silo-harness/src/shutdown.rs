//! Shutdown and interrupt coordination for one harness session.
//!
//! A shutdown can be requested by a frontend command (the Exit tool or a
//! client), by an OS signal, or by the harness itself. The first request
//! wins; its message becomes the session's final message. The agent loop
//! polls [`ShutdownSignal::check`] before every LLM call and every tool
//! call, and the top-level loop awaits [`ShutdownSignal::wait`] while
//! waiting for user input, so a mid-turn request interrupts the session at
//! the next await point.
//!
//! Interrupts ([`FrontendCommand::Interrupt`]) are counted in a generation
//! watch. Each top-level turn snapshots the generation after draining the
//! command channel; an interrupt applies to the turn when the generation
//! grows past the snapshot, so an interrupt that arrived while the harness
//! was idle is consumed by the snapshot and does not abort the next turn.

use std::sync::Arc;

use tokio::sync::{mpsc, watch, Mutex};

use silo_core::journal::{JournalEntry, JournalHandle};
use silo_core::traits::FrontendCommand;

/// Why in-progress work should stop.
#[derive(Debug, PartialEq)]
pub(crate) enum AbortReason {
    /// A shutdown was requested, with its final message.
    Shutdown(Option<String>),
    /// The user interrupted the turn.
    Interrupted,
}

#[derive(Clone)]
pub(crate) struct ShutdownSignal {
    /// `Some(message)` once a shutdown has been requested.
    state: watch::Sender<Option<Option<String>>>,
    /// Count of interrupt commands applied so far.
    interrupts: watch::Sender<u64>,
    /// Frontend command channel. Set to `None` once disconnected.
    commands: Arc<Mutex<Option<mpsc::Receiver<FrontendCommand>>>>,
    journal: JournalHandle,
}

impl ShutdownSignal {
    pub fn new(commands: mpsc::Receiver<FrontendCommand>, journal: JournalHandle) -> Self {
        let (state, _) = watch::channel(None);
        let (interrupts, _) = watch::channel(0);
        ShutdownSignal {
            state,
            interrupts,
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
            FrontendCommand::Interrupt => {
                self.interrupts.send_modify(|generation| *generation += 1);
            }
        }
    }

    /// Applies every queued frontend command. Acquires the command receiver
    /// with `try_lock` so it never blocks behind a concurrent abort waiter
    /// that is parked on the receiver; when that waiter holds the lock it is
    /// already draining and applying commands into the watch channels, so
    /// skipping here loses nothing.
    async fn drain(&self) {
        let Ok(mut guard) = self.commands.try_lock() else {
            return;
        };
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
    }

    /// Drains any pending frontend commands, then reports whether a
    /// shutdown has been requested (and with which message).
    pub async fn check(&self) -> Option<Option<String>> {
        self.drain().await;
        self.state.borrow().clone()
    }

    /// Generation of the most recent interrupt. A turn snapshots this at
    /// its start (after a drain) and compares against it at every
    /// checkpoint.
    pub fn interrupt_generation(&self) -> u64 {
        *self.interrupts.borrow()
    }

    /// True when an interrupt newer than `generation` has been applied.
    /// Call after a drain (for example [`ShutdownSignal::check`]) so queued
    /// commands are counted.
    pub fn interrupted_since(&self, generation: u64) -> bool {
        *self.interrupts.borrow() > generation
    }

    /// Waits until in-progress work should stop: a shutdown request, or an
    /// interrupt newer than `generation`. Drains frontend commands while
    /// waiting; a shutdown wins over a co-queued interrupt.
    ///
    /// Several abort waiters can run at once (the top-level agent and its
    /// background subagents each have one in flight). Only one of them holds
    /// the single command receiver at a time: it is acquired with `try_lock`
    /// and, when another waiter already holds it, this one watches only the
    /// shutdown and interrupt channels. The holder applies each command into
    /// those channels, which wakes every waiter — so no waiter blocks behind
    /// another on the receiver lock.
    pub async fn wait_abort(&self, generation: u64) -> AbortReason {
        let mut state_rx = self.state.subscribe();
        let mut interrupt_rx = self.interrupts.subscribe();
        loop {
            self.drain().await;
            if let Some(message) = state_rx.borrow_and_update().clone() {
                return AbortReason::Shutdown(message);
            }
            if *interrupt_rx.borrow_and_update() > generation {
                return AbortReason::Interrupted;
            }
            match self.commands.try_lock() {
                Ok(mut guard) => match guard.as_mut() {
                    Some(rx) => {
                        tokio::select! {
                            _ = state_rx.changed() => {}
                            _ = interrupt_rx.changed() => {}
                            command = rx.recv() => match command {
                                Some(command) => self.apply(command),
                                None => *guard = None,
                            }
                        }
                    }
                    None => {
                        drop(guard);
                        tokio::select! {
                            _ = state_rx.changed() => {}
                            _ = interrupt_rx.changed() => {}
                        }
                    }
                },
                Err(_) => {
                    // Another waiter holds the command receiver; it applies
                    // commands into the watch channels, so waiting on those
                    // is enough.
                    tokio::select! {
                        _ = state_rx.changed() => {}
                        _ = interrupt_rx.changed() => {}
                    }
                }
            }
        }
    }

    /// Waits until a shutdown is requested and returns its message. Like
    /// [`ShutdownSignal::wait_abort`], it acquires the command receiver with
    /// `try_lock` so it never blocks behind a concurrent abort waiter.
    pub async fn wait(&self) -> Option<String> {
        let mut state_rx = self.state.subscribe();
        loop {
            if let Some(message) = state_rx.borrow_and_update().clone() {
                return message;
            }
            match self.commands.try_lock() {
                Ok(mut guard) => match guard.as_mut() {
                    Some(rx) => {
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
                    }
                    None => {
                        drop(guard);
                        if state_rx.changed().await.is_err() {
                            return None;
                        }
                    }
                },
                Err(_) => {
                    if state_rx.changed().await.is_err() {
                        return None;
                    }
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

    #[tokio::test]
    async fn interrupt_commands_bump_the_generation() {
        let (signal, tx) = signal_with_channel();
        assert_eq!(signal.interrupt_generation(), 0);
        tx.send(FrontendCommand::Interrupt).await.unwrap();
        // An interrupt is not a shutdown.
        assert_eq!(signal.check().await, None);
        assert_eq!(signal.interrupt_generation(), 1);
        assert!(signal.interrupted_since(0));
        assert!(!signal.interrupted_since(1));
    }

    #[tokio::test]
    async fn wait_abort_returns_interrupted_for_a_newer_generation() {
        let (signal, tx) = signal_with_channel();
        let waiter = signal.clone();
        let handle = tokio::spawn(async move { waiter.wait_abort(0).await });
        tx.send(FrontendCommand::Interrupt).await.unwrap();
        assert_eq!(handle.await.unwrap(), AbortReason::Interrupted);
    }

    #[tokio::test]
    async fn wait_abort_ignores_interrupts_at_or_below_the_generation() {
        let (signal, tx) = signal_with_channel();
        tx.send(FrontendCommand::Interrupt).await.unwrap();
        signal.check().await;
        assert_eq!(signal.interrupt_generation(), 1);
        let waiter = signal.clone();
        let handle = tokio::spawn(async move { waiter.wait_abort(1).await });
        tx.send(FrontendCommand::Interrupt).await.unwrap();
        assert_eq!(handle.await.unwrap(), AbortReason::Interrupted);
        assert_eq!(signal.interrupt_generation(), 2);
    }

    #[tokio::test]
    async fn wait_abort_prefers_shutdown_over_a_queued_interrupt() {
        let (signal, tx) = signal_with_channel();
        tx.send(FrontendCommand::Interrupt).await.unwrap();
        tx.send(FrontendCommand::Shutdown {
            message: Some("bye".into()),
        })
        .await
        .unwrap();
        assert_eq!(
            signal.wait_abort(0).await,
            AbortReason::Shutdown(Some("bye".into()))
        );
    }
}
