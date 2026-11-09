//! `systema-tui` (also `systema tui`): a cockpit to monitor, steer and approve agents on a
//! running node. Connects to the node's control socket: streams events over `watch`, issues
//! `ask`/`steer`/`cancel`/`resolve` as commands.

mod app;
mod engine;
mod theme;
mod ui;

use std::path::PathBuf;
use std::time::Duration;

use app::{App, Focus, InputKind, InputState, Tab};
use ratatui::crossterm::event::{self, Event as CEvent, KeyCode, KeyEventKind, KeyModifiers};
use serde_json::Value;
use systema_kernel::{Answer, Event};
use systema_node::control::{self, Control};
use tokio::sync::mpsc;

enum WatchMsg {
    Connected,
    Disconnected,
    Ev(Box<Event>),
}

fn home_dir() -> PathBuf {
    if let Some(h) = std::env::args().nth(1).filter(|a| !a.starts_with('-')) {
        return PathBuf::from(h);
    }
    if let Some(h) = std::env::var_os("SYSTEMA_HOME") {
        return h.into();
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| ".".into());
    home.join(".systema")
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let home = home_dir();

    // Keyboard: crossterm reads block, so poll on a dedicated thread.
    let (key_tx, mut key_rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        loop {
            match event::poll(Duration::from_millis(200)) {
                Ok(true) => {
                    if let Ok(CEvent::Key(k)) = event::read() {
                        if k.kind == KeyEventKind::Press && key_tx.send(k).is_err() {
                            break;
                        }
                    }
                }
                Ok(false) => {}
                Err(_) => break,
            }
        }
    });

    // Event stream, reconnecting if the node restarts.
    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
    tokio::spawn(watch_task(home.clone(), ev_tx));

    let mut appstate = App::new(home.clone());
    // The engine is what most people run all day, so the cockpit opens on it; `1` switches to the swarm.
    appstate.tab = if std::env::var("SYSTEMA_TAB").is_ok_and(|t| t == "swarm") { Tab::Swarm } else { Tab::Engine };
    let (engine_tx, mut engine_rx) = engine::spawn(appstate.engine.url.clone());
    appstate.engine_tx = Some(engine_tx);
    refresh(&mut appstate).await;

    let mut terminal = ratatui::init();
    let mut tick = tokio::time::interval(Duration::from_millis(120));
    let mut refresh_iv = tokio::time::interval(Duration::from_secs(2));
    refresh_iv.reset();

    let result = loop {
        if let Err(e) = terminal.draw(|f| ui::draw(f, &appstate)) {
            break Err(anyhow::Error::from(e));
        }
        tokio::select! {
            Some(k) = key_rx.recv() => handle_key(&mut appstate, k).await,
            Some(m) = ev_rx.recv() => match m {
                WatchMsg::Connected => appstate.connected = true,
                WatchMsg::Disconnected => appstate.connected = false,
                WatchMsg::Ev(e) => appstate.ingest(*e),
            },
            Some(msg) = engine_rx.recv() => apply_engine(&mut appstate, msg),
            _ = tick.tick() => {
                appstate.frame = appstate.frame.wrapping_add(1);
                if let Some((_, at)) = &appstate.toast {
                    if at.elapsed() > Duration::from_secs(4) {
                        appstate.toast = None;
                    }
                }
            }
            _ = refresh_iv.tick() => refresh(&mut appstate).await,
        }
        if appstate.should_quit {
            break Ok(());
        }
    };
    ratatui::restore();
    result
}

