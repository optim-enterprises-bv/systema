//! TUI state: the live model of the node, updated from the event stream and periodic
//! status snapshots.

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::time::Instant;

use ratatui::style::Color;
use serde_json::Value;
use systema_kernel::{Event, EventKind, TrustState};

use crate::theme;

pub const MAX_EVENTS: usize = 400;

#[derive(Clone)]
pub struct AgentRow {
    pub name: String,
    pub description: String,
    pub public: bool,
    pub skills: Vec<String>,
}

#[derive(Clone, Default)]
pub struct SessionView {
    pub state: SessionState,
    pub trust: TrustState,
    pub last_text: String,
    pub turns: usize,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum SessionState {
    #[default]
    Idle,
    Thinking,
    Tool,
    Approval,
    Done,
    Error,
    Cancelled,
}

impl SessionState {
    pub fn label(self) -> &'static str {
        match self {
            SessionState::Idle => "idle",
            SessionState::Thinking => "thinking",
            SessionState::Tool => "tool",
            SessionState::Approval => "approval",
            SessionState::Done => "done",
            SessionState::Error => "error",
            SessionState::Cancelled => "cancelled",
        }
    }

    pub fn color(self) -> Color {
        match self {
            SessionState::Idle => theme::DIM,
            SessionState::Thinking => theme::YELLOW,
            SessionState::Tool => theme::CYAN,
            SessionState::Approval => theme::RED,
            SessionState::Done => theme::GREEN,
            SessionState::Error => theme::RED,
            SessionState::Cancelled => theme::DIM,
        }
    }
}

#[derive(Clone)]
pub struct Approval {
    pub id: String,
    pub agent: String,
    pub request: String,
    pub reason: String,
    pub rememberable: bool,
}

/// One rendered line in the EVENTS log.
#[derive(Clone)]
pub struct EvRow {
    pub ts: u64,
    pub tag: &'static str,
    pub tag_color: Color,
    pub who: String,
    pub text: String,
}

#[derive(Default)]
pub struct StatusView {
    pub peer_id: String,
    pub listen: Vec<String>,
    pub peers: u64,
    pub inbox: u64,
    pub grants: u64,
    pub blocked: u64,
    pub relay_server: bool,
    pub relay_reservations: u64,
}

pub enum InputKind {
    Ask,
    Steer,
    /// Ask the OS: route to the best agent, then run.
    Orchestrate,
}

pub struct InputState {
    pub kind: InputKind,
    pub target: String,
    pub buffer: String,
}

pub struct App {
    pub home: PathBuf,
    pub connected: bool,
    pub agents: Vec<AgentRow>,
    pub selected: usize,
    pub sessions: BTreeMap<String, SessionView>,
    pub events: VecDeque<EvRow>,
    pub approvals: Vec<Approval>,
    pub status: StatusView,
    pub input: Option<InputState>,
    pub toast: Option<(String, Instant)>,
    pub should_quit: bool,
    /// Animation frame, advanced by the UI tick.
    pub frame: u64,
    pub started: Instant,
    /// A node was launched from this session and has not answered yet.
    pub node_started: bool,
}

impl App {
    pub fn new(home: PathBuf) -> Self {
        Self {
            home,
            connected: false,
            agents: Vec::new(),
            selected: 0,
            sessions: BTreeMap::new(),
            events: VecDeque::new(),
            approvals: Vec::new(),
            status: StatusView::default(),
            input: None,
            toast: None,
            should_quit: false,
            frame: 0,
            started: Instant::now(),
            node_started: false,
        }
    }

    pub fn selected_agent(&self) -> Option<&AgentRow> {
        self.agents.get(self.selected)
    }

    pub fn session_state(&self, agent: &str) -> SessionState {
        self.sessions.get(agent).map(|s| s.state).unwrap_or_default()
    }

