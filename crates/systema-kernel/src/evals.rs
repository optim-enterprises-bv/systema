//! A small eval harness: run a set of prompts against agents and check their answers, so agent
//! changes (prompts, capabilities, `propose` suggestions) can be regression-tested. Evals live in
//! `$SYSTEMA_HOME/evals.toml`:
//!
//! ```toml
//! [[eval]]
//! name = "disk-free"
//! # agent = "operator"        # optional; omit to route like `systema do`
//! prompt = "How much disk space is free on the main filesystem? Answer in one line."
//! expect = ["free"]           # all of these must appear (case-insensitive)
//! expect_absent = ["error"]   # none of these may appear
//! ```

use std::path::Path;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Eval {
    pub name: String,
    /// Agent to run against; omit to route the prompt like `systema do`.
    #[serde(default)]
    pub agent: Option<String>,
    pub prompt: String,
    /// Substrings the answer must contain (case-insensitive).
    #[serde(default)]
    pub expect: Vec<String>,
    /// Substrings the answer must NOT contain (case-insensitive).
    #[serde(default)]
    pub expect_absent: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
struct EvalFile {
    #[serde(default, rename = "eval")]
    evals: Vec<Eval>,
}

/// Evals defined in `evals.toml` (missing or malformed file → none).
pub fn load(home: &Path) -> Vec<Eval> {
    let Ok(s) = std::fs::read_to_string(home.join("evals.toml")) else { return Vec::new() };
    match toml::from_str::<EvalFile>(&s) {
        Ok(f) => f.evals,
        Err(e) => {
            tracing::warn!("evals.toml: {e}");
            Vec::new()
        }
    }
}

#[derive(Debug, Clone)]
pub struct Outcome {
    pub name: String,
    pub agent: String,
    pub pass: bool,
    /// Expected substrings that were missing.
    pub missing: Vec<String>,
    /// Forbidden substrings that were present.
    pub forbidden: Vec<String>,
    pub answer: String,
}

/// Check an answer against an eval's expectations.
pub fn check(eval: &Eval, agent: &str, answer: &str) -> Outcome {
    let hay = answer.to_lowercase();
    let missing: Vec<String> = eval.expect.iter().filter(|s| !hay.contains(&s.to_lowercase())).cloned().collect();
    let forbidden: Vec<String> = eval.expect_absent.iter().filter(|s| hay.contains(&s.to_lowercase())).cloned().collect();
    Outcome {
        name: eval.name.clone(),
        agent: agent.to_string(),
        pass: missing.is_empty() && forbidden.is_empty(),
        missing,
        forbidden,
        answer: answer.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval() -> Eval {
        Eval {
            name: "t".into(),
            agent: None,
            prompt: "p".into(),
            expect: vec!["free".into(), "GB".into()],
            expect_absent: vec!["error".into()],
        }
    }

    #[test]
    fn passes_when_all_expected_present_and_none_forbidden() {
        let o = check(&eval(), "operator", "There are 281 GB free on the disk.");
        assert!(o.pass, "{o:?}");
    }

    #[test]
    fn fails_and_reports_missing_and_forbidden() {
        let o = check(&eval(), "operator", "error: could not read free space");
        assert!(!o.pass);
        assert_eq!(o.missing, vec!["GB".to_string()]); // "free" is present, "GB" missing
        assert_eq!(o.forbidden, vec!["error".to_string()]);
    }
}
