//! A shared knowledge base: documents and notes agents can add and search. Retrieval is
//! lexical (BM25) today — no embedding model is required — and the store is shaped so a vector
//! index can be layered on later without changing the tools.
//!
//! Like the message board, the KB is shared state: entries record their author's provenance, and
//! a search that could return a public/tainted entry taints the reader (private + untrusted) so
//! the Rule of Two still applies.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use systema_proto::{now_ms, random_id};

pub const SCAN_LIMIT: usize = 20_000;
pub const DEFAULT_K: usize = 5;
const CHUNK_CHARS: usize = 1200;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Doc {
    pub id: String,
    pub title: String,
    pub source: String,
    pub author: String,
    pub public: bool,
    pub tainted: bool,
    pub text: String,
    pub ts: u64,
}

fn path(home: &Path) -> PathBuf {
    home.join("kb.jsonl")
}

/// Split long text into chunks on paragraph boundaries, each ≲ CHUNK_CHARS.
fn chunk(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for para in text.split("\n\n") {
        if cur.chars().count() + para.chars().count() > CHUNK_CHARS && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        if para.chars().count() > CHUNK_CHARS {
            // A single huge paragraph: hard-split by chars.
            for piece in para.chars().collect::<Vec<_>>().chunks(CHUNK_CHARS) {
                out.push(piece.iter().collect());
            }
        } else {
            if !cur.is_empty() {
                cur.push_str("\n\n");
            }
            cur.push_str(para);
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    if out.is_empty() {
        out.push(text.to_string());
    }
    out
}

/// Add a document (chunked). Returns how many chunks were stored.
pub fn add(home: &Path, title: &str, source: &str, author: &str, public: bool, tainted: bool, text: &str) -> anyhow::Result<usize> {
    use std::io::Write as _;
    let title: String = title.chars().take(200).collect();
    let source: String = source.chars().take(200).collect();
    let chunks = chunk(text);
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path(home))?;
    for c in &chunks {
        let d = Doc {
            id: random_id(),
            title: title.clone(),
            source: source.clone(),
            author: author.to_string(),
            public,
            tainted,
            text: c.clone(),
            ts: now_ms(),
        };
        writeln!(f, "{}", serde_json::to_string(&d)?)?;
    }
    Ok(chunks.len())
}

fn all_docs(home: &Path) -> Vec<Doc> {
    let raw = std::fs::read_to_string(path(home)).unwrap_or_default();
    raw.lines().rev().take(SCAN_LIMIT).filter_map(|l| serde_json::from_str::<Doc>(l).ok()).collect()
}

/// Whether the KB holds any entry a reader should treat as untrusted (public/tainted author).
pub fn has_untrusted(home: &Path) -> bool {
    all_docs(home).iter().any(|d| d.public || d.tainted)
}

fn tokenize(s: &str) -> Vec<String> {
    s.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| w.len() > 1).map(str::to_string).collect()
}

/// BM25 lexical search; returns up to `k` docs, best first.
pub fn search(home: &Path, query: &str, k: usize) -> Vec<Doc> {
    let docs = all_docs(home);
    if docs.is_empty() {
        return Vec::new();
    }
    let q = tokenize(query);
    if q.is_empty() {
        return Vec::new();
    }
    let tokenized: Vec<Vec<String>> = docs.iter().map(|d| tokenize(&d.text)).collect();
    let n = docs.len() as f64;
    let avg_len = tokenized.iter().map(|t| t.len()).sum::<usize>() as f64 / n;
    // Document frequency per query term.
    let df = |term: &str| tokenized.iter().filter(|t| t.iter().any(|w| w == term)).count() as f64;
    let (k1, b) = (1.5_f64, 0.75_f64);
    let mut scored: Vec<(f64, &Doc)> = docs
        .iter()
        .zip(&tokenized)
        .map(|(doc, toks)| {
            let len = toks.len() as f64;
            let mut score = 0.0;
            for term in &q {
                let f = toks.iter().filter(|w| *w == term).count() as f64;
                if f == 0.0 {
                    continue;
                }
                let idf = (1.0 + (n - df(term) + 0.5) / (df(term) + 0.5)).ln();
                score += idf * (f * (k1 + 1.0)) / (f + k1 * (1.0 - b + b * len / avg_len.max(1.0)));
            }
            (score, doc)
        })
        .filter(|(s, _)| *s > 0.0)
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored.into_iter().take(k).map(|(_, d)| d.clone()).collect()
}

pub fn render(docs: &[Doc]) -> String {
    if docs.is_empty() {
        return "(no matching knowledge)".into();
    }
    docs.iter()
        .map(|d| {
            let flag = if d.public || d.tainted { " [untrusted source]" } else { "" };
            let src = if d.source.is_empty() { String::new() } else { format!(" ({})", d.source) };
            format!("## {}{}{}\n{}", d.title, src, flag, d.text)
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("systema-kb-{}", random_id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn search_ranks_relevant_docs_first() {
        let h = tmp();
        add(&h, "Rust errors", "", "operator", false, false, "Use the question mark operator to propagate errors in Rust.").unwrap();
        add(&h, "Disk cleanup", "", "operator", false, false, "Run df and du to find large files and free disk space.").unwrap();
        let hits = search(&h, "how do I free disk space", DEFAULT_K);
        assert!(!hits.is_empty());
        assert_eq!(hits[0].title, "Disk cleanup");
        assert!(search(&h, "propagate errors", DEFAULT_K)[0].title == "Rust errors");
        std::fs::remove_dir_all(&h).ok();
    }

    #[test]
    fn chunks_long_documents_and_tracks_taint() {
        let h = tmp();
        let long = "para ".repeat(1000); // well over CHUNK_CHARS
        let n = add(&h, "big", "", "coder", true, false, &long).unwrap();
        assert!(n > 1, "long text should be chunked");
        assert!(has_untrusted(&h), "a public author's entry taints the KB");
        std::fs::remove_dir_all(&h).ok();
    }
}
