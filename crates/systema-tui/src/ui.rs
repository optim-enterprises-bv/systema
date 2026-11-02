//! Rendering. One dashboard: header, a three-column body (agents / focus / node+swarm),
//! a tail-f event log, and a hotbar — the velo cockpit, mapped onto Systema.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};

use crate::app::{App, SessionState};
use crate::theme::{self, panel};

pub fn draw(f: &mut Frame, app: &App) {
    let rows = Layout::vertical([
        Constraint::Length(1), // header
        Constraint::Min(6),    // body
        Constraint::Length(1), // hotbar / input
    ])
    .split(f.area());

    header(f, rows[0], app);

    let body = Layout::vertical([Constraint::Percentage(58), Constraint::Min(5)]).split(rows[1]);
    let top = Layout::horizontal([Constraint::Percentage(26), Constraint::Min(20), Constraint::Length(34)]).split(body[0]);
    agents_panel(f, top[0], app);
    focus_panel(f, top[1], app);
    let right = Layout::vertical([Constraint::Length(10), Constraint::Min(4)]).split(top[2]);
    node_panel(f, right[0], app);
    swarm_panel(f, right[1], app);
    events_panel(f, body[1], app);

    if app.input.is_some() {
        input_line(f, rows[2], app);
    } else {
        hotbar(f, rows[2], app);
    }
}

fn hms(ms: u64) -> String {
    let s = ms / 1000;
    format!("{:02}:{:02}:{:02}", (s / 3600) % 24, (s / 60) % 60, s % 60)
}

fn header(f: &mut Frame, area: Rect, app: &App) {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
    let dot = if app.connected { Span::styled(" ● ", theme::accent(theme::GREEN)) } else { Span::styled(" ○ ", theme::accent(theme::RED)) };
    let running = app.sessions.values().filter(|s| matches!(s.state, SessionState::Thinking | SessionState::Tool | SessionState::Approval)).count();
    let peer = if app.status.peer_id.len() > 12 { &app.status.peer_id[..12] } else { &app.status.peer_id };
    let left = Line::from(vec![
        Span::styled(" SYSTEMA ", Style::default().fg(theme::BG).bg(theme::BLUE).add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(peer.to_string(), theme::dim()),
        dot,
        Span::styled(if app.connected { "node up" } else { "connecting…" }, theme::fg()),
        Span::styled(format!("   running {running}"), theme::dim()),
    ]);
    f.render_widget(Paragraph::new(left), area);
    let right = Line::from(vec![Span::styled(format!("{}  tokyo night · systema ", hms(now)), theme::dim())]).alignment(Alignment::Right);
    f.render_widget(Paragraph::new(right), area);
}

fn agents_panel(f: &mut Frame, area: Rect, app: &App) {
    let sub = Line::from(Span::styled(format!("{} ", app.agents.len()), theme::dim()));
    let block = panel("AGENTS", sub, theme::BLUE);
    let items: Vec<ListItem> = app
        .agents
        .iter()
        .enumerate()
        .map(|(i, a)| {
            let st = app.session_state(&a.name);
            let marker = if i == app.selected { "▸ " } else { "  " };
            let badge = if a.public { "◆" } else { "·" };
            let line = Line::from(vec![
                Span::styled(marker, theme::accent(theme::BLUE)),
                Span::styled(format!("{:02} ", i + 1), theme::dim()),
                Span::styled(format!("{:<10}", a.name), if i == app.selected { theme::bold(theme::FG) } else { theme::fg() }),
                Span::styled(format!("{badge} "), theme::dim()),
                Span::styled(st.label(), theme::accent(st.color())),
            ]);
            ListItem::new(line)
        })
        .collect();
    let mut state = ListState::default();
    state.select(Some(app.selected));
    f.render_stateful_widget(List::new(items).block(block), area, &mut state);
}

fn trust_spans(app: &App, agent: &str) -> Vec<Span<'static>> {
    let t = app.sessions.get(agent).map(|s| s.trust).unwrap_or_default();
    let leg = |on: bool, ch: &'static str| {
        if on { Span::styled(ch, theme::accent(theme::RED)) } else { Span::styled(ch, theme::dim()) }
    };
    vec![
        Span::styled("trust ", theme::dim()),
        leg(t.untrusted, "U"),
        Span::raw(" "),
        leg(t.private, "P"),
        Span::raw(" "),
        leg(t.outbound, "O"),
    ]
}

