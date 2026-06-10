//! Interactive colorful terminal client for the harness's interactive
//! WebSocket frontend.
//!
//! Connects to a local harness discovered through the state directory's run
//! files, or to a remote harness by address and certificate fingerprint
//! (pairing on first use, challenge-signature login afterwards).

mod app;
mod commands;
mod net;
mod ui;

use std::io::Stdout;
use std::path::Path;

use anyhow::{anyhow, bail, Context};
use clap::Parser;
use crossterm::event::{Event as TermEvent, EventStream, KeyCode, KeyEventKind};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::tty::IsTty;
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use silo_core::protocol::RunInfo;
use silo_frontend::client::{
    generate_signing_key, list_local_harnesses, load_signing_key, save_signing_key,
};

use crate::app::App;
use crate::net::{AuthSpec, ConnectTarget};

#[derive(Debug, Parser)]
#[command(
    name = "silo-tui",
    about = "Terminal client for llmdevsilo interactive harnesses"
)]
struct Args {
    /// Connect to a local harness by id (discovered from the state
    /// directory's run files).
    #[arg(long, conflicts_with_all = ["url", "fingerprint", "pair"])]
    harness: Option<String>,

    /// Connect to a remote harness at host:port (or a wss:// URL).
    #[arg(long)]
    url: Option<String>,

    /// SHA-256 fingerprint (hex) of the server's TLS certificate. Stored in
    /// known-hosts after the first successful connection.
    #[arg(long, requires = "url")]
    fingerprint: Option<String>,

    /// One-time pairing code; generates and registers a new client key.
    #[arg(long, requires = "url")]
    pair: Option<String>,

    /// Start with debug mode on: raw ids in the status bar, transcript,
    /// and harness picker. Toggled at runtime with /debug.
    #[arg(long)]
    debug: bool,
}

enum Launch {
    Connect(Box<ConnectTarget>),
    Choose(Vec<RunInfo>),
}

fn target_from_run_info(info: &RunInfo) -> anyhow::Result<Box<ConnectTarget>> {
    let token = std::fs::read_to_string(&info.local_token_path)
        .with_context(|| {
            format!(
                "cannot read the local token for harness {} at {}",
                info.harness_id, info.local_token_path
            )
        })?
        .trim()
        .to_string();
    Ok(Box::new(ConnectTarget {
        addr: info.addr.clone(),
        fingerprint: info.cert_fingerprint_sha256.clone(),
        auth: AuthSpec::LocalToken { token },
        persist_state_dir: None,
        workspace: Some(info.workspace.clone()),
    }))
}

fn pairing_client_name() -> String {
    match std::env::var("USER") {
        Ok(user) if !user.is_empty() => format!("silo-tui ({user})"),
        _ => "silo-tui".to_string(),
    }
}

fn resolve_remote(args: &Args, state_dir: &Path, url: &str) -> anyhow::Result<Box<ConnectTarget>> {
    let stored = net::lookup_host(state_dir, url)?;
    let fingerprint = match (&args.fingerprint, &stored) {
        (Some(flag), Some(known)) => {
            if !flag.trim().eq_ignore_ascii_case(known) {
                bail!(
                    "the fingerprint for {url} does not match the one stored in known-hosts; \
                     verify the server identity, then update {} if the certificate \
                     legitimately changed",
                    net::known_hosts_path(state_dir).display()
                );
            }
            flag.trim().to_string()
        }
        (Some(flag), None) => flag.trim().to_string(),
        (None, Some(known)) => known.clone(),
        (None, None) => bail!(
            "no stored fingerprint for {url}; pass --fingerprint <hex> \
             (shown by the harness, or in its run file)"
        ),
    };
    let auth = if let Some(code) = &args.pair {
        let key = generate_signing_key();
        let path = net::key_path(state_dir, url);
        save_signing_key(&path, &key)
            .map_err(|e| anyhow!("cannot save the client key at {}: {e}", path.display()))?;
        AuthSpec::Pair {
            code: code.clone(),
            key,
            client_name: pairing_client_name(),
        }
    } else {
        let path = net::key_path(state_dir, url);
        let key = load_signing_key(&path).map_err(|e| {
            anyhow!(
                "no usable client key for {url} ({e}); connect once with \
                 --pair <code> to register this device"
            )
        })?;
        let key_id = net::load_key_id(state_dir, url)?.ok_or_else(|| {
            anyhow!(
                "no stored key id for {url}; connect once with --pair <code> \
                 to register this device"
            )
        })?;
        AuthSpec::Key { key, key_id }
    };
    Ok(Box::new(ConnectTarget {
        addr: url.to_string(),
        fingerprint,
        auth,
        persist_state_dir: Some(state_dir.to_path_buf()),
        workspace: None,
    }))
}

fn resolve(args: &Args, state_dir: &Path) -> anyhow::Result<Launch> {
    if let Some(url) = &args.url {
        return Ok(Launch::Connect(resolve_remote(args, state_dir, url)?));
    }
    let harnesses = list_local_harnesses(state_dir);
    if let Some(id) = &args.harness {
        let info = harnesses
            .iter()
            .find(|h| &h.harness_id == id)
            .ok_or_else(|| {
                let available: Vec<&str> =
                    harnesses.iter().map(|h| h.harness_id.as_str()).collect();
                if available.is_empty() {
                    anyhow!("harness {id} not found: no local harnesses are running")
                } else {
                    anyhow!(
                        "harness {id} not found; running harnesses: {}",
                        available.join(", ")
                    )
                }
            })?;
        return Ok(Launch::Connect(target_from_run_info(info)?));
    }
    match harnesses.len() {
        0 => bail!(
            "no local harnesses are running; start one with `silo run`, or \
             connect to a remote harness with --url <host:port> --fingerprint <hex>"
        ),
        1 => Ok(Launch::Connect(target_from_run_info(&harnesses[0])?)),
        _ => Ok(Launch::Choose(harnesses)),
    }
}

