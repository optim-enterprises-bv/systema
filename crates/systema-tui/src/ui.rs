//! Rendering. One dashboard: header, a three-column body (agents / focus / node+swarm),
//! a tail-f event log, and a hotbar — the velo cockpit, mapped onto Systema.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};

use crate::app::{App, Focus, SessionState, Tab};
use crate::theme::{self, panel};

pub fn draw(f: &mut Frame, app: &App) {
    let rows = Layout::vertical([
        Constraint::Length(1), // header
        Constraint::Min(6),    // body
        Constraint::Length(1), // hotbar / input
    ])
    .split(f.area());

    header(f, rows[0], app);

    if app.tab == Tab::Dash {
        crate::dash::draw(f, rows[1], app);
        hotbar(f, rows[2], app);
        return;
    }

    if app.tab == Tab::Engine {
        engine_view(f, rows[1], app);
        if app.input.is_some() {
            input_line(f, rows[2], app);
        } else {
            hotbar(f, rows[2], app);
        }
        return;
    }

    if !app.connected && app.agents.is_empty() {
        splash(f, rows[1], app);
        if app.input.is_some() {
            input_line(f, rows[2], app);
        } else {
            hotbar(f, rows[2], app);
        }
        return;
    }

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
    // The live dot breathes while connected; offline it stays a hollow red ring.
    let dot = if app.connected {
        let on = (app.frame / 4) % 2 == 0;
        Span::styled(if on { " ● " } else { " ◉ " }, theme::accent(theme::GREEN))
    } else {
        Span::styled(" ○ ", theme::accent(theme::RED))
    };
    let running = app.sessions.values().filter(|s| matches!(s.state, SessionState::Thinking | SessionState::Tool | SessionState::Approval)).count();
    let peer = if app.status.peer_id.len() > 12 { &app.status.peer_id[..12] } else { &app.status.peer_id };
    let mut left = vec![Span::styled("▌", theme::accent(theme::CYAN))];
    left.extend(theme::gradient_spans(" SYSTEMA ", app.frame));
    left.push(Span::styled("▐", theme::accent(theme::PINK)));
    left.push(Span::raw(" "));
    for (key, name, tab) in [("1", "SWARM", Tab::Swarm), ("2", "ENGINE", Tab::Engine), ("3", "DASH", Tab::Dash)] {
        let on = app.tab == tab;
        let style = if on { Style::default().fg(theme::BG).bg(theme::PINK).add_modifier(Modifier::BOLD) } else { theme::dim() };
        left.push(Span::styled(format!(" {key} {name} "), style));
        left.push(Span::raw(" "));
    }
    if app.tab != Tab::Swarm {
        let e = &app.engine;
        let on = e.online;
        left.push(Span::styled(if on { " ● " } else { " ○ " }, theme::accent(if on { theme::GREEN } else { theme::RED })));
        left.push(Span::styled(if on { "engine up" } else { "engine offline" }, theme::fg()));
        left.push(Span::styled(format!("   {} · {} workers · {} sessions", e.url, e.workers.len(), e.sessions.len()), theme::dim()));
    } else {
        left.push(Span::styled(peer.to_string(), theme::dim()));
        left.push(dot);
        left.push(Span::styled(if app.connected { "node up" } else { "no node" }, theme::fg()));
        left.push(Span::styled(format!("   ⟫ running {running}"), theme::accent(if running > 0 { theme::YELLOW } else { theme::DIM })));
    }
    f.render_widget(Paragraph::new(Line::from(left)), area);
    // Tokyo time (UTC+9): the theme's one joke, and a useful second clock.
    let jst = hms(now + 9 * 3600 * 1000);
    let up = hms(app.started.elapsed().as_millis() as u64);
    let right = Line::from(vec![
        Span::styled(format!("up {up}  "), theme::dim()),
        Span::styled("東京 ", theme::accent(theme::PINK)),
        Span::styled(format!("{jst} "), theme::bold(theme::CYAN)),
        Span::styled("JST ", theme::dim()),
    ])
    .alignment(Alignment::Right);
    f.render_widget(Paragraph::new(right), area);
}

