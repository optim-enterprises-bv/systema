//! The shared message board: a single append-only log that every board-capable agent can
//! read and post to. Unlike per-agent `memory` (private, in the agent's workspace), the board
//! is **shared state** — one agent's post is another's input.
//!
//! Security: because a swarm-facing (`public`) agent, or any agent in an untrusted session, can
//! post, board content is treated as **untrusted input**. A read taints the reading session's
//! `untrusted` leg whenever any returned post came from a public agent or a tainted session, so
//! the Rule of Two still guards against a post injecting an exfiltration. An operator-only board
//! (no public authors, no tainted posts) reads clean.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use systema_proto::now_ms;

/// Most recent posts returned by a read, and the cap on the board file we scan.
pub const READ_LIMIT: usize = 50;
pub const SCAN_LIMIT: usize = 2000;
const TEXT_CAP: usize = 4096;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Post {
    pub ts: u64,
    pub author: String,
    /// The author agent is swarm-facing (its posts are untrusted to readers).
    pub public: bool,
    /// The authoring session already held untrusted content.
    pub tainted: bool,
    pub topic: String,
    pub text: String,
}

pub fn path(home: &Path) -> PathBuf {
    home.join("board.jsonl")
}

/// Append a post. `text` and `topic` are capped; newlines in the topic are stripped.
pub fn post(home: &Path, author: &str, public: bool, tainted: bool, topic: &str, text: &str) -> anyhow::Result<String> {
    use std::io::Write as _;
    let topic = topic.replace(['\n', '\r'], " ");
    let topic: String = topic.trim().chars().take(120).collect();
    let topic = if topic.is_empty() { "general".to_string() } else { topic };
    let text: String = text.chars().take(TEXT_CAP).collect();
    let p = Post { ts: now_ms(), author: author.to_string(), public, tainted, topic: topic.clone(), text };
    let line = serde_json::to_string(&p)?;
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path(home))?;
    writeln!(f, "{line}")?;
    Ok(format!("posted to board topic `{topic}`"))
}

/// The most recent posts (optionally filtered to one topic), oldest-first for reading.
pub fn read(home: &Path, topic: Option<&str>, limit: usize) -> Vec<Post> {
    let raw = std::fs::read_to_string(path(home)).unwrap_or_default();
    let mut posts: Vec<Post> = raw
        .lines()
        .rev()
        .take(SCAN_LIMIT)
        .filter_map(|l| serde_json::from_str::<Post>(l).ok())
        .filter(|p| topic.is_none_or(|t| p.topic.eq_ignore_ascii_case(t)))
        .take(limit)
        .collect();
    posts.reverse(); // oldest-first for display
    posts
}

/// Whether the given posts would taint a reader (any public author or tainted session).
pub fn is_tainted(posts: &[Post]) -> bool {
    posts.iter().any(|p| p.public || p.tainted)
}

/// Timestamp of the most recent post (optionally on one topic), or 0 if none. Used by the
/// scheduler to fire a job when a new message appears on a watched topic.
pub fn latest_ts(home: &Path, topic: Option<&str>) -> u64 {
    read(home, topic, 1).first().map(|p| p.ts).unwrap_or(0)
}

/// Human-readable rendering of posts for the model.
pub fn render(posts: &[Post]) -> String {
    if posts.is_empty() {
        return "(the board is empty)".into();
    }
    posts
        .iter()
        .map(|p| {
            let flag = if p.public || p.tainted { " [untrusted author]" } else { "" };
            format!("[{}] {}{}: {}", p.topic, p.author, flag, p.text)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("systema-board-{}", systema_proto::random_id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn posts_round_trip_and_filter_by_topic() {
        let h = tmp();
        post(&h, "operator", false, false, "status", "disk cleaned").unwrap();
        post(&h, "coder", false, false, "build", "tests green").unwrap();
        let all = read(&h, None, READ_LIMIT);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].author, "operator"); // oldest-first
        let build = read(&h, Some("build"), READ_LIMIT);
        assert_eq!(build.len(), 1);
        assert_eq!(build[0].text, "tests green");
        std::fs::remove_dir_all(&h).ok();
    }

    #[test]
    fn taint_tracks_public_or_tainted_authors() {
        let h = tmp();
        post(&h, "operator", false, false, "general", "hello").unwrap();
        assert!(!is_tainted(&read(&h, None, READ_LIMIT)));
        post(&h, "coder", true, false, "general", "from the swarm").unwrap();
        assert!(is_tainted(&read(&h, None, READ_LIMIT)));
        assert!(render(&read(&h, None, READ_LIMIT)).contains("[untrusted author]"));
        std::fs::remove_dir_all(&h).ok();
    }
}
