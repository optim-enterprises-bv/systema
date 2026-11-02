//! Approvals: how `Ask` decisions are answered, and the operator's remembered answers.
//!
//! Only asks that come from the *manifest* (`mode = "ask"`, `ask_tools`, `shell = true`,
//! writes from an untrusted session) can be remembered, and only for the exact resolved
//! target (`run /usr/bin/journalctl -u nginx`, `write /home/me/src/x.rs`). Rule-of-Two
//! escalations are never rememberable: an "always allow" there would permanently reopen the
//! exfiltration path the rule exists to close.
//!
//! Remembered rules live in `$SYSTEMA_HOME/approvals.json`, inside the protected Systema
//! home, so agents can neither read nor extend them.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use serde::{Deserialize, Serialize};

use crate::capability::{Checked, Target, ToolRequest};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Answer {
    /// Allow this one call.
    Once,
    /// Allow and remember (only honoured when the ask is rememberable).
    Always,
    /// Refuse this call.
    Deny,
}

/// Decides `Ask` requests. `rememberable` is false for Rule-of-Two escalations.
pub trait Approver: Send + Sync {
    fn approve(&self, agent: &str, request: &ToolRequest, reason: &str, rememberable: bool) -> Answer;
}

/// No human present (the node): refuse every ask.
pub struct DenyAll;

impl Approver for DenyAll {
    fn approve(&self, _: &str, _: &ToolRequest, _: &str, _: bool) -> Answer {
        Answer::Deny
    }
}

/// One remembered "always allow" for an exact (agent, target).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    pub agent: String,
    /// Exact resolved target; see [`rule_key`].
    pub key: String,
    pub created_ms: u64,
}

/// The exact thing an "always" answer covers. Built from the *resolved* target, never the
/// raw string the model sent, so a remembered `run git status` can't be widened by argument
/// tricks. Returns `None` for requests that must never be remembered (fetches: the host is
/// attacker-influenced), so the caller treats them as non-rememberable.
pub fn rule_key(req: &ToolRequest, checked: &Checked) -> Option<String> {
    match (req, &checked.target) {
        (ToolRequest::Run { args, .. }, Target::Program(p)) => {
            Some(format!("run {} {}", p.display(), args.join(" ")).trim_end().to_string())
        }
        (ToolRequest::WriteFile { .. }, Target::Path(p)) => Some(format!("write {}", p.display())),
        (ToolRequest::ReadFile { .. }, Target::Path(p)) => Some(format!("read {}", p.display())),
        (ToolRequest::ListDir { .. }, Target::Path(p)) => Some(format!("list {}", p.display())),
        (ToolRequest::Mcp { server, tool, .. }, Target::Mcp) => Some(format!("mcp {server}.{tool}")),
        (ToolRequest::Fetch { .. }, _) => None,
        _ => None,
    }
}

/// Remembered "always allow" rules, persisted to `approvals.json`.
pub struct ApprovalStore {
    path: PathBuf,
    rules: RwLock<Vec<Rule>>,
}

impl ApprovalStore {
    pub fn load(home: &Path) -> Self {
        let path = home.join("approvals.json");
        let rules = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Self { path, rules: RwLock::new(rules) }
    }

    /// Whether (agent, key) has a remembered allow.
    pub fn is_remembered(&self, agent: &str, key: &str) -> bool {
        self.rules.read().unwrap().iter().any(|r| r.agent == agent && r.key == key)
    }

    /// Add a rule (idempotent) and persist.
    pub fn remember(&self, agent: &str, key: &str) {
        {
            let mut rules = self.rules.write().unwrap();
            if rules.iter().any(|r| r.agent == agent && r.key == key) {
                return;
            }
            rules.push(Rule { agent: agent.to_string(), key: key.to_string(), created_ms: crate::now_ms() });
        }
        self.save();
    }

    /// Remove a remembered rule (for the UI's "forget"). Returns whether one was removed.
    pub fn forget(&self, agent: &str, key: &str) -> bool {
        let removed = {
            let mut rules = self.rules.write().unwrap();
            let before = rules.len();
            rules.retain(|r| !(r.agent == agent && r.key == key));
            rules.len() != before
        };
        if removed {
            self.save();
        }
        removed
    }

    /// Drop every remembered rule for an agent (e.g. on manifest change). Returns the count.
    pub fn forget_agent(&self, agent: &str) -> usize {
        let n = {
            let mut rules = self.rules.write().unwrap();
            let before = rules.len();
            rules.retain(|r| r.agent != agent);
            before - rules.len()
        };
        if n > 0 {
            self.save();
        }
        n
    }

    pub fn rules(&self) -> Vec<Rule> {
        self.rules.read().unwrap().clone()
    }

    fn save(&self) {
        let snapshot = self.rules.read().unwrap().clone();
        let tmp = self.path.with_extension("tmp");
        if let Ok(bytes) = serde_json::to_vec_pretty(&snapshot) {
            if std::fs::write(&tmp, bytes).and_then(|_| std::fs::rename(&tmp, &self.path)).is_err() {
                tracing::warn!("could not persist approvals to {}", self.path.display());
            }
        }
    }
}

/// Agents named in `approvals.json` but absent from a fresh manifest set — stale rules the
/// UI may offer to clear.
pub fn stale_agents(store: &ApprovalStore, live: &HashSet<String>) -> Vec<String> {
    let mut v: Vec<String> = store.rules().into_iter().map(|r| r.agent).filter(|a| !live.contains(a)).collect();
    v.sort();
    v.dedup();
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (ApprovalStore, PathBuf) {
        let dir = std::env::temp_dir().join(format!("systema-appr-{}", systema_proto::random_id()));
        std::fs::create_dir_all(&dir).unwrap();
        (ApprovalStore::load(&dir), dir)
    }

    #[test]
    fn remember_is_idempotent_and_persists() {
        let (s, dir) = store();
        assert!(!s.is_remembered("coder", "run /usr/bin/git status"));
        s.remember("coder", "run /usr/bin/git status");
        s.remember("coder", "run /usr/bin/git status");
        assert_eq!(s.rules().len(), 1);
        assert!(s.is_remembered("coder", "run /usr/bin/git status"));
        // Reload from disk.
        let s2 = ApprovalStore::load(&dir);
        assert!(s2.is_remembered("coder", "run /usr/bin/git status"));
        assert!(!s2.is_remembered("coder", "run /usr/bin/git push"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn forget_removes() {
        let (s, dir) = store();
        s.remember("a", "read /x");
        assert!(s.forget("a", "read /x"));
        assert!(!s.forget("a", "read /x"));
        assert!(!s.is_remembered("a", "read /x"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn fetches_are_never_rememberable() {
        use crate::capability::{Checked, Decision, TrustState};
        let checked = Checked { decision: Decision::Allow, legs: TrustState::default(), target: Target::Url("https://x/".into()) };
        assert_eq!(rule_key(&ToolRequest::Fetch { url: "https://x/".into() }, &checked), None);
    }
}