/// No node to talk to: matrix rain behind a boxed banner and the one thing to do next.
fn splash(f: &mut Frame, area: Rect, app: &App) {
    f.render_widget(Paragraph::new(theme::rain(area.width, area.height, app.frame)), area);
    let w = 54.min(area.width);
    let h = 11.min(area.height);
    let boxed = Rect { x: area.x + (area.width.saturating_sub(w)) / 2, y: area.y + (area.height.saturating_sub(h)) / 2, width: w, height: h };
    f.render_widget(ratatui::widgets::Clear, boxed);
    let mut lines: Vec<Line> = theme::BANNER
        .iter()
        .map(|l| Line::from(theme::gradient_spans(l, app.frame)).alignment(Alignment::Center))
        .collect();
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("no node running", theme::bold(theme::RED))).alignment(Alignment::Center));
    lines.push(Line::from(Span::styled(app.home.display().to_string(), theme::dim())).alignment(Alignment::Center));
    lines.push(Line::from(""));
    let hint = if app.node_started { "starting the node…" } else { "press n to start a node, q to quit" };
    lines.push(Line::from(Span::styled(hint, theme::accent(theme::YELLOW))).alignment(Alignment::Center));
    f.render_widget(Paragraph::new(lines).block(panel("OFFLINE", Line::from(""), theme::PINK)), boxed);
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
    if rows.is_empty() {
        // Idle: let the rain fall until something happens.
        let inner = block.inner(area);
        f.render_widget(block, area);
        f.render_widget(Paragraph::new(theme::rain(inner.width, inner.height, app.frame)), inner);
        return;
    }
    f.render_widget(Paragraph::new(rows).block(block), area);
}

fn hotbar(f: &mut Frame, area: Rect, app: &App) {
    if app.tab == Tab::Dash {
        let mut spans = Vec::new();
        spans.extend(theme::keycap("r", "refresh", true));
        spans.extend(theme::keycap("1", "swarm", false));
        spans.extend(theme::keycap("2", "engine", false));
        spans.extend(theme::keycap("Tab", "next tab", false));
        spans.extend(theme::keycap("q", "quit", true));
        f.render_widget(Paragraph::new(Line::from(spans)), area);
        return;
    }
    if app.tab == Tab::Engine {
        let mut spans = Vec::new();
        spans.extend(theme::keycap("j/k", "move", true));
        spans.extend(theme::keycap("h/l", "focus", true));
        spans.extend(theme::keycap("i", "chat", true));
        spans.extend(theme::keycap("N", "new", true));
        spans.extend(theme::keycap(":", "call fn", true));
        spans.extend(theme::keycap("PgUp", "scroll", true));
        spans.extend(theme::keycap("1", "swarm", false));
        spans.extend(theme::keycap("q", "quit", true));
        if let Some((msg, _)) = &app.toast {
            f.render_widget(Paragraph::new(Line::from(Span::styled(format!("{msg} "), theme::accent(theme::YELLOW))).alignment(Alignment::Right)), area);
        }
        f.render_widget(Paragraph::new(Line::from(spans)), area);
        return;
    }
    let approving = app.next_approval().is_some();
    let mut spans = Vec::new();
    if approving {
        spans.extend(theme::keycap("y", "allow", true));
        spans.extend(theme::keycap("n", "deny", true));
        spans.extend(theme::keycap("A", "always", true));
    }
    if !app.connected {
        spans.extend(theme::keycap("n", "start node", true));
    }
    spans.extend(theme::keycap("o", "ask OS", !approving));
    spans.extend(theme::keycap("a", "ask agent", !approving));
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
        crate::app::InputKind::Orchestrate => "ask OS",
        crate::app::InputKind::EngineChat => "chat",
        crate::app::InputKind::EngineNew => "new chat",
        crate::app::InputKind::EngineCall => "call",
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

pub fn ago(ms: u64) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
    let s = now.saturating_sub(ms) / 1000;
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        3600..=86399 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86400),
    }
}