fn focus_panel(f: &mut Frame, area: Rect, app: &App) {
    // If an approval is pending, this panel becomes the approval card.
    if let Some(a) = app.next_approval() {
        let block = panel("APPROVAL NEEDED", Line::from(Span::styled("y / n / A ", theme::dim())), theme::RED);
        let choices = if a.rememberable { "[y] allow once   [A] always   [n] deny" } else { "[y] allow once   [n] deny" };
        let text = vec![
            Line::from(vec![Span::styled("agent  ", theme::dim()), Span::styled(a.agent.clone(), theme::bold(theme::FG))]),
            Line::from(vec![Span::styled("wants  ", theme::dim()), Span::styled(a.request.clone(), theme::accent(theme::YELLOW))]),
            Line::from(""),
            Line::from(Span::styled(a.reason.clone(), theme::fg())),
            Line::from(""),
            Line::from(Span::styled(choices, theme::bold(theme::RED))),
        ];
        f.render_widget(Paragraph::new(text).wrap(Wrap { trim: true }).block(block), area);
        return;
    }

    let Some(agent) = app.selected_agent() else {
        f.render_widget(panel("FOCUS", "", theme::BLUE), area);
        return;
    };
    let st = app.session_state(&agent.name);
    let sv = app.sessions.get(&agent.name);
    let sub = Line::from(vec![trust_spans(app, &agent.name), vec![Span::raw(" ")]].concat());
    let block = panel("FOCUS", sub, st.color());
    let mut lines = vec![
        Line::from(vec![
            Span::styled(agent.name.clone(), theme::bold(theme::FG)),
            Span::raw("   "),
            Span::styled(st.label().to_uppercase(), theme::bold(st.color())),
            Span::styled(format!("   turns {}", sv.map(|s| s.turns).unwrap_or(0)), theme::dim()),
        ]),
        Line::from(Span::styled(agent.description.clone(), theme::dim())),
    ];
    if !agent.skills.is_empty() {
        lines.push(Line::from(Span::styled(format!("skills: {}", agent.skills.join(", ")), theme::dim())));
    }
    lines.push(Line::from(""));
    if let Some(sv) = sv {
        if !sv.last_text.is_empty() {
            lines.push(Line::from(Span::styled("last output", theme::dim())));
            for l in textwrap(&sv.last_text, area.width.saturating_sub(4) as usize, 8) {
                lines.push(Line::from(Span::styled(l, theme::fg())));
            }
        }
    } else {
        lines.push(Line::from(Span::styled("idle — press 'a' to ask this agent", theme::dim())));
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }).block(block), area);
}

