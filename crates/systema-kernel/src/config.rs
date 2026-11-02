use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use systema_trust::Policy;

/// `$SYSTEMA_HOME/systema.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub inference: InferenceConfig,
    pub node: NodeConfig,
    /// Directory of agent manifests (`*.toml`). Relative paths resolve against SYSTEMA_HOME.
    pub agents_dir: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct InferenceConfig {
    /// OpenAI-compatible base URL. titan-engine's serve scripts listen on 1234.
    pub endpoint: String,
    /// Model id for fast tool-calling steps (Qwen3.6-35B-A3B MoE).
    pub fast_model: String,
    /// Model id for planning / long context (Qwen3.8-27B dense, 256k).
    pub deep_model: String,
    /// Concurrent sequences the engine can run. titan-engine is tuned for 2.
    pub max_concurrent: usize,
    pub timeout_secs: u64,
    /// Sampling; unset = server default. Qwen3.x thinking models suggest 0.6 / 0.95.
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NodeConfig {
    pub listen: Vec<String>,
    /// Multiaddrs including `/p2p/<peer id>`.
    pub bootstrap: Vec<String>,
    /// Discover peers on the LAN with mDNS.
    pub mdns: bool,
    /// Seconds between agent-card announcements on gossip.
    pub announce_secs: u64,
    pub trust: Policy,
}

impl Default for Config {
    fn default() -> Self {
        Self { inference: InferenceConfig::default(), node: NodeConfig::default(), agents_dir: "agents".into() }
    }
}

impl Default for InferenceConfig {
    fn default() -> Self {
        Self {
            endpoint: "http://127.0.0.1:1234/v1".into(),
            fast_model: "default".into(),
            deep_model: "default".into(),
            max_concurrent: 2,
            timeout_secs: 600,
            temperature: Some(0.6),
            top_p: Some(0.95),
        }
    }
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            listen: vec!["/ip4/0.0.0.0/udp/0/quic-v1".into(), "/ip4/0.0.0.0/tcp/0".into()],
            bootstrap: vec![],
            mdns: true,
            announce_secs: 60,
            trust: Policy::default(),
        }
    }
}

impl Config {
    pub fn load(home: &Path) -> anyhow::Result<Self> {
        let path = home.join("systema.toml");
        let mut cfg: Config = if path.exists() { toml::from_str(&std::fs::read_to_string(&path)?)? } else { Config::default() };
        if cfg.agents_dir.is_relative() {
            cfg.agents_dir = home.join(&cfg.agents_dir);
        }
        Ok(cfg)
    }
}

/// `$SYSTEMA_HOME`, default `~/.systema`.
pub fn home_dir() -> PathBuf {
    if let Some(h) = std::env::var_os("SYSTEMA_HOME") {
        return h.into();
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| ".".into());
    home.join(".systema")
}
