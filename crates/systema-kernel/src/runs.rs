//! Run history: a structured record of every turn (prompt, each tool call + outcome, final
//! answer, timing, trust) appended to `$SYSTEMA_HOME/runs.jsonl`. This is the observability /
//! replay layer — `systema history` lists recent runs and `systema replay <id>` shows one turn's
//! full trace. It complements the raw `audit.jsonl` (one line per tool call) with a per-turn view.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use systema_proto::{now_ms, random_id};

use crate::capability::TrustState;

pub const SCAN_LIMIT: usize = 5000;
const ANSWER_CAP: usize = 4000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    pub tool: String,
    pub outcome: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    pub id: String,
    pub ts: u64,
    pub agent: String,
    pub session: String,
    pub prompt: String,
    pub steps: Vec<Step>,
    pub ok: bool,
    pub answer: String,
    pub ms: u64,
    pub trust: TrustState,
}

fn path(home: &Path) -> PathBuf {
    home.join("runs.jsonl")
}

/// Record a completed turn. Builds the id/timestamp; the caller supplies the rest.
#[allow(clippy::too_many_arguments)]
pub fn record(home: &Path, agent: &str, session: &str, prompt: &str, steps: Vec<Step>, ok: bool, answer: &str, ms: u64, trust: TrustState) {
    use std::io::Write as _;
    let run = Run {
        id: random_id(),
        ts: now_ms(),
        agent: agent.to_string(),
        session: session.to_string(),
        prompt: prompt.chars().take(1000).collect(),
        steps,
        ok,
        answer: answer.chars().take(ANSWER_CAP).collect(),
        ms,
        trust,
    };
    let res = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path(home))
        .and_then(|mut f| writeln!(f, "{}", serde_json::to_string(&run).unwrap_or_default()));
    if let Err(e) = res {
        tracing::warn!("runs log: {e}");
    }
}

fn all(home: &Path) -> Vec<Run> {
    let raw = std::fs::read_to_string(path(home)).unwrap_or_default();
    raw.lines().rev().take(SCAN_LIMIT).filter_map(|l| serde_json::from_str::<Run>(l).ok()).collect()
}

/// Recent runs, newest first, optionally filtered to one agent.
pub fn list(home: &Path, agent: Option<&str>, limit: usize) -> Vec<Run> {
    all(home).into_iter().filter(|r| agent.is_none_or(|a| r.agent == a)).take(limit).collect()
}

/// One run by id prefix (the short id `history` prints).
pub fn get(home: &Path, id_prefix: &str) -> Option<Run> {
    all(home).into_iter().find(|r| r.id.starts_with(id_prefix))
}

fn short(id: &str) -> &str {
    &id[..id.len().min(8)]
}

/// One line per run for `history`.
pub fn render_list(runs: &[Run]) -> String {
    if runs.is_empty() {
        return "(no runs recorded yet)".into();
    }
    runs.iter()
        .map(|r| {
            let status = if r.ok { "ok " } else { "ERR" };
            let prompt: String = r.prompt.replace('\n', " ").chars().take(60).collect();
            format!("{}  {status}  {:<10} {:>5}ms  {} tools  {}", short(&r.id), r.agent, r.ms, r.steps.len(), prompt)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The full trace of one run for `replay`.
pub fn render_full(r: &Run) -> String {
    let mut out = format!(
        "run {}  [{}]  session={}  {}  {}ms\ntrust: untrusted={} private={} outbound={}\n\n> {}\n",
        short(&r.id),
        r.agent,
        r.session,
        if r.ok { "ok" } else { "error" },
        r.ms,
        r.trust.untrusted,
        r.trust.private,
        r.trust.outbound,
        r.prompt.trim(),
    );
    for (i, s) in r.steps.iter().enumerate() {
        out.push_str(&format!("\n  {}. {}\n     → {}", i + 1, s.tool, s.outcome));
    }
    out.push_str(&format!("\n\n{}\n", r.answer.trim()));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_reads_back_a_run() {
        let h = std::env::temp_dir().join(format!("systema-runs-{}", random_id()));
        std::fs::create_dir_all(&h).unwrap();
        record(&h, "operator", "operator", "free disk?", vec![Step { tool: "shell df".into(), outcome: "allowed".into() }], true, "281G free", 1200, TrustState::operator());
        let runs = list(&h, None, 10);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].agent, "operator");
        let id = runs[0].id.clone();
        let one = get(&h, &id[..8]).expect("by prefix");
        assert!(render_full(&one).contains("shell df"));
        assert!(list(&h, Some("coder"), 10).is_empty());
        std::fs::remove_dir_all(&h).ok();
    }
}
