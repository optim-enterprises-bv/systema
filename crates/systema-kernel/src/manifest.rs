use std::path::Path;

use serde::{Deserialize, Serialize};
use systema_proto::Skill;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ModelTier {
    /// MoE model: fast tool-calling steps.
    #[default]
    Fast,
    /// Dense model: planning, long documents, hard reasoning.
    Deep,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    Background = 0,
    #[default]
    Normal = 1,
    Interactive = 2,
}

/// What an agent process is allowed to touch. Enforced by the sandbox, not the prompt.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Capabilities {
    pub fs_read: Vec<String>,
    pub fs_write: Vec<String>,
    pub network: bool,
    pub shell: bool,
    /// MCP servers this agent may call.
    pub mcp: Vec<String>,
    /// Whether this agent's skills are advertised to the swarm.
    pub public: bool,
}

/// `agents/<name>.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentManifest {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub model: ModelTier,
    #[serde(default)]
    pub priority: Priority,
    pub system_prompt: String,
    #[serde(default)]
    pub skills: Vec<Skill>,
    #[serde(default)]
    pub capabilities: Capabilities,
}

pub fn load_dir(dir: &Path) -> anyhow::Result<Vec<AgentManifest>> {
    let mut out = Vec::new();
    if !dir.exists() {
        return Ok(out);
    }
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.filter_map(Result::ok).map(|e| e.path()).collect();
    entries.sort();
    for path in entries.into_iter().filter(|p| p.extension().is_some_and(|e| e == "toml")) {
        let m: AgentManifest = toml::from_str(&std::fs::read_to_string(&path)?)
            .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
        out.push(m);
    }
    Ok(out)
}
