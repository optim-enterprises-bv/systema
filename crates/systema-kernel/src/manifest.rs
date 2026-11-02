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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum GrantMode {
    /// Run without asking (still subject to the Rule of Two).
    Allow,
    /// Ask the operator every time; denied when no operator is present.
    #[default]
    Ask,
}

/// Permission to run one program, optionally only with a given leading argument list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecGrant {
    /// Program name, resolved from the system PATH only (never a path).
    pub program: String,
    /// The call's arguments must start with these, e.g. `["status"]` for `systemctl status …`.
    #[serde(default)]
    pub args_prefix: Vec<String>,
    #[serde(default)]
    pub mode: GrantMode,
}

/// The ceiling on what an agent may ever ask the capability broker for. Paths may use `~`.
/// The agent's own workspace (`$SYSTEMA_HOME/workspaces/<name>`) is always available.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Capabilities {
    /// Trees the agent may read and list (private data).
    pub fs_read: Vec<String>,
    /// Trees the agent may write (implies read).
    pub fs_write: Vec<String>,
    /// May fetch public http(s) URLs.
    pub network: bool,
    /// Any program not covered by `exec`, always with operator approval.
    pub shell: bool,
    /// Programs the agent may run.
    pub exec: Vec<ExecGrant>,
    /// MCP servers this agent may call.
    pub mcp: Vec<String>,
    /// May read and post to the shared message board (`board_read`/`board_post`).
    pub board: bool,
    /// May hand sub-tasks to other agents (`delegate_task`) and read the task board.
    pub delegate: bool,
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