/// The Engine tab: sessions + workers on the left, the selected session's transcript on the right.
fn engine_view(f: &mut Frame, area: Rect, app: &App) {
    let e = &app.engine;
    if !e.online && e.workers.is_empty() {
        f.render_widget(Paragraph::new(theme::rain(area.width, area.height, app.frame)), area);
        let w = 60.min(area.width);
        let h = 9.min(area.height);
        let boxed = Rect { x: area.x + area.width.saturating_sub(w) / 2, y: area.y + area.height.saturating_sub(h) / 2, width: w, height: h };
        f.render_widget(ratatui::widgets::Clear, boxed);
        let lines = vec![
            Line::from(Span::styled("engine offline", theme::bold(theme::RED))).alignment(Alignment::Center),
            Line::from(Span::styled(e.url.clone(), theme::dim())).alignment(Alignment::Center),
            Line::from(""),
            Line::from(Span::styled("waiting for it to answer (set SYSTEMA_ENGINE_URL to change)", theme::accent(theme::YELLOW))).alignment(Alignment::Center),
            Line::from(Span::styled("1 switches to the swarm node", theme::dim())).alignment(Alignment::Center),
        ];
        f.render_widget(Paragraph::new(lines).block(panel("ENGINE", Line::from(""), theme::PINK)), boxed);
        return;
    }
    let cols = Layout::horizontal([Constraint::Length(38), Constraint::Min(30)]).split(area);
    let left = Layout::vertical([Constraint::Percentage(55), Constraint::Min(5)]).split(cols[0]);

    // Sessions.
    let sf = e.focus == Focus::Sessions;
    let items: Vec<ListItem> = e
        .sessions
        .iter()
        .map(|s| {
            ListItem::new(Line::from(vec![
                Span::styled(format!("{:>3} ", s.messages), theme::dim()),
                Span::styled(crate::app::truncate(&s.title, 20), theme::fg()),
                Span::styled(format!(" {}", ago(s.updated)), theme::dim()),
            ]))
        })
        .collect();
    let mut st = ListState::default();
    st.select(if e.sessions.is_empty() { None } else { Some(e.sel_session) });
    let block = panel("SESSIONS", Line::from(Span::styled(format!("{} ", e.sessions.len()), theme::dim())), if sf { theme::PINK } else { theme::PURPLE });
    f.render_stateful_widget(
        List::new(items).block(block).highlight_style(Style::default().fg(theme::BG).bg(if sf { theme::PINK } else { theme::DIM }).add_modifier(Modifier::BOLD)),
        left[0],
        &mut st,
    );

    // Workers.
    let wf = e.focus == Focus::Workers;
    let items: Vec<ListItem> = e
        .workers
        .iter()
        .map(|w| {
            let c = if w.status == "connected" { theme::GREEN } else { theme::RED };
            ListItem::new(Line::from(vec![
                Span::styled("● ", theme::accent(c)),
                Span::styled(format!("{:<20}", crate::app::truncate(&w.name, 20)), theme::fg()),
                Span::styled(format!(" {:>3}ƒ", w.functions), theme::dim()),
                Span::styled(if w.active > 0 { format!(" ▶{}", w.active) } else { String::new() }, theme::accent(theme::YELLOW)),
            ]))
        })
        .collect();
    let mut st = ListState::default();
    st.select(if e.workers.is_empty() { None } else { Some(e.sel_worker) });
    let block = panel("WORKERS", Line::from(Span::styled(format!("{} ", e.workers.len()), theme::dim())), if wf { theme::CYAN } else { theme::BLUE });
    f.render_stateful_widget(
        List::new(items).block(block).highlight_style(Style::default().fg(theme::BG).bg(if wf { theme::CYAN } else { theme::DIM }).add_modifier(Modifier::BOLD)),
        left[1],
        &mut st,
    );

    // Transcript, or the last `:` call's result.
    let (title, body, color) = match &e.result {
        Some((func, text)) => (format!("CALL {func}"), text.lines().map(|l| Line::from(Span::styled(l.to_string(), theme::fg()))).collect::<Vec<_>>(), theme::YELLOW),
        None => {
            let title = e.selected_session().map(|s| crate::app::truncate(&s.title, 40)).unwrap_or_else(|| "no session".into());
            let width = cols[1].width.saturating_sub(4) as usize;
            let mut lines: Vec<Line> = Vec::new();
            for m in &e.messages {
                let (tag, c) = match m.role.as_str() {
                    "user" => ("you", theme::CYAN),
                    "assistant" => ("agent", theme::PINK),
                    "notice" => ("sys", theme::YELLOW),
                    other => (if other == "function_result" { "fn" } else { other }, theme::DIM),
                };
                lines.push(Line::from(vec![Span::styled("▌", theme::accent(c)), Span::styled(format!(" {tag}"), theme::bold(c))]));
                for l in textwrap(&m.text, width, 400) {
                    lines.push(Line::from(Span::styled(format!("  {l}"), if tag == "fn" { theme::dim() } else { theme::fg() })));
                }
                lines.push(Line::from(""));
            }
            if lines.is_empty() {
                lines.push(Line::from(Span::styled("  no messages — press i to chat, N for a new session", theme::dim())));
            }
            (title, lines, theme::PINK)
        }
    };
    let h = cols[1].height.saturating_sub(2) as usize;
    let total = body.len();
    let max_off = total.saturating_sub(h);
    // A call result reads from the top; a transcript sticks to the bottom (newest) and scrolls up.
    let off = if e.result.is_some() { (e.result_off as usize).min(max_off) } else { max_off.saturating_sub((e.scroll as usize).min(max_off)) };
    let visible: Vec<Line> = body.into_iter().skip(off).take(h).collect();
    let sub = if e.scroll > 0 { Line::from(Span::styled(format!("↑{} ", e.scroll), theme::accent(theme::YELLOW))) } else { Line::from(Span::styled("live ", theme::dim())) };
    f.render_widget(Paragraph::new(visible).block(panel(&title, sub, color)), cols[1]);
}