type Term = Terminal<CrosstermBackend<Stdout>>;

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = crossterm::execute!(std::io::stdout(), LeaveAlternateScreen);
}

/// Restores the terminal when dropped, so every exit path (including `?`
/// early returns) leaves the shell usable.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

fn setup_terminal() -> anyhow::Result<(Term, TerminalGuard)> {
    enable_raw_mode().context("cannot enable raw terminal mode")?;
    let guard = TerminalGuard;
    crossterm::execute!(std::io::stdout(), EnterAlternateScreen)
        .context("cannot enter the alternate screen")?;
    let terminal =
        Terminal::new(CrosstermBackend::new(std::io::stdout())).context("terminal setup failed")?;
    Ok((terminal, guard))
}

/// Blocking selection screen shown when several local harnesses are
/// running. Harnesses are identified by their workspace folder name; the
/// raw harness id is added in debug mode. Returns `None` when the user
/// cancels.
fn select_harness(
    terminal: &mut Term,
    harnesses: &[RunInfo],
    debug: bool,
) -> anyhow::Result<Option<RunInfo>> {
    use ratatui::layout::{Constraint, Layout};
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::Line;
    use ratatui::widgets::{Block, Borders, Paragraph};

    let mut selected = 0usize;
    loop {
        terminal.draw(|frame| {
            let [title_area, list_area, hint_area] = Layout::vertical([
                Constraint::Length(2),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .areas(frame.area());
            frame.render_widget(
                Paragraph::new(Line::styled(
                    "select a harness",
                    Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                )),
                title_area,
            );
            let lines: Vec<Line> = harnesses
                .iter()
                .enumerate()
                .map(|(index, info)| {
                    let marker = if index == selected { "> " } else { "  " };
                    let style = if index == selected {
                        Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD)
                    } else {
                        Style::new()
                    };
                    let name = crate::app::workspace_folder_name(&info.workspace);
                    let mut line = format!("{marker}{name}  {}  {}", info.addr, info.workspace);
                    if debug {
                        line.push_str(&format!("  [{}]", info.harness_id));
                    }
                    Line::styled(line, style)
                })
                .collect();
            frame.render_widget(
                Paragraph::new(lines).block(Block::new().borders(Borders::ALL)),
                list_area,
            );
            frame.render_widget(
                Paragraph::new(Line::styled(
                    "up/down: select · enter: connect · q: quit",
                    Style::new().add_modifier(Modifier::DIM),
                )),
                hint_area,
            );
        })?;
        if let TermEvent::Key(key) = crossterm::event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match key.code {
                KeyCode::Up => selected = selected.saturating_sub(1),
                KeyCode::Down => selected = (selected + 1).min(harnesses.len() - 1),
                KeyCode::Enter => return Ok(Some(harnesses[selected].clone())),
                KeyCode::Esc | KeyCode::Char('q') => return Ok(None),
                KeyCode::Char('c')
                    if key
                        .modifiers
                        .contains(crossterm::event::KeyModifiers::CONTROL) =>
                {
                    return Ok(None)
                }
                _ => {}
            }
        }
    }
}

async fn event_loop(
    terminal: &mut Term,
    app: &mut App,
    target: ConnectTarget,
) -> anyhow::Result<()> {
    let (cmd_tx, mut net_rx, net_task) = net::spawn(target);
    let mut term_events = EventStream::new();
    let mut net_open = true;
    terminal.draw(|frame| ui::draw(frame, app))?;
    loop {
        tokio::select! {
            net_event = net_rx.recv(), if net_open => match net_event {
                Some(event) => app.handle_net(event),
                None => net_open = false,
            },
            term_event = term_events.next() => match term_event {
                Some(Ok(TermEvent::Key(key))) => {
                    for message in app.handle_key(key) {
                        let _ = cmd_tx.send(message);
                    }
                }
                Some(Ok(_)) => {}
                Some(Err(e)) => bail!("terminal input failed: {e}"),
                None => break,
            },
        }
        // Batch any further pending network traffic into this redraw.
        while let Ok(event) = net_rx.try_recv() {
            app.handle_net(event);
        }
        if app.should_quit {
            break;
        }
        terminal.draw(|frame| ui::draw(frame, app))?;
    }
    net_task.abort();
    Ok(())
}

async fn run(args: Args) -> anyhow::Result<()> {
    let state_dir = silo_core::paths::state_dir();
    let launch = resolve(&args, &state_dir)?;
    if !std::io::stdout().is_tty() {
        bail!("silo-tui needs an interactive terminal");
    }

    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        default_hook(info);
    }));

    let (mut terminal, guard) = setup_terminal()?;
    let target = match launch {
        Launch::Connect(target) => target,
        Launch::Choose(harnesses) => match select_harness(&mut terminal, &harnesses, args.debug)? {
            Some(info) => target_from_run_info(&info)?,
            None => return Ok(()),
        },
    };

    let mut app = App::new(
        target.addr.clone(),
        target.fingerprint.clone(),
        target.workspace.clone(),
    );
    app.debug = args.debug;
    let result = event_loop(&mut terminal, &mut app, *target).await;
    drop(guard);
    result?;
    if let Some(message) = app.fatal {
        bail!(message);
    }
    Ok(())
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    if let Err(error) = run(args).await {
        eprintln!("silo-tui: {error:#}");
        std::process::exit(1);
    }
}
