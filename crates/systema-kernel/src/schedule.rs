//! Scheduled and triggered jobs: the OS acting on its own. Jobs live in
//! `$SYSTEMA_HOME/schedules.toml` and the node checks them on a tick. A job fires on exactly one
//! trigger:
//!   - `every = "6h"`        — an interval (s/m/h/d);
//!   - `on_change = "~/path"`— when a file or directory's modification time changes;
//!   - `on_board = "ops"`    — when a new post appears on a board topic (empty/`*` = any topic).
//!
//! ```toml
//! [[job]]
//! name = "log-sweep"
//! agent = "operator"          # optional, defaults to operator
//! prompt = "Summarize errors in the last 6h of the journal."
//! every = "6h"
//! enabled = true              # optional, defaults to true
//! ```
//!
//! A newly seen job arms its trigger (records the current value) and fires on the *next* change,
//! not at startup. The per-job cursor is persisted so a restart neither double-fires nor re-arms.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Job {
    pub name: String,
    #[serde(default = "default_agent")]
    pub agent: String,
    pub prompt: String,
    /// Interval trigger, a number with a unit suffix s/m/h/d.
    #[serde(default)]
    pub every: Option<String>,
    /// File/dir trigger: fire when this path's mtime changes.
    #[serde(default)]
    pub on_change: Option<String>,
    /// Board trigger: fire on a new post to this topic (empty or `*`/`any` = any topic).
    #[serde(default)]
    pub on_board: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_agent() -> String {
    "operator".into()
}
fn default_true() -> bool {
    true
}

/// What makes a job fire. Exactly one per job.
#[derive(Debug, Clone)]
pub enum Trigger {
    Interval(Duration),
    OnChange(PathBuf),
    /// `None` = any topic.
    OnBoard(Option<String>),
}

impl Job {
    /// The job's trigger, or `None` if it names zero or more than one.
    pub fn trigger(&self) -> Option<Trigger> {
        match (&self.every, &self.on_change, &self.on_board) {
            (Some(e), None, None) => parse_duration(e).map(Trigger::Interval),
            (None, Some(p), None) => Some(Trigger::OnChange(expand(p))),
            (None, None, Some(t)) => {
                let topic = (!t.is_empty() && t != "*" && t != "any").then(|| t.clone());
                Some(Trigger::OnBoard(topic))
            }
            _ => None,
        }
    }

    /// One-line description of the trigger for listings.
    pub fn describe_trigger(&self) -> String {
        match self.trigger() {
            Some(Trigger::Interval(d)) => format!("every {}s", d.as_secs()),
            Some(Trigger::OnChange(p)) => format!("on change {}", p.display()),
            Some(Trigger::OnBoard(Some(t))) => format!("on board [{t}]"),
            Some(Trigger::OnBoard(None)) => "on any board post".into(),
            None => "(no valid trigger)".into(),
        }
    }
}

fn expand(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(p)
}

#[derive(Debug, Default, Deserialize)]
struct ScheduleFile {
    #[serde(default, rename = "job")]
    jobs: Vec<Job>,
}

/// Jobs defined in `schedules.toml` (missing or malformed file → no jobs).
pub fn load(home: &Path) -> Vec<Job> {
    let Ok(s) = std::fs::read_to_string(home.join("schedules.toml")) else { return Vec::new() };
    match toml::from_str::<ScheduleFile>(&s) {
        Ok(f) => f.jobs,
        Err(e) => {
            tracing::warn!("schedules.toml: {e}");
            Vec::new()
        }
    }
}

/// Parse a duration like `90s`, `30m`, `6h`, `1d`. No suffix → seconds.
pub fn parse_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (num, unit): (&str, u64) = match s.chars().last().unwrap() {
        's' => (&s[..s.len() - 1], 1),
        'm' => (&s[..s.len() - 1], 60),
        'h' => (&s[..s.len() - 1], 3600),
        'd' => (&s[..s.len() - 1], 86400),
        c if c.is_ascii_digit() => (s, 1),
        _ => return None,
    };
    let n: u64 = num.trim().parse().ok()?;
    (n > 0).then(|| Duration::from_secs(n * unit))
}

/// name → cursor. The cursor's meaning depends on the trigger: last fire time (interval), last
/// seen mtime (on_change), or last seen newest-post timestamp (on_board) — all epoch ms.
pub type State = BTreeMap<String, u64>;

