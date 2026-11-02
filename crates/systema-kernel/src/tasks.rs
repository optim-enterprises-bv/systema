//! The durable task board: a record of work delegated between agents. Each `delegate_task`
//! creates a task here, and its status (running → done/failed) and result are written back, so
//! the operator can see what agents handed to each other and how it turned out — even across
//! restarts. Append-only (`tasks.jsonl`); the current view takes the latest record per id.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use systema_proto::{now_ms, random_id};

pub const SCAN_LIMIT: usize = 2000;
const RESULT_CAP: usize = 2000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub creator: String,
    pub assignee: String,
    pub title: String,
    /// "running", "done" or "failed".
    pub status: String,
    pub result: String,
    pub created: u64,
    pub updated: u64,
}

fn path(home: &Path) -> PathBuf {
    home.join("tasks.jsonl")
}

fn append(home: &Path, t: &Task) -> anyhow::Result<()> {
    use std::io::Write as _;
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path(home))?;
    writeln!(f, "{}", serde_json::to_string(t)?)?;
    Ok(())
}

/// Record a new task as `running`; returns its id.
pub fn create(home: &Path, creator: &str, assignee: &str, title: &str) -> anyhow::Result<String> {
    let now = now_ms();
    let title: String = title.chars().take(400).collect();
    let t = Task {
        id: random_id(),
        creator: creator.to_string(),
        assignee: assignee.to_string(),
        title,
        status: "running".into(),
        result: String::new(),
        created: now,
        updated: now,
    };
    append(home, &t)?;
    Ok(t.id)
}

/// Write a terminal status (`done`/`failed`) and result for a task.
pub fn finish(home: &Path, id: &str, ok: bool, result: &str) {
    // Reconstruct the task so created/creator/assignee carry over.
    let mut all = latest(home);
    if let Some(t) = all.remove(id) {
        let updated = Task {
            status: if ok { "done".into() } else { "failed".into() },
            result: result.chars().take(RESULT_CAP).collect(),
            updated: now_ms(),
            ..t
        };
        let _ = append(home, &updated);
    }
}

/// Latest record per task id.
fn latest(home: &Path) -> BTreeMap<String, Task> {
    let raw = std::fs::read_to_string(path(home)).unwrap_or_default();
    let mut map: BTreeMap<String, Task> = BTreeMap::new();
    for line in raw.lines().rev().take(SCAN_LIMIT) {
        if let Ok(t) = serde_json::from_str::<Task>(line) {
            // rev() means we see the newest first; keep the first seen per id.
            map.entry(t.id.clone()).or_insert(t);
        }
    }
    map
}

/// Recent tasks, newest first.
pub fn list(home: &Path, limit: usize) -> Vec<Task> {
    let mut v: Vec<Task> = latest(home).into_values().collect();
    v.sort_by(|a, b| b.updated.cmp(&a.updated));
    v.truncate(limit);
    v
}

pub fn render(tasks: &[Task]) -> String {
    if tasks.is_empty() {
        return "(no tasks on the board)".into();
    }
    tasks
        .iter()
        .map(|t| {
            let r: String = t.result.chars().take(200).collect();
            format!("{} [{}] {} → {}: {}{}", &t.id[..t.id.len().min(8)], t.status, t.creator, t.assignee, t.title, if r.is_empty() { String::new() } else { format!("  ⇒ {r}") })
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_finish_list_round_trip() {
        let h = std::env::temp_dir().join(format!("systema-tasks-{}", random_id()));
        std::fs::create_dir_all(&h).unwrap();
        let id = create(&h, "operator", "coder", "review the parser").unwrap();
        let running = list(&h, 10);
        assert_eq!(running.len(), 1);
        assert_eq!(running[0].status, "running");
        finish(&h, &id, true, "looks good, one nit on line 10");
        let done = list(&h, 10);
        assert_eq!(done.len(), 1, "finish must not create a second task");
        assert_eq!(done[0].status, "done");
        assert!(done[0].result.contains("nit"));
        assert!(render(&done).contains("coder"));
        std::fs::remove_dir_all(&h).ok();
    }
}
