//! `bbs view`: a read-only, auto-following feed of every message on the board.
//! It never archives anything, so watching does not consume anyone's mail.

use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, Local};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};

use crate::backend::{Backend, FeedWatch};
use crate::proto::{Kind, Message};

const BACKLOG: u32 = 200;
const KEY_POLL_INTERVAL: Duration = Duration::from_millis(100);

pub async fn run(backend: &mut Backend) -> Result<()> {
    let mut feed = backend.watch_feed(BACKLOG).await?;
    // Fetch before taking over the terminal, so a refusal prints normally.
    let rows = feed.next().await?;
    let mut terminal = ratatui::init();
    let result = feed_loop(&mut terminal, &mut feed, rows).await;
    ratatui::restore();
    result
}

async fn feed_loop(
    terminal: &mut DefaultTerminal,
    feed: &mut FeedWatch<'_>,
    mut rows: Vec<Message>,
) -> Result<()> {
    let mut dirty = true;
    loop {
        if dirty {
            terminal.draw(|frame| render(frame, &rows))?;
            dirty = false;
        }
        // Wait for the next snapshot, checking keys at least every KEY_POLL_INTERVAL.
        tokio::select! {
            snapshot = feed.next() => {
                rows = snapshot?;
                dirty = true;
            }
            _ = tokio::time::sleep(KEY_POLL_INTERVAL) => {}
        }
        while event::poll(Duration::ZERO)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    let ctrl_c = key.modifiers.contains(KeyModifiers::CONTROL)
                        && key.code == KeyCode::Char('c');
                    if ctrl_c || matches!(key.code, KeyCode::Char('q') | KeyCode::Esc) {
                        return Ok(());
                    }
                }
                Event::Resize(..) => dirty = true,
                _ => {}
            }
        }
    }
}

fn render(frame: &mut Frame, rows: &[Message]) {
    let [title_area, feed_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(frame.area());

    let title = Line::from(vec![
        " bbs view ".bold().reversed(),
        format!("  {} messages  ·  q/Esc to quit", rows.len()).dark_gray(),
    ]);
    frame.render_widget(title, title_area);

    if rows.is_empty() {
        frame.render_widget(
            Paragraph::new("No messages yet. Waiting…".dark_gray()),
            feed_area,
        );
        return;
    }

    let mut text = Text::default();
    for row in rows {
        text.lines.push(Line::default());
        text.lines.push(header(row));
        for mut line in tui_markdown::from_str(&row.body).lines {
            line.spans.insert(0, Span::raw("  "));
            text.lines.push(line);
        }
    }

    // Follow the tail: scroll so the newest message sits at the bottom.
    let feed = Paragraph::new(text).wrap(Wrap { trim: false });
    let total = feed.line_count(feed_area.width);
    let scroll = total.saturating_sub(feed_area.height as usize);
    let feed = feed.scroll((scroll.min(u16::MAX as usize) as u16, 0));
    frame.render_widget(feed, feed_area);
}

fn header(row: &Message) -> Line<'static> {
    let sent = local(&row.created_at);
    let mut spans = vec![
        Span::styled(
            sent.map_or_else(
                || row.created_at.clone(),
                |t| t.format("%Y-%m-%d %H:%M:%S").to_string(),
            ),
            Style::new().fg(Color::DarkGray),
        ),
        Span::raw("  "),
        name_span(&row.sender_display_name(), row.from_kind()),
        Span::styled(" → ", Style::new().fg(Color::DarkGray)),
    ];
    match (&row.to_group, &row.to_name) {
        (Some(group), _) => spans.push(Span::styled(
            format!("@{group}"),
            Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        )),
        (None, Some(to)) => {
            spans.push(Span::styled(
                to.clone(),
                Style::new().add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::raw("  "));
            spans.push(match row.read_at.as_deref().and_then(local) {
                Some(read) => {
                    let same_day = sent.is_some_and(|s| s.date_naive() == read.date_naive());
                    let fmt = if same_day {
                        "%H:%M:%S"
                    } else {
                        "%Y-%m-%d %H:%M:%S"
                    };
                    Span::styled(
                        format!("read {}", read.format(fmt)),
                        Style::new().fg(Color::Green),
                    )
                }
                None => Span::styled("unread", Style::new().fg(Color::Red)),
            });
        }
        (None, None) => spans.push(Span::raw("?")),
    }
    Line::from(spans)
}

fn name_span(name: &str, kind: Kind) -> Span<'static> {
    let color = match kind {
        Kind::Human => Color::Magenta,
        Kind::Agent | Kind::Unspecified => Color::Cyan,
    };
    Span::styled(
        name.to_string(),
        Style::new().fg(color).add_modifier(Modifier::BOLD),
    )
}

fn local(stored: &str) -> Option<DateTime<Local>> {
    DateTime::parse_from_rfc3339(stored)
        .ok()
        .map(|t| t.with_timezone(&Local))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn row(group: Option<&str>, to: Option<&str>, read_at: Option<&str>, body: &str) -> Message {
        Message {
            id: 0,
            created_at: "2026-10-09T14:02:11.000000Z".into(),
            from_kind: Kind::Agent.into(),
            from_name: "Juliana".into(),
            from_owner: None,
            reader_owner: None,
            to_group: group.map(Into::into),
            to_name: to.map(Into::into),
            read_at: read_at.map(Into::into),
            body: body.into(),
        }
    }

    fn screen(rows: &[Message], height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(70, height)).unwrap();
        terminal.draw(|f| render(f, rows)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn renders_headers_and_markdown() {
        let rows = [
            row(Some("acme"), None, None, "standup in **5 minutes**"),
            row(
                None,
                Some("Mateo"),
                Some("2026-10-09T14:03:00.000000Z"),
                "- one\n- two",
            ),
            row(None, Some("Aiko"), None, "ping"),
        ];
        let out = screen(&rows, 20);
        assert!(out.contains("3 messages"), "{out}");
        assert!(out.contains("Juliana → @acme"), "{out}");
        assert!(
            out.contains("standup in 5 minutes") && !out.contains("**"),
            "{out}"
        );
        assert!(out.contains("Juliana → Mateo  read "), "{out}");
        assert!(out.contains("Juliana → Aiko  unread"), "{out}");
        // Broadcasts never show a read time.
        let acme = out.lines().find(|l| l.contains("@acme")).unwrap();
        assert!(!acme.contains("read"), "{acme}");
    }

    #[test]
    fn follows_the_tail_when_overflowing() {
        let rows: Vec<_> = (0..30)
            .map(|i| row(Some("acme"), None, None, &format!("message number {i}")))
            .collect();
        let out = screen(&rows, 10);
        assert!(out.contains("message number 29"), "{out}");
        assert!(!out.contains("message number 0\n"), "{out}");
    }
}
