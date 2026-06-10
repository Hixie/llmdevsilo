//! Pure rendering: draws the app state onto a ratatui frame. No state is
//! mutated here, so the renderer can be exercised against a test backend.

use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::app::{App, ConnState, ItemKind, PendingQuestion, Popup};

pub fn draw(frame: &mut Frame, app: &App) {
    let [transcript_area, status_area, input_area] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    draw_transcript(frame, app, transcript_area);
    draw_status(frame, app, status_area);
    draw_input(frame, app, input_area);

    if let Some(question) = &app.question {
        draw_question(frame, question);
    }
    if let Some(popup) = &app.popup {
        draw_popup(frame, popup);
    }
}

fn style_for(kind: ItemKind) -> Style {
    match kind {
        ItemKind::Prompt => Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ItemKind::Assistant => Style::new().fg(Color::White),
        ItemKind::ToolUse => Style::new().fg(Color::Yellow).add_modifier(Modifier::DIM),
        ItemKind::ToolResult => Style::new().add_modifier(Modifier::DIM),
        ItemKind::AgentNote => Style::new().fg(Color::Magenta),
        ItemKind::Question => Style::new().fg(Color::Blue).add_modifier(Modifier::BOLD),
        ItemKind::Answer => Style::new().fg(Color::Blue),
        ItemKind::FileNote => Style::new().fg(Color::Green),
        ItemKind::System => Style::new().add_modifier(Modifier::DIM),
        ItemKind::Error => Style::new().fg(Color::Red),
        ItemKind::Shutdown => Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
    }
}

/// Greedy word wrap; words longer than the width are hard-broken.
pub fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_string()];
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_len = 0usize;
    for word in text.split(' ') {
        let mut word = word;
        loop {
            let word_len = word.chars().count();
            let sep = usize::from(current_len > 0);
            if current_len + sep + word_len <= width {
                if sep == 1 {
                    current.push(' ');
                }
                current.push_str(word);
                current_len += sep + word_len;
                break;
            }
            if word_len > width {
                // Hard-break an overlong word at the remaining width.
                let take = width.saturating_sub(current_len + sep);
                if take == 0 {
                    lines.push(std::mem::take(&mut current));
                    current_len = 0;
                    continue;
                }
                if sep == 1 {
                    current.push(' ');
                }
                let split_at = word
                    .char_indices()
                    .nth(take)
                    .map(|(i, _)| i)
                    .unwrap_or(word.len());
                current.push_str(&word[..split_at]);
                lines.push(std::mem::take(&mut current));
                current_len = 0;
                word = &word[split_at..];
                continue;
            }
            lines.push(std::mem::take(&mut current));
            current_len = 0;
        }
    }
    lines.push(current);
    lines
}

/// The transcript flattened into display lines for a given width.
fn transcript_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for item in &app.transcript {
        let style = style_for(item.kind);
        for raw in item.text.split('\n') {
            for wrapped in wrap_text(raw, width) {
                lines.push(Line::styled(wrapped, style));
            }
        }
    }
    lines
}

fn draw_transcript(frame: &mut Frame, app: &App, area: Rect) {
    let width = area.width.max(1) as usize;
    let height = area.height as usize;
    let lines = transcript_lines(app, width);
    let max_offset = lines.len().saturating_sub(height);
    let offset = max_offset.saturating_sub(app.scroll_from_bottom as usize);
    let end = (offset + height).min(lines.len());
    let visible = lines[offset..end].to_vec();
    frame.render_widget(Paragraph::new(visible), area);
}

fn conn_span(conn: &ConnState) -> Span<'static> {
    match conn {
        ConnState::Connecting { attempt: 0 } => {
            Span::styled("connecting…", Style::new().fg(Color::Yellow))
        }
        ConnState::Connecting { attempt } => Span::styled(
            format!("connecting… (retry {attempt})"),
            Style::new().fg(Color::Yellow),
        ),
        ConnState::Connected => Span::styled("connected", Style::new().fg(Color::Green)),
        ConnState::Reconnecting {
            reason,
            retry_in_secs,
        } => Span::styled(
            format!("disconnected ({reason}), retrying in {retry_in_secs}s"),
            Style::new().fg(Color::Yellow),
        ),
        ConnState::Closed { reason } => {
            Span::styled(format!("closed ({reason})"), Style::new().fg(Color::Red))
        }
    }
}

fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    let mut left = vec![
        Span::styled(
            format!(" {} ", app.harness_id),
            Style::new()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        conn_span(&app.conn),
    ];
    if app.question.is_some() {
        left.push(Span::styled(
            "  · question pending",
            Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD),
        ));
    } else if app.awaiting_input {
        left.push(Span::styled(
            "  · awaiting input",
            Style::new().fg(Color::Cyan),
        ));
    }
    let cost = crate::app::format_cost_summary(app.costs.values());
    let cost_width = (cost.chars().count() + 1).min(area.width as usize) as u16;
    let [left_area, cost_area] =
        Layout::horizontal([Constraint::Min(1), Constraint::Length(cost_width)]).areas(area);
    frame.render_widget(Paragraph::new(Line::from(left)), left_area);
    frame.render_widget(
        Paragraph::new(Line::styled(cost, Style::new().fg(Color::Green))),
        cost_area,
    );
}

fn draw_input(frame: &mut Frame, app: &App, area: Rect) {
    let width = area.width.saturating_sub(2) as usize;
    let chars: Vec<char> = app.input.chars().collect();
    // Keep the cursor visible by windowing long input lines.
    let start = app.cursor.saturating_sub(width.saturating_sub(1).max(1));
    let visible: String = chars.iter().skip(start).take(width.max(1)).collect();
    let line = Line::from(vec![
        Span::styled(
            "> ",
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Span::raw(visible),
    ]);
    frame.render_widget(Paragraph::new(line), area);
    if app.question.is_none() && app.popup.is_none() {
        let x = area.x + 2 + (app.cursor - start) as u16;
        frame.set_cursor_position(Position::new(x.min(area.right().saturating_sub(1)), area.y));
    }
}

/// A centered rectangle of the requested size, clamped to the frame.
fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

fn draw_modal(frame: &mut Frame, title: &str, lines: Vec<Line<'static>>, footer: &str) {
    let area = frame.area();
    let width = 72.min(area.width.saturating_sub(4)).max(20);
    let inner_width = width.saturating_sub(2) as usize;
    let mut wrapped: Vec<Line<'static>> = Vec::new();
    for line in lines {
        // Wrap single-span lines; styled multi-span lines pass through.
        if line.spans.len() == 1 {
            let style = line.spans[0].style;
            for piece in wrap_text(&line.spans[0].content, inner_width) {
                wrapped.push(Line::styled(piece, style));
            }
        } else {
            wrapped.push(line);
        }
    }
    wrapped.push(Line::raw(""));
    wrapped.push(Line::styled(
        footer.to_string(),
        Style::new().add_modifier(Modifier::DIM),
    ));
    let height = (wrapped.len() as u16 + 2).min(area.height.saturating_sub(2));
    let rect = centered_rect(area, width, height);
    frame.render_widget(Clear, rect);
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(Color::Cyan))
        .title(format!(" {title} "));
    frame.render_widget(Paragraph::new(wrapped).block(block), rect);
}

fn draw_question(frame: &mut Frame, pending: &PendingQuestion) {
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::styled(
        pending.question.question.clone(),
        Style::new().add_modifier(Modifier::BOLD),
    ));
    lines.push(Line::raw(""));
    if let Some(buffer) = &pending.free_text {
        lines.push(Line::from(vec![
            Span::styled("answer: ", Style::new().fg(Color::Cyan)),
            Span::raw(buffer.clone()),
            Span::styled("█", Style::new().fg(Color::Cyan)),
        ]));
    } else {
        for (index, option) in pending.question.options.iter().enumerate() {
            let selected = index == pending.selected;
            let marker = if pending.question.multi_select {
                if pending.checked.contains(&index) {
                    "[x] "
                } else {
                    "[ ] "
                }
            } else {
                ""
            };
            let pointer = if selected { "> " } else { "  " };
            let style = if selected {
                Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD)
            } else {
                Style::new()
            };
            let mut spans = vec![Span::styled(
                format!("{pointer}{marker}{}", option.label),
                style,
            )];
            if !option.description.is_empty() {
                spans.push(Span::styled(
                    format!(" — {}", option.description),
                    Style::new().add_modifier(Modifier::DIM),
                ));
            }
            lines.push(Line::from(spans));
        }
    }
    let mut hints = Vec::new();
    if pending.free_text.is_some() {
        hints.push("enter: send");
        if !pending.question.options.is_empty() {
            hints.push("esc: back to options");
        }
    } else {
        hints.push("up/down: select");
        if pending.question.multi_select {
            hints.push("space: toggle");
        }
        hints.push("enter: answer");
        if pending.question.allow_free_text {
            hints.push("type: free text");
        }
    }
    let title = format!("question from {}", pending.agent);
    draw_modal(frame, &title, lines, &hints.join(" · "));
}

