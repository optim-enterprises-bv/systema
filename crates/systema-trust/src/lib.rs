//! The trust gate. Every message that crosses the node boundary passes through here
//! *before* any model sees it. Rules are enforced in code, never delegated to an LLM:
//!
//! 1. Unknown peers are quarantined (no tools, no memory) or held for human approval.
//! 2. Known peers get narrow, expiring, rate-limited grants per skill (UCAN-style).
//! 3. Forwarding a message onward is a kernel decision; a model asking to forward is denied.
//!    That single rule breaks the self-replicating prompt-injection ("AI worm") path.
//! 4. Nonces are remembered so a captured envelope cannot be replayed.

use std::collections::{HashMap, HashSet, VecDeque};

use serde::{Deserialize, Serialize};
use systema_proto::libp2p_identity::PeerId;

/// Where a message came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// Another agent in this kernel.
    Local(String),
    /// A remote node.
    Peer(PeerId),
}

/// What to do when a peer with no grant contacts us.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FirstContact {
    /// Hold it in the approvals inbox for the human.
    #[default]
    AskHuman,
    /// Answer from a tool-less, memory-less quarantine agent.
    Quarantine,
    Deny,
}

/// Permission for one peer to invoke one skill.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Grant {
    pub peer: String,
    /// Skill id, or `*` for any skill this node exposes.
    pub skill: String,
    pub max_per_hour: u32,
    /// Unix ms; `None` = until revoked.
    pub expires_at_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Hand to the agent that owns the skill, with that agent's normal capabilities.
    Allow,
    Quarantine,
    AskHuman,
    Deny(&'static str),
}

/// Who wants a message re-sent to other peers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForwardCause {
    /// A kernel rule (e.g. task-board fan-out the human configured).
    KernelPolicy,
    /// A model's output asked for it. Never honoured.
    ModelRequested,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Policy {
    #[serde(default)]
    pub first_contact: FirstContact,
    #[serde(default)]
    pub grants: Vec<Grant>,
    #[serde(default)]
    pub blocked: HashSet<String>,
}

/// Stateful gate: policy plus rate counters and replay memory.
pub struct TrustGate {
    policy: Policy,
    /// (peer, skill) -> timestamps of recent allowed calls.
    calls: HashMap<(String, String), VecDeque<u64>>,
    seen_nonces: HashSet<String>,
    nonce_order: VecDeque<(u64, String)>,
    nonce_window_ms: u64,
}

impl TrustGate {
    pub fn new(policy: Policy) -> Self {
        Self {
            policy,
            calls: HashMap::new(),
            seen_nonces: HashSet::new(),
            nonce_order: VecDeque::new(),
            nonce_window_ms: systema_proto::DEFAULT_MAX_AGE_MS,
        }
    }

    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    pub fn grant(&mut self, grant: Grant) {
        self.policy.grants.push(grant);
    }

    pub fn block(&mut self, peer: &PeerId) {
        let p = peer.to_string();
        self.policy.grants.retain(|g| g.peer != p);
        self.policy.blocked.insert(p);
    }

    /// Returns false if this nonce was already seen inside the freshness window.
    pub fn check_replay(&mut self, nonce: &str, now_ms: u64) -> bool {
        while let Some((t, _)) = self.nonce_order.front() {
            if now_ms.saturating_sub(*t) <= self.nonce_window_ms {
                break;
            }
            let (_, n) = self.nonce_order.pop_front().unwrap();
            self.seen_nonces.remove(&n);
        }
        if !self.seen_nonces.insert(nonce.to_string()) {
            return false;
        }
        self.nonce_order.push_back((now_ms, nonce.to_string()));
        true
    }

    /// Decide what happens to an inbound request for `skill` from `origin`.
    pub fn evaluate(&mut self, origin: &Origin, skill: Option<&str>, now_ms: u64) -> Decision {
        let peer = match origin {
            Origin::Local(_) => return Decision::Allow,
            Origin::Peer(p) => p.to_string(),
        };
        if self.policy.blocked.contains(&peer) {
            return Decision::Deny("peer is blocked");
        }
        let skill = skill.unwrap_or("");
        let grant = self.policy.grants.iter().find(|g| {
            g.peer == peer && (g.skill == "*" || g.skill == skill) && g.expires_at_ms.is_none_or(|e| e > now_ms)
        });
        let Some(grant) = grant else {
            return match self.policy.first_contact {
                FirstContact::AskHuman => Decision::AskHuman,
                FirstContact::Quarantine => Decision::Quarantine,
                FirstContact::Deny => Decision::Deny("no grant"),
            };
        };
        let limit = grant.max_per_hour as usize;
        let window = self.calls.entry((peer, skill.to_string())).or_default();
        while window.front().is_some_and(|t| now_ms.saturating_sub(*t) > 3_600_000) {
            window.pop_front();
        }
        if window.len() >= limit {
            return Decision::Deny("rate limit");
        }
        window.push_back(now_ms);
        Decision::Allow
    }

    /// Whether a message may be re-sent to other peers.
    pub fn may_forward(&self, cause: ForwardCause) -> bool {
        matches!(cause, ForwardCause::KernelPolicy)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use systema_proto::libp2p_identity::Keypair;

    fn peer() -> PeerId {
        Keypair::generate_ed25519().public().to_peer_id()
    }

    #[test]
    fn local_always_allowed() {
        let mut g = TrustGate::new(Policy::default());
        assert_eq!(g.evaluate(&Origin::Local("coder".into()), None, 0), Decision::Allow);
    }

    #[test]
    fn unknown_peer_goes_to_human() {
        let mut g = TrustGate::new(Policy::default());
        assert_eq!(g.evaluate(&Origin::Peer(peer()), Some("code-review"), 0), Decision::AskHuman);
    }

    #[test]
    fn grant_is_scoped_rate_limited_and_expires() {
        let p = peer();
        let mut g = TrustGate::new(Policy::default());
        g.grant(Grant { peer: p.to_string(), skill: "code-review".into(), max_per_hour: 2, expires_at_ms: Some(10_000) });
        let o = Origin::Peer(p);
        assert_eq!(g.evaluate(&o, Some("code-review"), 1), Decision::Allow);
        assert_eq!(g.evaluate(&o, Some("code-review"), 2), Decision::Allow);
        assert_eq!(g.evaluate(&o, Some("code-review"), 3), Decision::Deny("rate limit"));
        assert_eq!(g.evaluate(&o, Some("shell"), 4), Decision::AskHuman);
        assert_eq!(g.evaluate(&o, Some("code-review"), 20_000), Decision::AskHuman);
    }

    #[test]
    fn blocked_beats_grant() {
        let p = peer();
        let mut g = TrustGate::new(Policy::default());
        g.grant(Grant { peer: p.to_string(), skill: "*".into(), max_per_hour: 100, expires_at_ms: None });
        g.block(&p);
        assert_eq!(g.evaluate(&Origin::Peer(p), Some("x"), 0), Decision::Deny("peer is blocked"));
    }

    #[test]
    fn replay_rejected() {
        let mut g = TrustGate::new(Policy::default());
        assert!(g.check_replay("n1", 0));
        assert!(!g.check_replay("n1", 1));
        assert!(g.check_replay("n1", systema_proto::DEFAULT_MAX_AGE_MS + 10));
    }

    #[test]
    fn models_cannot_forward() {
        let g = TrustGate::new(Policy::default());
        assert!(!g.may_forward(ForwardCause::ModelRequested));
        assert!(g.may_forward(ForwardCause::KernelPolicy));
    }
}