async fn watch_task(home: PathBuf, tx: mpsc::UnboundedSender<WatchMsg>) {
    loop {
        match control::watch(&home).await {
            Ok(mut lines) => {
                if tx.send(WatchMsg::Connected).is_err() {
                    return;
                }
                while let Ok(Some(line)) = lines.next_line().await {
                    if let Ok(ev) = serde_json::from_str::<Event>(&line) {
                        if tx.send(WatchMsg::Ev(Box::new(ev))).is_err() {
                            return;
                        }
                    }
                }
            }
            Err(_) => {}
        }
        if tx.send(WatchMsg::Disconnected).is_err() {
            return;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Pull a one-shot snapshot (agents, node vitals).
async fn refresh(app: &mut App) {
    match control::request(&app.home, &Control::Agents).await {
        Ok(v) => {
            app.set_agents(&v);
            app.connected = true;
        }
        Err(_) => app.connected = false,
    }
    if let Ok(v) = control::request(&app.home, &Control::Status).await {
        app.set_status(&v);
    }
    if let Ok(v) = control::request(&app.home, &Control::Approvals).await {
        app.set_approvals(&v);
    }
}

async fn cmd(app: &mut App, c: Control) -> Result<Value, String> {
    control::request(&app.home, &c).await.map_err(|e| e.to_string())
}

async fn handle_key(app: &mut App, k: ratatui::crossterm::event::KeyEvent) {
    // Input mode: capture text for ask/steer.
    if let Some(mut input) = app.input.take() {
        match k.code {
            KeyCode::Esc => {}
            KeyCode::Enter => {
                let text = input.buffer.trim().to_string();
                if !text.is_empty() {
                    match input.kind {
                        InputKind::EngineChat | InputKind::EngineNew => {
                            let session = matches!(input.kind, InputKind::EngineChat).then(|| input.target.clone());
                            engine_cmd(app, engine::Cmd::Send { session, text });
                        }
                        InputKind::EngineCall => {
                            let (function, rest) = text.split_once(char::is_whitespace).map_or((text.as_str(), ""), |(f, r)| (f, r.trim()));
                            match if rest.is_empty() { Ok(serde_json::json!({})) } else { serde_json::from_str::<Value>(rest) } {
                                Ok(payload) => engine_cmd(app, engine::Cmd::Call { function: function.to_string(), payload }),
                                Err(e) => app.toast(format!("payload is not JSON: {e}")),
                            }
                        }
                        InputKind::Orchestrate => {
                            // Route to the best agent, then run it there.
                            match cmd(app, Control::Route { text: text.clone() }).await {
                                Ok(v) => {
                                    let agent = v["agent"].as_str().unwrap_or("operator").to_string();
                                    match cmd(app, Control::Ask { agent: agent.clone(), text }).await {
                                        Ok(_) => app.toast(format!("routed to {agent}")),
                                        Err(e) => app.toast(e),
                                    }
                                }
                                Err(e) => app.toast(e),
                            }
                        }
                        InputKind::Ask => {
                            let c = Control::Ask { agent: input.target.clone(), text };
                            match cmd(app, c).await {
                                Ok(_) => app.toast(format!("sent to {}", input.target)),
                                Err(e) => app.toast(e),
                            }
                        }
                        InputKind::Steer => {
                            let c = Control::Steer { session: input.target.clone(), text };
                            match cmd(app, c).await {
                                Ok(_) => app.toast(format!("sent to {}", input.target)),
                                Err(e) => app.toast(e),
                            }
                        }
                    }
                }
            }
            KeyCode::Backspace => {
                input.buffer.pop();
                app.input = Some(input);
            }
            KeyCode::Char(ch) => {
                input.buffer.push(ch);
                app.input = Some(input);
            }
            _ => app.input = Some(input),
        }
        return;
    }

    // Tabs: 1 = swarm node, 2 = engine.
    match k.code {
        KeyCode::Char('1') => {
            app.tab = Tab::Swarm;
            return;
        }
        KeyCode::Char('2') => {
            app.tab = Tab::Engine;
            return;
        }
        KeyCode::Tab => {
            app.tab = if app.tab == Tab::Swarm { Tab::Engine } else { Tab::Swarm };
            return;
        }
        _ => {}
    }
    if app.tab == Tab::Engine && engine_key(app, k) {
        return;
    }

    // Approval answers take priority when one is pending.
    if let Some(a) = app.next_approval().cloned() {
        let answer = match k.code {
            KeyCode::Char('y') => Some((Answer::Once, false)),
            KeyCode::Char('n') => Some((Answer::Deny, false)),
            KeyCode::Char('A') => a.rememberable.then_some((Answer::Always, true)),
            _ => None,
        };
        if let Some((answer, remember)) = answer {
            match cmd(app, Control::Resolve { id: a.id.clone(), answer, remember }).await {
                Ok(_) => app.toast(format!("approval {}: {answer:?}", a.id)),
                Err(e) => app.toast(e),
            }
            return;
        }
    }

    match k.code {
        KeyCode::Char('q') => app.should_quit = true,
        KeyCode::Char('n') if !app.connected => start_node(app),
        KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => app.should_quit = true,
        KeyCode::Down | KeyCode::Char('j') => app.move_selection(1),
        KeyCode::Up | KeyCode::Char('k') => app.move_selection(-1),
        KeyCode::Char('o') => {
            app.input = Some(InputState { kind: InputKind::Orchestrate, target: "OS".into(), buffer: String::new() });
        }
        KeyCode::Char('a') => {
            if let Some(agent) = app.selected_agent() {
                app.input = Some(InputState { kind: InputKind::Ask, target: agent.name.clone(), buffer: String::new() });
            }
        }
        KeyCode::Char('s') => {
            if let Some(agent) = app.selected_agent() {
                app.input = Some(InputState { kind: InputKind::Steer, target: agent.name.clone(), buffer: String::new() });
            }
        }
        KeyCode::Char('c') => {
            if let Some(agent) = app.selected_agent().map(|a| a.name.clone()) {
                match cmd(app, Control::Cancel { session: agent.clone() }).await {
                    Ok(_) => app.toast(format!("cancelled {agent}")),
                    Err(e) => app.toast(e),
                }
            }
        }
        _ => {}
    }
}

/// Launch `systema node` for this home, detached, with its output in `<home>/node.log`. The cockpit
/// keeps polling, so it connects as soon as the node's control socket answers.
fn start_node(app: &mut App) {
    use std::os::unix::process::CommandExt as _;
    if app.node_started {
        app.toast("node is already starting");
        return;
    }
    let exe = std::env::current_exe().ok();
    let sibling = exe.as_ref().map(|e| e.with_file_name("systema")).filter(|p| p.exists());
    let bin = sibling.unwrap_or_else(|| PathBuf::from("systema"));
    let _ = std::fs::create_dir_all(&app.home);
    let log = std::fs::OpenOptions::new().create(true).append(true).open(app.home.join("node.log"));
    let (out, err) = match log.and_then(|l| l.try_clone().map(|c| (l, c))) {
        Ok((a, b)) => (std::process::Stdio::from(a), std::process::Stdio::from(b)),
        Err(_) => (std::process::Stdio::null(), std::process::Stdio::null()),
    };
    match std::process::Command::new(&bin)
        .arg("--home")
        .arg(&app.home)
        .arg("node")
        .stdin(std::process::Stdio::null())
        .stdout(out)
        .stderr(err)
        .process_group(0)
        .spawn()
    {
        Ok(_) => {
            app.node_started = true;
            app.toast(format!("started {} node (log: node.log)", bin.display()));
        }
        Err(e) => app.toast(format!("could not start {}: {e}", bin.display())),
    }
}

fn engine_cmd(app: &mut App, c: engine::Cmd) {
    if let Some(tx) = &app.engine_tx {
        let _ = tx.send(c);
    }
}

fn select_current(app: &mut App) {
    let id = app.engine.selected_session().map(|s| s.id.clone());
    app.engine.messages.clear();
    app.engine.msg_session = id.clone();
    app.engine.scroll = 0;
    engine_cmd(app, engine::Cmd::Select(id));
}

/// Keys for the Engine tab. Returns false for the keys the shared handler owns (quit).
fn engine_key(app: &mut App, k: ratatui::crossterm::event::KeyEvent) -> bool {
    match k.code {
        KeyCode::Char('q') => return false,
        KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => return false,
        KeyCode::Down | KeyCode::Char('j') => {
            app.engine.move_sel(1);
            if app.engine.focus == Focus::Sessions {
                select_current(app);
            }
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.engine.move_sel(-1);
            if app.engine.focus == Focus::Sessions {
                select_current(app);
            }
        }
        KeyCode::Left | KeyCode::Right | KeyCode::Char('h') | KeyCode::Char('l') => {
            app.engine.focus = if app.engine.focus == Focus::Sessions { Focus::Workers } else { Focus::Sessions };
        }
        KeyCode::Char('i') | KeyCode::Enter => match app.engine.selected_session().map(|s| s.id.clone()) {
            Some(id) => app.input = Some(InputState { kind: InputKind::EngineChat, target: id, buffer: String::new() }),
            None => app.toast("no session selected: press N for a new one"),
        },
        KeyCode::Char('N') => {
            app.input = Some(InputState { kind: InputKind::EngineNew, target: "new session".into(), buffer: String::new() });
        }
        KeyCode::Char(':') => {
            app.input = Some(InputState { kind: InputKind::EngineCall, target: "call".into(), buffer: String::new() });
        }
        KeyCode::Char('r') => engine_cmd(app, engine::Cmd::Refresh),
        KeyCode::Esc => app.engine.result = None,
        KeyCode::PageUp if app.engine.result.is_some() => app.engine.result_off = app.engine.result_off.saturating_sub(8),
        KeyCode::PageDown if app.engine.result.is_some() => app.engine.result_off = app.engine.result_off.saturating_add(8),
        KeyCode::PageUp => app.engine.scroll = app.engine.scroll.saturating_add(8),
        KeyCode::PageDown => app.engine.scroll = app.engine.scroll.saturating_sub(8),
        KeyCode::Char('G') | KeyCode::End => app.engine.scroll = 0,
        _ => {}
    }
    true
}

fn apply_engine(app: &mut App, msg: engine::Msg) {
    let e = &mut app.engine;
    match msg {
        engine::Msg::Online(on) => e.online = on,
        engine::Msg::Workers(w) => {
            e.workers = w;
            e.sel_worker = e.sel_worker.min(e.workers.len().saturating_sub(1));
        }
        engine::Msg::Sessions(s) => {
            // Keep the same session selected when the list reorders; honour a just-opened one.
            let keep = e.want.take().or_else(|| e.msg_session.clone());
            e.sessions = s;
            if let Some(id) = keep {
                if let Some(i) = e.sessions.iter().position(|x| x.id == id) {
                    e.sel_session = i;
                }
            }
            e.sel_session = e.sel_session.min(e.sessions.len().saturating_sub(1));
            if e.msg_session.is_none() {
                let id = e.selected_session().map(|s| s.id.clone());
                e.msg_session = id.clone();
                if let Some(tx) = &app.engine_tx {
                    let _ = tx.send(engine::Cmd::Select(id));
                }
            }
        }
        engine::Msg::Messages { session, rows } => {
            if e.msg_session.as_deref() == Some(session.as_str()) {
                e.messages = rows;
            }
        }
        engine::Msg::Opened(id) => {
            e.want = Some(id.clone());
            e.msg_session = Some(id);
            e.messages.clear();
            e.scroll = 0;
        }
        engine::Msg::Toast(t) => app.toast(t),
        engine::Msg::CallResult { function, text } => {
            e.result = Some((function, text));
            e.result_off = 0;
        }
    }
}
