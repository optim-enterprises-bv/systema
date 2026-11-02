//! Agent conversation state, kept **append-only** so titan-engine's prefix cache hits on
//! every turn. Rules:
//! - the system prompt and tool list are fixed for the life of the session;
//! - nothing earlier in the transcript is ever edited, reordered or re-summarized in place;
//! - volatile context (time, directory listings, peer state) goes in the newest message.
//!
//! When a session outgrows its budget, start a *new* session seeded with a summary rather
//! than rewriting this one. Each session also carries its [`TrustState`]: which legs of the
//! lethal trifecta it already holds.

use serde::{Deserialize, Serialize};

use crate::capability::TrustState;

/// OpenAI chat message, including tool calls (`assistant`) and tool results (`tool`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatMessage {
    pub role: String,
    /// `null` on assistant turns that only call tools.
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type", default = "function_type")]
    pub kind: String,
    pub function: FunctionCall,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FunctionCall {
    pub name: String,
    /// JSON-encoded arguments, as the model produced them.
    pub arguments: String,
}

fn function_type() -> String {
    "function".into()
}

impl ChatMessage {
    pub fn text(role: &str, content: impl Into<String>) -> Self {
        Self { role: role.into(), content: Some(content.into()), tool_calls: None, tool_call_id: None }
    }

    pub fn tool_result(call_id: &str, content: impl Into<String>) -> Self {
        Self { role: "tool".into(), content: Some(content.into()), tool_calls: None, tool_call_id: Some(call_id.into()) }
    }
}

#[derive(Debug, Clone)]
pub struct Session {
    system: ChatMessage,
    turns: Vec<ChatMessage>,
    pub trust: TrustState,
}

impl Session {
    pub fn new(system_prompt: impl Into<String>, trust: TrustState) -> Self {
        Self { system: ChatMessage::text("system", system_prompt), turns: Vec::new(), trust }
    }

    pub fn push(&mut self, m: ChatMessage) {
        self.turns.push(m);
    }

    pub fn push_user(&mut self, content: impl Into<String>) {
        self.push(ChatMessage::text("user", content));
    }

    pub fn push_assistant(&mut self, content: impl Into<String>) {
        self.push(ChatMessage::text("assistant", content));
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
        let len = |m: &ChatMessage| {
            m.content.as_ref().map_or(0, String::len)
                + m.tool_calls.as_ref().map_or(0, |c| c.iter().map(|t| t.function.arguments.len() + 32).sum())
        };
        (len(&self.system) + self.turns.iter().map(len).sum::<usize>()) / 4
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_is_stable_across_turns() {
        let mut s = Session::new("you are coder", TrustState::operator());
        s.push_user("hi");
        let before = s.messages();
        s.push_assistant("hello");
        s.push(ChatMessage::tool_result("c1", "ok"));
        s.push_user("next");
        let after = s.messages();
        assert_eq!(&after[..before.len()], &before[..]);
    }

    #[test]
    fn serializes_like_openai() {
        let m = ChatMessage {
            role: "assistant".into(),
            content: None,
            tool_calls: Some(vec![ToolCall {
                id: "c1".into(),
                kind: "function".into(),
                function: FunctionCall { name: "read_file".into(), arguments: "{\"path\":\"x\"}".into() },
            }]),
            tool_call_id: None,
        };
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["content"], serde_json::Value::Null);
        assert_eq!(v["tool_calls"][0]["type"], "function");
        assert!(v.get("tool_call_id").is_none());
    }
}