pub fn load_state(home: &Path) -> State {
    std::fs::read_to_string(home.join("schedule-state.json")).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

pub fn save_state(home: &Path, state: &State) {
    if let Ok(s) = serde_json::to_vec_pretty(state) {
        let _ = std::fs::write(home.join("schedule-state.json"), s);
    }
}

/// A path's modification time in epoch ms, or 0 if it can't be read.
fn mtime_ms(p: &Path) -> u64 {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Check all jobs against their triggers, updating `state`, and return the jobs that should fire
/// now. A job seen for the first time is armed (its cursor recorded) and does not fire.
pub fn check(jobs: &[Job], state: &mut State, home: &Path, now: u64) -> Vec<Job> {
    let mut ready = Vec::new();
    for job in jobs.iter().filter(|j| j.enabled) {
        let Some(trigger) = job.trigger() else {
            tracing::warn!(job = %job.name, "scheduled job has no single valid trigger (set exactly one of every/on_change/on_board)");
            continue;
        };
        let last = state.get(&job.name).copied();
        let (fire, cursor) = match trigger {
            Trigger::Interval(d) => match last {
                None => (false, now),
                Some(c) => {
                    if now.saturating_sub(c) >= d.as_millis() as u64 {
                        (true, now)
                    } else {
                        (false, c)
                    }
                }
            },
            Trigger::OnChange(path) => observe(mtime_ms(&path), last),
            Trigger::OnBoard(topic) => observe(crate::board::latest_ts(home, topic.as_deref()), last),
        };
        state.insert(job.name.clone(), cursor);
        if fire {
            ready.push(job.clone());
        }
    }
    ready
}

/// Shared logic for value-change triggers: arm on first sight, fire when the observed value grows.
fn observe(observed: u64, last: Option<u64>) -> (bool, u64) {
    match last {
        None => (false, observed),
        Some(c) if observed > c => (true, observed),
        Some(c) => (false, c),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(name: &str, every: Option<&str>, on_change: Option<&str>, on_board: Option<&str>) -> Job {
        Job {
            name: name.into(),
            agent: "operator".into(),
            prompt: "x".into(),
            every: every.map(str::to_string),
            on_change: on_change.map(str::to_string),
            on_board: on_board.map(str::to_string),
            enabled: true,
        }
    }

    #[test]
    fn durations_parse() {
        assert_eq!(parse_duration("90s"), Some(Duration::from_secs(90)));
        assert_eq!(parse_duration("6h"), Some(Duration::from_secs(21600)));
        assert_eq!(parse_duration("1d"), Some(Duration::from_secs(86400)));
        assert_eq!(parse_duration("0h"), None);
        assert_eq!(parse_duration("abc"), None);
    }

    #[test]
    fn exactly_one_trigger_is_required() {
        assert!(job("j", Some("6h"), None, None).trigger().is_some());
        assert!(job("j", None, Some("/x"), None).trigger().is_some());
        assert!(job("j", None, None, Some("ops")).trigger().is_some());
        assert!(job("j", None, None, None).trigger().is_none()); // none
        assert!(job("j", Some("6h"), Some("/x"), None).trigger().is_none()); // two
    }

    #[test]
    fn interval_arms_then_fires() {
        let home = std::env::temp_dir();
        let j = job("j", Some("10s"), None, None);
        let mut st = State::new();
        assert!(check(std::slice::from_ref(&j), &mut st, &home, 1_000).is_empty()); // arm
        assert!(check(std::slice::from_ref(&j), &mut st, &home, 5_000).is_empty()); // too soon
        assert_eq!(check(std::slice::from_ref(&j), &mut st, &home, 12_000).len(), 1); // fire
    }

    #[test]
    fn file_change_fires_on_mtime_growth() {
        let j = job("f", None, Some("/does/not/matter"), None);
        // Simulate cursors directly via observe(): armed at 100, no fire until mtime grows.
        assert_eq!(observe(100, None), (false, 100));
        assert_eq!(observe(100, Some(100)), (false, 100));
        assert_eq!(observe(250, Some(100)), (true, 250));
        assert!(j.trigger().is_some());
    }

    #[test]
    fn board_trigger_fires_on_new_post() {
        let home = std::env::temp_dir().join(format!("systema-sched-{}", systema_proto::random_id()));
        std::fs::create_dir_all(&home).unwrap();
        let j = job("b", None, None, Some("ops"));
        let mut st = State::new();
        // Nothing posted yet: arm at 0.
        assert!(check(std::slice::from_ref(&j), &mut st, &home, 1).is_empty());
        crate::board::post(&home, "operator", false, false, "ops", "alert").unwrap();
        // A new post on the topic now fires.
        assert_eq!(check(std::slice::from_ref(&j), &mut st, &home, 2).len(), 1);
        // No new post: no fire.
        assert!(check(std::slice::from_ref(&j), &mut st, &home, 3).is_empty());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn disabled_jobs_never_fire() {
        let mut j = job("j", Some("1s"), None, None);
        j.enabled = false;
        let mut st = State::from([("j".into(), 0u64)]);
        assert!(check(std::slice::from_ref(&j), &mut st, &std::env::temp_dir(), 1_000_000).is_empty());
    }
}