fn draw_popup(frame: &mut Frame, popup: &Popup) {
    match popup {
        Popup::Access(report) => {
            let mut lines = vec![
                Line::raw(format!("sandbox: {}", report.sandbox_kind)),
                Line::raw(format!("workspace mount: {}", report.workspace_mount)),
                Line::raw(format!("scratch: {}", report.scratch_dir)),
            ];
            let section = |lines: &mut Vec<Line<'static>>, title: &str, items: &[String]| {
                lines.push(Line::raw(""));
                lines.push(Line::styled(
                    title.to_string(),
                    Style::new().add_modifier(Modifier::BOLD),
                ));
                if items.is_empty() {
                    lines.push(Line::styled(
                        "  (none)".to_string(),
                        Style::new().add_modifier(Modifier::DIM),
                    ));
                }
                for item in items {
                    lines.push(Line::raw(format!("  {item}")));
                }
            };
            section(&mut lines, "readable paths", &report.readable_paths);
            section(&mut lines, "allowed domains", &report.allowed_domains);
            section(&mut lines, "credential domains", &report.credential_domains);
            if !report.notes.is_empty() {
                section(&mut lines, "notes", &report.notes);
            }
            draw_modal(frame, "sandbox access", lines, "any key to close");
        }
        Popup::Cost(entries) => {
            let mut lines = Vec::new();
            if entries.is_empty() {
                lines.push(Line::styled(
                    "no cost reports yet".to_string(),
                    Style::new().add_modifier(Modifier::DIM),
                ));
            }
            for entry in entries {
                lines.push(Line::raw(format!(
                    "{}: ${:.4} · {} in / {} out tok",
                    entry.backend,
                    entry.usage.usd,
                    crate::app::format_tokens(entry.usage.input_tokens),
                    crate::app::format_tokens(entry.usage.output_tokens),
                )));
                let mut limits = Vec::new();
                if let Some(max) = entry.quota.max_usd {
                    limits.push(format!("max ${max:.2}"));
                }
                if let Some(max) = entry.quota.max_total_tokens {
                    limits.push(format!("max {} tok", crate::app::format_tokens(max)));
                }
                if !limits.is_empty() {
                    lines.push(Line::styled(
                        format!("  quota: {}", limits.join(", ")),
                        Style::new().add_modifier(Modifier::DIM),
                    ));
                }
            }
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                format!(
                    "total: {}",
                    crate::app::format_cost_summary(entries.iter().map(|e| &e.usage))
                ),
                Style::new().add_modifier(Modifier::BOLD),
            ));
            draw_modal(frame, "cost", lines, "any key to close");
        }
        Popup::Pairing {
            code,
            expires_in_secs,
            addr,
            fingerprint,
        } => {
            let lines = vec![
                Line::from(vec![
                    Span::raw("pairing code: "),
                    Span::styled(
                        code.clone(),
                        Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("  (expires in {expires_in_secs}s)"),
                        Style::new().add_modifier(Modifier::DIM),
                    ),
                ]),
                Line::raw(""),
                Line::raw(format!("address: {addr}")),
                Line::raw(format!("fingerprint: {fingerprint}")),
                Line::raw(""),
                Line::raw("on the other device:".to_string()),
                Line::styled(
                    format!("  silo-tui --url {addr} --fingerprint {fingerprint} --pair {code}"),
                    Style::new().fg(Color::Cyan),
                ),
            ];
            draw_modal(frame, "pair another device", lines, "any key to close");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use silo_core::clock::Timestamp;
    use silo_core::event::{Event, EventPayload, QuestionOption, UserQuestion};
    use silo_core::sandbox::AccessReport;

    fn render(app: &App) -> String {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        text
    }

    fn event(seq: u64, payload: EventPayload) -> Event {
        Event {
            seq,
            time: Timestamp {
                logical: seq,
                wall_ms: None,
            },
            payload,
        }
    }

    #[test]
    fn wrap_text_wraps_at_word_boundaries() {
        assert_eq!(wrap_text("a b c", 3), vec!["a b", "c"]);
        assert_eq!(wrap_text("hello world", 11), vec!["hello world"]);
        assert_eq!(wrap_text("hello world", 5), vec!["hello", "world"]);
        assert_eq!(wrap_text("", 5), vec![""]);
    }

    #[test]
    fn wrap_text_hard_breaks_long_words() {
        assert_eq!(wrap_text("abcdefgh", 3), vec!["abc", "def", "gh"]);
        assert_eq!(wrap_text("x abcdefgh", 4), vec!["x ab", "cdef", "gh"]);
    }

    #[test]
    fn renders_status_bar_and_transcript() {
        let mut app = App::new("h42".into(), "127.0.0.1:1".into(), "00".repeat(32));
        app.handle_net(crate::net::NetEvent::Connected {
            harness_id: "h42".into(),
        });
        app.apply_event(event(
            0,
            EventPayload::UserPrompt {
                client_id: Some("client-1".into()),
                text: "build it".into(),
            },
        ));
        app.apply_event(event(
            1,
            EventPayload::AssistantText {
                agent: "agent-0".into(),
                text: "on it".into(),
            },
        ));
        let text = render(&app);
        assert!(text.contains("h42"));
        assert!(text.contains("connected"));
        assert!(text.contains("client-1 > build it"));
        assert!(text.contains("on it"));
        assert!(text.contains("$0.0000 | 0 tok"));
    }

    #[test]
    fn renders_question_modal_with_options() {
        let mut app = App::new("h".into(), "a:1".into(), "00".repeat(32));
        app.apply_event(event(
            0,
            EventPayload::QuestionAsked {
                id: "q1".into(),
                agent: "agent-0".into(),
                question: UserQuestion {
                    question: "Deploy now?".into(),
                    options: vec![
                        QuestionOption {
                            label: "yes".into(),
                            description: "ship it".into(),
                        },
                        QuestionOption {
                            label: "no".into(),
                            description: String::new(),
                        },
                    ],
                    multi_select: false,
                    allow_free_text: true,
                },
            },
        ));
        let text = render(&app);
        assert!(text.contains("question from agent-0"));
        assert!(text.contains("Deploy now?"));
        assert!(text.contains("> yes"));
        assert!(text.contains("ship it"));
        assert!(text.contains("type: free text"));
    }

    #[test]
    fn renders_access_popup() {
        let mut app = App::new("h".into(), "a:1".into(), "00".repeat(32));
        app.handle_server(silo_core::protocol::ServerMessage::AccessReport {
            report: AccessReport {
                sandbox_kind: "mock".into(),
                workspace_mount: "/workspace".into(),
                scratch_dir: "/scratch".into(),
                readable_paths: vec!["/usr/bin".into()],
                allowed_domains: vec!["crates.io".into()],
                credential_domains: vec!["api.github.com".into()],
                notes: vec!["test note".into()],
            },
        });
        let text = render(&app);
        assert!(text.contains("sandbox access"));
        assert!(text.contains("/usr/bin"));
        assert!(text.contains("crates.io"));
        assert!(text.contains("api.github.com"));
        assert!(text.contains("test note"));
    }

    #[test]
    fn renders_pairing_popup_with_connect_hint() {
        let mut app = App::new("h".into(), "host.example:9000".into(), "ab".repeat(32));
        app.handle_server(silo_core::protocol::ServerMessage::PairingCode {
            code: "ZZZZ9999".into(),
            expires_in_secs: 120,
        });
        let text = render(&app);
        assert!(text.contains("ZZZZ9999"));
        assert!(text.contains("host.example:9000"));
    }

    #[test]
    fn scrolled_transcript_shows_older_lines() {
        let mut app = App::new("h".into(), "a:1".into(), "00".repeat(32));
        for i in 0..100 {
            app.apply_event(event(
                i,
                EventPayload::AssistantText {
                    agent: "agent-0".into(),
                    text: format!("message number {i}"),
                },
            ));
        }
        let bottom = render(&app);
        assert!(bottom.contains("message number 99"));
        assert!(!bottom.contains("message number 10 "));
        app.scroll_from_bottom = 80;
        let scrolled = render(&app);
        assert!(!scrolled.contains("message number 99"));
        assert!(scrolled.contains("message number 10"));
    }
}
