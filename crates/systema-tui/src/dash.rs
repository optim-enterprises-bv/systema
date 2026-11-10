//! The Dash tab: the engine at a glance. Fleet health, the busiest workers, live sessions, a
//! load sparkline and a log of what changed between polls. Everything comes from the data the
//! Engine tab already polls (`engine::workers::list`, `session::list`); nothing extra is called.

use std::collections::BTreeMap;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Color;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Sparkline};

use crate::app::{App, ChangeKind};
use crate::theme::{self, panel};

/// A horizontal bar `width` cells wide, filled to `frac` (0..=1).
fn bar(frac: f64, width: usize) -> (String, String) {
    let filled = ((frac.clamp(0.0, 1.0) * width as f64).round() as usize).min(width);
    ("█".repeat(filled), "░".repeat(width - filled))
}

fn status_color(status: &str) -> Color {
    match status {
        "connected" => theme::GREEN,
        "available" => theme::CYAN,
        "disconnected" => theme::RED,
        _ => theme::YELLOW,
    }
}

pub fn draw(f: &mut Frame, area: Rect, app: &App) {
    let rows = Layout::vertical([Constraint::Length(11), Constraint::Length(7), Constraint::Min(4)]).split(area);
    let top = Layout::horizontal([Constraint::Length(36), Constraint::Min(30), Constraint::Length(46)]).split(rows[0]);
    fleet(f, top[0], app);
    busy(f, top[1], app);
    sessions(f, top[2], app);
    load(f, rows[1], app);
    changes(f, rows[2], app);
}

fn fleet(f: &mut Frame, area: Rect, app: &App) {
    let e = &app.engine;
    let total = e.workers.len();
    let mut by: BTreeMap<&str, usize> = BTreeMap::new();
    for w in &e.workers {
        *by.entry(w.status.as_str()).or_default() += 1;
    }
    let functions: u64 = e.workers.iter().map(|w| w.functions).sum();
    let active: u64 = e.workers.iter().map(|w| w.active).sum();
    let mut lines = vec![Line::from(vec![
        Span::styled(format!("{total}"), theme::bold(theme::CYAN)),
        Span::styled(" workers  ", theme::dim()),
        Span::styled(if e.online { "engine up" } else { "engine offline" }, theme::accent(if e.online { theme::GREEN } else { theme::RED })),
    ])];
    for (status, n) in &by {
        let (on, off) = bar(*n as f64 / total.max(1) as f64, 14);
        lines.push(Line::from(vec![
            Span::styled(format!("{status:<12}"), theme::fg()),
            Span::styled(on, theme::accent(status_color(status))),
            Span::styled(off, theme::dim()),
            Span::styled(format!(" {n}"), theme::bold(status_color(status))),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![Span::styled("functions   ", theme::dim()), Span::styled(format!("{functions}"), theme::fg())]));
    lines.push(Line::from(vec![
        Span::styled("in flight   ", theme::dim()),
        Span::styled(format!("{active}"), theme::accent(if active > 0 { theme::YELLOW } else { theme::DIM })),
    ]));
    f.render_widget(Paragraph::new(lines).block(panel("FLEET", Line::from(""), theme::CYAN)), area);
}

fn busy(f: &mut Frame, area: Rect, app: &App) {
    let e = &app.engine;
    let mut ws: Vec<&crate::engine::WorkerRow> = e.workers.iter().collect();
    ws.sort_by(|a, b| b.active.cmp(&a.active).then(b.functions.cmp(&a.functions)));
    let max = ws.iter().map(|w| w.functions).max().unwrap_or(1).max(1);
    let name_w = ws.iter().map(|w| w.name.chars().count()).max().unwrap_or(8).min(22);
    let room = area.height.saturating_sub(2) as usize;
    let lines: Vec<Line> = ws
        .iter()
        .take(room)
        .map(|w| {
            let (on, off) = bar(w.functions as f64 / max as f64, 16);
            Line::from(vec![
                Span::styled(format!("{:<name_w$} ", crate::app::truncate(&w.name, name_w)), theme::fg()),
                Span::styled(on, theme::accent(status_color(&w.status))),
                Span::styled(off, theme::dim()),
                Span::styled(format!(" {:>4} fn", w.functions), theme::dim()),
                Span::styled(
                    if w.active > 0 { format!("  ▶ {}", w.active) } else { String::new() },
                    theme::accent(theme::YELLOW),
                ),
            ])
        })
        .collect();
    f.render_widget(Paragraph::new(lines).block(panel("WORKERS", Line::from(Span::styled("by activity, then surface ", theme::dim())), theme::PURPLE)), area);
}

fn sessions(f: &mut Frame, area: Rect, app: &App) {
    let e = &app.engine;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
    let live = e.sessions.iter().filter(|s| now.saturating_sub(s.updated) < 60_000).count();
    let room = area.height.saturating_sub(2) as usize;
    let lines: Vec<Line> = e
        .sessions
        .iter()
        .take(room)
        .map(|s| {
            let fresh = now.saturating_sub(s.updated) < 60_000;
            Line::from(vec![
                Span::styled(if fresh { "● " } else { "· " }, theme::accent(if fresh { theme::GREEN } else { theme::DIM })),
                Span::styled(format!("{:>3} ", s.messages), theme::dim()),
                Span::styled(crate::app::truncate(&s.title, 28), theme::fg()),
                Span::styled(format!(" {}", crate::ui::ago(s.updated)), theme::dim()),
            ])
        })
        .collect();
    let sub = Line::from(Span::styled(format!("{} total · {live} live ", e.sessions.len()), theme::dim()));
    f.render_widget(Paragraph::new(lines).block(panel("SESSIONS", sub, theme::PINK)), area);
}

fn changes(f: &mut Frame, area: Rect, app: &App) {
    let e = &app.engine;
    let room = area.height.saturating_sub(2) as usize;
    let lines: Vec<Line> = if e.changes.is_empty() {
        vec![Line::from(Span::styled("nothing has changed since the cockpit started", theme::dim()))]
    } else {
        e.changes
            .iter()
            .rev()
            .take(room)
            .map(|c| {
                let (mark, color) = match c.kind {
                    ChangeKind::Up => ("▲", theme::GREEN),
                    ChangeKind::Down => ("▼", theme::RED),
                    ChangeKind::Info => ("●", theme::CYAN),
                };
                Line::from(vec![
                    Span::styled(format!("{:>5}s ", c.at.elapsed().as_secs()), theme::dim()),
                    Span::styled(format!("{mark} "), theme::accent(color)),
                    Span::styled(c.text.clone(), theme::fg()),
                ])
            })
            .collect()
    };
    f.render_widget(Paragraph::new(lines).block(panel("LOG", Line::from(Span::styled("what changed, newest first ", theme::dim())), theme::YELLOW)), area);
}

fn load(f: &mut Frame, area: Rect, app: &App) {
    let e = &app.engine;
    let width = area.width.saturating_sub(2) as usize;
    let data: Vec<u64> = e.load.iter().rev().take(width).rev().copied().collect();
    let now = e.load.back().copied().unwrap_or(0);
    let peak = e.load.iter().copied().max().unwrap_or(0);
    let sub = Line::from(Span::styled(format!("now {now} · peak {peak} "), theme::dim()));
    let spark = Sparkline::default().block(panel("IN FLIGHT", sub, theme::GREEN)).data(&data).style(theme::accent(theme::GREEN));
    f.render_widget(spark, area);
}
