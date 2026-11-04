//! An outer policy layer the kernel consults **before** the capability broker on every tool
//! call (docs/IIi-FUSION.md Phase 3, modelled on iii's `pre-trigger` hook). A hook can only ever
//! veto a call early -- deny it before the broker, the operator-ask path, and the audit log even
//! see it -- never approve one the broker or the agent's manifest would otherwise refuse. The
//! broker remains the sole ceiling on what is *possible*; hooks only narrow what actually
//! *happens*, e.g. a wall-clock rate limit the capability model has no concept of, or an external
//! SIEM sink that wants to see (and potentially veto) a call before it runs, not just audit it
//! after. Hooks run in registration order; the first veto wins.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::capability::{ToolRequest, TrustState};

/// Consulted before `Broker::check` on every tool request. Return `Some(reason)` to veto the
/// call outright -- the model sees the same `"denied: <reason>"` shape a broker denial produces,
/// and it is audited and emitted the same way. Return `None` to let the request proceed to the
/// broker unchanged.
pub trait PolicyHook: Send + Sync {
    fn before_check(&self, agent: &str, req: &ToolRequest, trust: &TrustState) -> Option<String>;
}

/// Caps how many tool calls one agent may make per rolling minute, regardless of what its
/// manifest or the broker would otherwise allow. A backstop against a runaway loop or a
/// misbehaving/compromised agent burning GPU time -- orthogonal to capability (what an agent may
/// touch), which has no notion of call *rate*.
pub struct RateLimiter {
    max_per_minute: u32,
    hits: Mutex<HashMap<String, Vec<Instant>>>,
}

impl RateLimiter {
    /// `0` disables the limiter (every call passes through unchanged).
    pub fn new(max_per_minute: u32) -> Self {
        Self { max_per_minute, hits: Mutex::new(HashMap::new()) }
    }
}

impl PolicyHook for RateLimiter {
    fn before_check(&self, agent: &str, _req: &ToolRequest, _trust: &TrustState) -> Option<String> {
        if self.max_per_minute == 0 {
            return None;
        }
        let now = Instant::now();
        let mut hits = self.hits.lock().unwrap();
        let entry = hits.entry(agent.to_string()).or_default();
        entry.retain(|t| now.duration_since(*t) < Duration::from_secs(60));
        if entry.len() as u32 >= self.max_per_minute {
            return Some(format!("rate limit: more than {} tool calls in the last minute", self.max_per_minute));
        }
        entry.push(now);
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req() -> ToolRequest {
        ToolRequest::TaskList
    }

    #[test]
    fn zero_disables_the_limiter() {
        let rl = RateLimiter::new(0);
        for _ in 0..100 {
            assert!(rl.before_check("coder", &req(), &TrustState::operator()).is_none());
        }
    }

    #[test]
    fn caps_calls_per_minute_per_agent() {
        let rl = RateLimiter::new(3);
        assert!(rl.before_check("coder", &req(), &TrustState::operator()).is_none());
        assert!(rl.before_check("coder", &req(), &TrustState::operator()).is_none());
        assert!(rl.before_check("coder", &req(), &TrustState::operator()).is_none());
        // 4th call within the window is vetoed.
        assert!(rl.before_check("coder", &req(), &TrustState::operator()).is_some());
        // A different agent has its own budget.
        assert!(rl.before_check("sysadmin", &req(), &TrustState::operator()).is_none());
    }
}