fn node_panel(f: &mut Frame, area: Rect, app: &App) {
    let block = panel("NODE", Line::from(Span::styled("vitals ", theme::dim())), theme::PURPLE);
    let s = &app.status;
    let kv = |k: &'static str, v: String| {
        Line::from(vec![Span::styled(format!("{k:<12}"), theme::dim()), Span::styled(v, theme::fg())])
    };
    let lines = vec![
        kv("agents", app.agents.len().to_string()),
        kv("running", app.sessions.values().filter(|x| matches!(x.state, SessionState::Thinking | SessionState::Tool | SessionState::Approval)).count().to_string()),
        kv("approvals", app.approvals.len().to_string()),
        kv("grants", s.grants.to_string()),
        kv("inbox", s.inbox.to_string()),
        kv("blocked", s.blocked.to_string()),
    ];
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn swarm_panel(f: &mut Frame, area: Rect, app: &App) {
    let block = panel("SWARM", Line::from(Span::styled("p2p ", theme::dim())), theme::CYAN);
    let s = &app.status;
    let relay = if s.relay_server {
        format!("serving ({})", s.relay_reservations)
    } else if s.relay_reservations > 0 {
        format!("reserved x{}", s.relay_reservations)
    } else {
        "—".into()
    };
    let kv = |k: &'static str, v: String| {
        Line::from(vec![Span::styled(format!("{k:<12}"), theme::dim()), Span::styled(v, theme::fg())])
    };
    let mut lines = vec![kv("peers", s.peers.to_string()), kv("relay", relay), kv("listen", s.listen.len().to_string())];
    if let Some(addr) = s.listen.iter().find(|a| !a.contains("127.0.0.1")) {
        lines.push(Line::from(Span::styled(crate::app::truncate(addr, 28), theme::dim())));
    }
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn events_panel(f: &mut Frame, area: Rect, app: &App) {
    let sub = Line::from(Span::styled("tail -f ", theme::dim()));
    let block = panel("EVENTS", sub, theme::GREEN);
    let inner_h = area.height.saturating_sub(2) as usize;
    // Newest at the bottom: take the newest `inner_h`, then reverse so they read top→bottom.
    let mut rows: Vec<Line> = app
        .events
        .iter()
        .take(inner_h)
        .map(|e| {
            Line::from(vec![
                Span::styled(format!("{} ", hms(e.ts)), theme::dim()),
                Span::styled(format!("{:>8} ", e.tag), theme::accent(e.tag_color).add_modifier(Modifier::BOLD)),
                Span::styled(format!("{:<12} ", crate::app::truncate(&e.who, 12)), theme::dim()),
                Span::styled(e.text.clone(), theme::fg()),
            ])
        })
        .collect();
    rows.reverse();
    f.render_widget(Paragraph::new(rows).block(block), area);
}

fn hotbar(f: &mut Frame, area: Rect, app: &App) {
    let approving = app.next_approval().is_some();
    let mut spans = Vec::new();
    if approving {
        spans.extend(theme::keycap("y", "allow", true));
        spans.extend(theme::keycap("n", "deny", true));
        spans.extend(theme::keycap("A", "always", true));
    }
    spans.extend(theme::keycap("a", "ask", !approving));
    spans.extend(theme::keycap("s", "steer", !approving));
    spans.extend(theme::keycap("c", "cancel", !approving));
    spans.extend(theme::keycap("j/k", "move", !approving));
    spans.extend(theme::keycap("q", "quit", true));
    if let Some((msg, _)) = &app.toast {
        f.render_widget(Paragraph::new(Line::from(Span::styled(format!("{msg} "), theme::accent(theme::YELLOW))).alignment(Alignment::Right)), area);
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn input_line(f: &mut Frame, area: Rect, app: &App) {
    let Some(input) = &app.input else { return };
    let label = match input.kind {
        crate::app::InputKind::Ask => "ask",
        crate::app::InputKind::Steer => "steer",
    };
    let line = Line::from(vec![
        Span::styled(format!(" {label} "), Style::default().fg(theme::BG).bg(theme::PURPLE).add_modifier(Modifier::BOLD)),
        Span::styled(format!(" {} ", input.target), theme::accent(theme::PURPLE)),
        Span::styled("› ", theme::dim()),
        Span::styled(input.buffer.clone(), theme::fg()),
        Span::styled("█", theme::accent(theme::PURPLE)),
        Span::styled("   esc cancel · enter send", theme::dim()),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

/// Wrap `text` to `width`, at most `max` lines.
fn textwrap(text: &str, width: usize, max: usize) -> Vec<String> {
    let width = width.max(8);
    let mut out = Vec::new();
    for raw in text.split('\n') {
        let mut line = String::new();
        for word in raw.split_whitespace() {
            if line.chars().count() + word.chars().count() + 1 > width && !line.is_empty() {
                out.push(std::mem::take(&mut line));
                if out.len() >= max {
                    out.push("…".into());
                    return out;
                }
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
        if !line.is_empty() {
            out.push(line);
        }
        if out.len() >= max {
            break;
        }
    }
    out
}
