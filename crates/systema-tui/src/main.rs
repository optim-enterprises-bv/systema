//! `systema-tui` (also `systema tui`): a cockpit to monitor, steer and approve agents on a
//! running node. Connects to the node's control socket: streams events over `watch`, issues
//! `ask`/`steer`/`cancel`/`resolve` as commands.

mod app;
mod theme;
mod ui;

use std::path::PathBuf;
use std::time::Duration;

use app::{App, InputKind, InputState};
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
    if !control::socket_path(&home).exists() {
        eprintln!(
            "No running node at {} — start one with `systema node` first.",
            control::socket_path(&home).display()
        );
        std::process::exit(1);
    }

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
    refresh(&mut appstate).await;

    let mut terminal = ratatui::init();
    let mut tick = tokio::time::interval(Duration::from_millis(250));
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
            _ = tick.tick() => {
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
