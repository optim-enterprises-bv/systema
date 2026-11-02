//! Agent conversation state, kept **append-only** so titan-engine's prefix cache hits on
//! every turn. Rules:
//! - the system prompt and tool list are fixed for the life of the session;
//! - nothing earlier in the transcript is ever edited, reordered or re-summarized in place;
//! - volatile context (time, directory listings, peer state) goes in the newest message.
//!
//! When a session outgrows its budget, start a *new* session seeded with a summary rather
//! than rewriting this one.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct Session {
    system: ChatMessage,
    turns: Vec<ChatMessage>,
}

impl Session {
    pub fn new(system_prompt: impl Into<String>) -> Self {
        Self { system: ChatMessage { role: "system".into(), content: system_prompt.into() }, turns: Vec::new() }
    }

    pub fn push_user(&mut self, content: impl Into<String>) {
        self.turns.push(ChatMessage { role: "user".into(), content: content.into() });
    }

    pub fn push_assistant(&mut self, content: impl Into<String>) {
        self.turns.push(ChatMessage { role: "assistant".into(), content: content.into() });
    }

    /// Full context to send; always `[system, turns...]` with a stable prefix.
    pub fn messages(&self) -> Vec<ChatMessage> {
        std::iter::once(self.system.clone()).chain(self.turns.iter().cloned()).collect()
    }

    pub fn len(&self) -> usize {
        self.turns.len()
    }

    pub fn is_empty(&self) -> bool {
        self.turns.is_empty()
    }

    /// Rough size for budget checks (chars/4).
    pub fn approx_tokens(&self) -> usize {
        (self.system.content.len() + self.turns.iter().map(|t| t.content.len()).sum::<usize>()) / 4
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_is_stable_across_turns() {
        let mut s = Session::new("you are coder");
        s.push_user("hi");
        let before = s.messages();
        s.push_assistant("hello");
        s.push_user("next");
        let after = s.messages();
        assert_eq!(&after[..before.len()], &before[..]);
    }
}
