//! Screen layout: header, NEEDS YOU panel, table, footer

use crate::app::App;
use crate::fmt;
use crate::model::{Session, Status};
use crate::providers::clip;
use chrono::{DateTime, Utc};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState};

/// Footer flash messages stay this long
const FLASH_SECS: i64 = 10;
/// Below this width the LAST TOOL column is dropped
const WIDE: u16 = 100;

fn status_style(s: &Status) -> Style {
    match s {
        Status::NeedsYou { .. } => Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        Status::RateLimited { .. } => Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD),
        Status::Working { .. } if s.is_quiet() => Style::new().fg(Color::Yellow),
        Status::Working { .. } => Style::new().fg(Color::Green),
        Status::Idle => Style::new(),
        Status::Dead { .. } => Style::new().fg(Color::DarkGray),
    }
}

fn dim() -> Style {
    Style::new().fg(Color::DarkGray)
}

fn bold() -> Style {
    Style::new().add_modifier(Modifier::BOLD)
}

fn needs_attention(s: &Session) -> bool {
    matches!(s.status, Status::NeedsYou { .. } | Status::RateLimited { .. })
}

pub fn render(f: &mut Frame, app: &App, now: DateTime<Utc>) {
    let sessions = &app.snapshot.sessions;
    let stuck: Vec<&Session> = sessions.iter().filter(|s| needs_attention(s)).take(4).collect();
    let panel = if stuck.is_empty() { 0 } else { stuck.len() as u16 + 2 };
    let [head, panel_area, main, foot] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(panel),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .areas(f.area());

    render_header(f, head, app);
    if panel > 0 {
        render_needs_you(f, panel_area, &stuck, now);
    }
    if sessions.is_empty() {
        let msg = "No live agent sessions. Start Claude Code in another terminal.";
        f.render_widget(
            Paragraph::new(vec![Line::raw(""), Line::styled(msg, dim())]).centered(),
            main,
        );
    } else {
        render_table(f, main, sessions, app.selected, now);
    }
    render_footer(f, foot, app, now);
}

fn render_header(f: &mut Frame, area: Rect, app: &App) {
    let sessions = &app.snapshot.sessions;
    let count = |pred: fn(&Status) -> bool| sessions.iter().filter(|s| pred(&s.status)).count();
    let live = count(|s| !matches!(s, Status::Dead { .. }));
    let needs = count(|s| matches!(s, Status::NeedsYou { .. }));
    let limited = count(|s| matches!(s, Status::RateLimited { .. }));
    let mut left = vec![
        Span::styled(" stalltop ", Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::raw(format!(" · {live} agents")),
    ];
    if needs > 0 {
        left.push(Span::styled(
            format!(" · {needs} needs you"),
            Style::new().fg(Color::Red),
        ));
    }
    if limited > 0 {
        left.push(Span::styled(
            format!(" · {limited} rate-limited"),
            Style::new().fg(Color::Magenta),
        ));
    }
    let t = &app.snapshot.today;
    let right = format!(
        "today {} · {} tok ",
        fmt::cost(Some(t.cost_usd)),
        fmt::tokens(t.tokens.total())
    );
    let [l, r] =
        Layout::horizontal([Constraint::Min(10), Constraint::Length(right.chars().count() as u16)]).areas(area);
    f.render_widget(Paragraph::new(Line::from(left)), l);
    f.render_widget(Paragraph::new(Line::from(right)).right_aligned(), r);
}

fn render_needs_you(f: &mut Frame, area: Rect, stuck: &[&Session], now: DateTime<Utc>) {
    let lines: Vec<Line> = stuck
        .iter()
        .map(|s| {
            let icon = if matches!(s.status, Status::NeedsYou { .. }) {
                "●"
            } else {
                "◷"
            };
            Line::from(vec![
                Span::styled(format!("{icon} "), status_style(&s.status)),
                Span::styled(format!("{:<18} ", clip(&s.project, 18)), bold()),
                Span::styled(fmt::status_detail(&s.status, now), status_style(&s.status)),
            ])
        })
        .collect();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(Color::Red))
        .title(Span::styled(" NEEDS YOU ", status_style(&stuck[0].status)));
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_table(f: &mut Frame, area: Rect, sessions: &[Session], selected: usize, now: DateTime<Utc>) {
    let wide = area.width >= WIDE;
    let mut head = vec!["AGENT", "PROJECT", "STATUS", "MODEL"];
    if wide {
        head.push("LAST TOOL");
    }
    head.extend(["TOKENS", "COST", "AGE"]);

    let rows = sessions.iter().map(|s| {
        let mut cells = vec![
            Cell::from(s.agent.label()),
            Cell::from(s.project.clone()),
            Cell::from(Span::styled(s.status.label(), status_style(&s.status))),
            Cell::from(fmt::model(s.model.as_deref())),
        ];
        if wide {
            cells.push(Cell::from(s.last_tool.clone().unwrap_or_else(|| "-".into())));
        }
        cells.push(Cell::from(Line::from(fmt::tokens(s.tokens.total())).right_aligned()));
        cells.push(Cell::from(Line::from(fmt::cost(s.cost_usd)).right_aligned()));
        cells.push(Cell::from(
            Line::from(fmt::age((now - s.started_at).num_seconds())).right_aligned(),
        ));
        let row = Row::new(cells);
        if matches!(s.status, Status::Dead { .. }) {
            row.style(dim())
        } else {
            row
        }
    });

    let mut widths = vec![
        Constraint::Length(7),
        Constraint::Min(12),
        Constraint::Length(10),
        Constraint::Length(14),
    ];

    if wide {
        widths.push(Constraint::Min(16));
    }
    widths.extend([Constraint::Length(7), Constraint::Length(8), Constraint::Length(6)]);

    let table = Table::new(rows, widths)
        .header(Row::new(head).style(bold()))
        .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED))
        .highlight_symbol("▶ ");
    let mut state = TableState::default().with_selected(Some(selected));
    f.render_stateful_widget(table, area, &mut state);
}

fn render_footer(f: &mut Frame, area: Rect, app: &App, now: DateTime<Utc>) {
    if let Some((msg, at)) = &app.flash
        && (now - *at).num_seconds() < FLASH_SECS
    {
        f.render_widget(
            Paragraph::new(Line::styled(format!(" {msg}"), Style::new().fg(Color::Red))),
            area,
        );
        return;
    }
    let info = format!("notify {}", if app.notify { "on" } else { "off" });
    let [l, r] = Layout::horizontal([Constraint::Min(10), Constraint::Length(info.chars().count() as u16)]).areas(area);
    f.render_widget(Paragraph::new(Line::styled(" q quit  ↑↓ select  n notify", dim())), l);
    f.render_widget(Paragraph::new(Line::styled(info, dim())).right_aligned(), r);
}

// TODO: tests
