//! Scheduled jobs: the OS acting on its own. Jobs are defined in `$SYSTEMA_HOME/schedules.toml`
//! and run by the node on an interval, so an agent turn fires without anyone prompting it.
//!
//! ```toml
//! [[job]]
//! name = "log-sweep"
//! agent = "operator"          # optional, defaults to operator
//! prompt = "Summarize any errors in the last 6h of the system journal."
//! every = "6h"                # 90s, 30m, 6h, 1d …
//! enabled = true              # optional, defaults to true
//! ```
//!
//! A newly added job records its first-seen time and fires one interval later (not at startup),
//! and the last-run time is persisted so a restart neither double-fires nor re-arms the clock.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Job {
    pub name: String,
    #[serde(default = "default_agent")]
    pub agent: String,
    pub prompt: String,
    /// Interval between runs: a number with a unit suffix s/m/h/d.
    pub every: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_agent() -> String {
    "operator".into()
}
fn default_true() -> bool {
    true
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

/// name → last-run epoch ms.
pub type State = BTreeMap<String, u64>;

pub fn load_state(home: &Path) -> State {
    std::fs::read_to_string(home.join("schedule-state.json")).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

pub fn save_state(home: &Path, state: &State) {
    if let Ok(s) = serde_json::to_vec_pretty(state) {
        let _ = std::fs::write(home.join("schedule-state.json"), s);
    }
}

/// Whether `job` is due given its last run (`None` = never seen before).
pub fn due(job: &Job, last_run: Option<u64>, now: u64) -> bool {
    let Some(d) = parse_duration(&job.every) else { return false };
    match last_run {
        // First sight: arm the clock, don't fire at startup.
        None => false,
        Some(last) => now.saturating_sub(last) >= d.as_millis() as u64,
    }
}

/// Convenience for the node's tick: the names of jobs that should fire now, updating `state`
/// (first-seen jobs are armed to `now`; returned jobs are marked run at `now`). Caller fires them.
pub fn fire_due(jobs: &[Job], state: &mut State, now: u64) -> Vec<Job> {
    let mut ready = Vec::new();
    for job in jobs.iter().filter(|j| j.enabled) {
        let last = state.get(&job.name).copied();
        if last.is_none() {
            state.insert(job.name.clone(), now); // arm, fire next interval
        } else if due(job, last, now) {
            state.insert(job.name.clone(), now);
            ready.push(job.clone());
        }
    }
    ready
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_parse() {
        assert_eq!(parse_duration("90s"), Some(Duration::from_secs(90)));
        assert_eq!(parse_duration("30m"), Some(Duration::from_secs(1800)));
        assert_eq!(parse_duration("6h"), Some(Duration::from_secs(21600)));
        assert_eq!(parse_duration("1d"), Some(Duration::from_secs(86400)));
        assert_eq!(parse_duration("45"), Some(Duration::from_secs(45)));
        assert_eq!(parse_duration("0h"), None);
        assert_eq!(parse_duration("abc"), None);
    }

    #[test]
    fn first_seen_arms_then_fires_after_interval() {
        let job = Job { name: "j".into(), agent: "operator".into(), prompt: "x".into(), every: "10s".into(), enabled: true };
        let mut st = State::new();
        // First tick arms, does not fire.
        assert!(fire_due(std::slice::from_ref(&job), &mut st, 1_000).is_empty());
        assert_eq!(st.get("j"), Some(&1_000));
        // Before the interval: still nothing.
        assert!(fire_due(std::slice::from_ref(&job), &mut st, 5_000).is_empty());
        // After the interval: fires and re-arms.
        let fired = fire_due(std::slice::from_ref(&job), &mut st, 12_000);
        assert_eq!(fired.len(), 1);
        assert_eq!(st.get("j"), Some(&12_000));
    }

    #[test]
    fn disabled_jobs_never_fire() {
        let job = Job { name: "j".into(), agent: "operator".into(), prompt: "x".into(), every: "1s".into(), enabled: false };
        let mut st = State::from([("j".into(), 0u64)]);
        assert!(fire_due(std::slice::from_ref(&job), &mut st, 1_000_000).is_empty());
    }
}
