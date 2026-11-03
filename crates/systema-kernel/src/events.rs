//! Live kernel events, broadcast to anything watching (the TUI, `systema watch`).
//! Events are observations, never commands: nothing that subscribes can change state
//! through this channel.

use serde::{Deserialize, Serialize};

use crate::capability::TrustState;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub ts: u64,
    /// Agent the event concerns, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Session key (`coder`, `coder@<peer>`), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(flatten)]
    pub kind: EventKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum EventKind {
    TurnStarted { prompt: String },
    /// Waiting for a GPU slot / generating.
    Thinking,
    ModelText { text: String },
    ToolCall { request: String },
    ToolResult { request: String, outcome: String, trust: TrustState },
    ApprovalNeeded { id: String, request: String, reason: String, rememberable: bool },
    ApprovalResolved { id: String, decision: String },
    Steered { text: String },
    TurnFinished { ok: bool, text: String },
    Inbound { peer: String, skill: String, outcome: String },
    Discovered { peer: String, skills: Vec<String> },
    /// A local agent posted to the shared board; the node gossips it to the swarm (blackboard).
    BoardPosted { topic: String, text: String },
    Note { text: String },
}

/// Live state of one session, for status views.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SessionStatus {
    pub agent: String,
    pub session: String,
    /// idle | thinking | tool | approval | cancelled | error
    pub state: String,
    pub trust: TrustState,
    pub turns: usize,
    pub last_ts: u64,
}