    pub fn toast(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), Instant::now()));
    }

    pub fn move_selection(&mut self, delta: isize) {
        if self.agents.is_empty() {
            return;
        }
        let n = self.agents.len() as isize;
        self.selected = (((self.selected as isize + delta) % n + n) % n) as usize;
    }

    /// The oldest pending approval (the one the hotbar's y/n/A acts on).
    pub fn next_approval(&self) -> Option<&Approval> {
        self.approvals.first()
    }

    pub fn set_agents(&mut self, v: &Value) {
        if let Some(arr) = v.as_array() {
            self.agents = arr
                .iter()
                .map(|a| AgentRow {
                    name: a["name"].as_str().unwrap_or("?").to_string(),
                    description: a["description"].as_str().unwrap_or("").to_string(),
                    public: a["public"].as_bool().unwrap_or(false),
                    skills: a["skills"].as_array().into_iter().flatten().filter_map(|s| s.as_str().map(String::from)).collect(),
                })
                .collect();
            if self.selected >= self.agents.len() {
                self.selected = self.agents.len().saturating_sub(1);
            }
        }
    }

    /// Replace the pending-approval list from the node's authoritative snapshot, so a UI
    /// that connected after an approval was raised still shows and can answer it.
    pub fn set_approvals(&mut self, v: &Value) {
        if let Some(arr) = v.as_array() {
            self.approvals = arr
                .iter()
                .map(|a| Approval {
                    id: a["id"].as_str().unwrap_or("").to_string(),
                    agent: a["agent"].as_str().unwrap_or("").to_string(),
                    request: a["request"].as_str().unwrap_or("").to_string(),
                    reason: a["reason"].as_str().unwrap_or("").to_string(),
                    rememberable: a["rememberable"].as_bool().unwrap_or(false),
                })
                .collect();
            for a in &self.approvals.clone() {
                if !a.agent.is_empty() {
                    self.session_mut(&a.agent).state = SessionState::Approval;
                }
            }
        }
    }

    pub fn set_status(&mut self, v: &Value) {
        self.status.peer_id = v["peer_id"].as_str().unwrap_or("").to_string();
        self.status.listen = v["listen"].as_array().into_iter().flatten().filter_map(|a| a.as_str().map(String::from)).collect();
        self.status.peers = v["peers"].as_u64().unwrap_or(0);
        self.status.inbox = v["inbox"].as_u64().unwrap_or(0);
        self.status.grants = v["grants"].as_u64().unwrap_or(0);
        self.status.blocked = v["blocked"].as_u64().unwrap_or(0);
        self.status.relay_server = v["relay_server"].as_bool().unwrap_or(false);
        self.status.relay_reservations = v["relay_reservations"].as_u64().unwrap_or(0);
    }

    fn session_mut(&mut self, key: &str) -> &mut SessionView {
        self.sessions.entry(key.to_string()).or_default()
    }

    fn push_event(&mut self, ts: u64, tag: &'static str, color: Color, who: String, text: String) {
        self.events.push_front(EvRow { ts, tag, tag_color: color, who, text });
        self.events.truncate(MAX_EVENTS);
    }

    /// Fold one kernel event into the state.
    pub fn ingest(&mut self, ev: Event) {
        let who = ev.session.clone().or_else(|| ev.agent.clone()).unwrap_or_default();
        let ts = ev.ts;
        match ev.kind {
            EventKind::TurnStarted { prompt } => {
                self.session_mut(&who).state = SessionState::Thinking;
                self.session_mut(&who).turns += 1;
                self.push_event(ts, "turn", theme::BLUE, who, truncate(&prompt, 200));
            }
            EventKind::Thinking => {
                if let Some(s) = self.sessions.get_mut(&who) {
                    if s.state != SessionState::Approval {
                        s.state = SessionState::Thinking;
                    }
                }
            }
            EventKind::ModelText { text } => {
                self.session_mut(&who).last_text = text.clone();
                self.push_event(ts, "model", theme::FG, who, truncate(&text, 200));
            }
            EventKind::ToolCall { request } => {
                self.session_mut(&who).state = SessionState::Tool;
                self.push_event(ts, "tool", theme::CYAN, who, request);
            }
            EventKind::ToolResult { request, outcome, trust } => {
                let s = self.session_mut(&who);
                s.trust = trust;
                let denied = outcome.starts_with("denied") || outcome.starts_with("not approved");
                let color = if denied { theme::RED } else { theme::GREEN };
                self.push_event(ts, "result", color, who, format!("{request} → {outcome}"));
            }
            EventKind::ApprovalNeeded { id, request, reason, rememberable } => {
                let agent = ev.agent.clone().unwrap_or_default();
                if !agent.is_empty() {
                    self.session_mut(&agent).state = SessionState::Approval;
                }
                self.approvals.push(Approval { id, agent: agent.clone(), request: request.clone(), reason, rememberable });
                self.push_event(ts, "APPROVE", theme::RED, agent, request);
            }
            EventKind::ApprovalResolved { id, decision } => {
                self.approvals.retain(|a| a.id != id);
                self.push_event(ts, "resolved", theme::PURPLE, ev.agent.unwrap_or_default(), decision);
            }
            EventKind::Steered { text } => {
                self.push_event(ts, "steer", theme::PURPLE, who, text);
            }
            EventKind::TurnFinished { ok, text } => {
                let cancelled = !ok && text.contains("cancelled");
                let s = self.session_mut(&who);
                s.state = if ok {
                    SessionState::Done
                } else if cancelled {
                    SessionState::Cancelled
                } else {
                    SessionState::Error
                };
                s.last_text = text.clone();
                let color = if ok { theme::GREEN } else if cancelled { theme::DIM } else { theme::RED };
                self.push_event(ts, "finish", color, who, truncate(&text, 200));
            }
            EventKind::Inbound { peer, skill, outcome } => {
                self.push_event(ts, "inbound", theme::YELLOW, short_peer(&peer), format!("{skill} → {outcome}"));
            }
            EventKind::Discovered { peer, skills } => {
                self.push_event(ts, "found", theme::BLUE, short_peer(&peer), skills.join(", "));
            }
            EventKind::Note { text } => {
                self.push_event(ts, "note", theme::DIM, who, text);
            }
            EventKind::BoardPosted { topic, text } => {
                self.push_event(ts, "board", theme::PURPLE, who, format!("[{topic}] {text}"));
            }
        }
    }
}

pub fn truncate(s: &str, n: usize) -> String {
    let s = s.replace('\n', " ");
    if s.chars().count() > n {
        let cut: String = s.chars().take(n).collect();
        format!("{cut}…")
    } else {
        s
    }
}

pub fn short_peer(p: &str) -> String {
    if p.len() > 10 { format!("{}…", &p[..10]) } else { p.to_string() }
}
