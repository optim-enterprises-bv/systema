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
    /// MCP servers the operator allows agents to use (`[mcp.<name>]`).
    pub mcp: std::collections::BTreeMap<String, crate::mcp::McpServerConfig>,
    /// SYSTEMA_HOME this config was loaded from (not stored in the file).
    #[serde(skip)]
    pub home: PathBuf,
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
    /// Max context window (tokens) this node advertises to the swarm, so peers can route
    /// long-context work here. Unset = not advertised.
    #[serde(default)]
    pub max_context: Option<u32>,
    /// Free-text hardware descriptor advertised to the swarm, e.g. "RTX 4090 24GB". Operator-set.
    #[serde(default)]
    pub hardware: Option<String>,
    /// OpenAI-compatible `/embeddings` endpoint for the knowledge base. Unset = BM25 only.
    /// Point this at a small embedding service (e.g. one running on the NPU), separate from the
    /// generation endpoint so the two never contend for the GPU.
    #[serde(default)]
    pub embed_endpoint: Option<String>,
    /// Model id to request from `embed_endpoint`.
    #[serde(default = "default_embed_model")]
    pub embed_model: String,
}

fn default_embed_model() -> String {
    "default".into()
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
    /// Announce cards on gossip at all (DHT lookup + direct card fetch work without it).
    pub gossip_cards: bool,
    /// Serve the DHT for others. `auto` = only once an external address is confirmed;
    /// `on` for public or LAN nodes (bootstrap nodes must be `on`); `off` for client-only.
    pub dht_server: DhtMode,
    /// Relays to reserve a slot on (multiaddrs ending in `/p2p/<id>`), for nodes behind NAT:
    /// the node becomes reachable at `<relay>/p2p-circuit/p2p/<self>`.
    pub relays: Vec<String>,
    /// Addresses this node is known to be reachable at (e.g. a VPS's public IP); advertised as-is.
    pub external_addrs: Vec<String>,
    /// Act as a circuit relay for other nodes (public nodes only).
    pub relay_server: bool,
    pub relay_limits: RelayLimits,
    pub trust: Policy,
}

/// What a relay server donates. libp2p's defaults (2 min, 128 KiB) only cover hole-punch
/// coordination; agent requests can wait minutes for a shared GPU.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RelayLimits {
    pub max_reservations: usize,
    pub max_circuits: usize,
    pub max_circuits_per_peer: usize,
    pub max_circuit_secs: u64,
    pub max_circuit_bytes: u64,
}

impl Default for RelayLimits {
    fn default() -> Self {
        Self { max_reservations: 128, max_circuits: 32, max_circuits_per_peer: 4, max_circuit_secs: 900, max_circuit_bytes: 4 << 20 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DhtMode {
    #[default]
    Auto,
    On,
    Off,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            inference: InferenceConfig::default(),
            node: NodeConfig::default(),
            agents_dir: "agents".into(),
            mcp: Default::default(),
            home: home_dir(),
        }
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
            max_context: None,
            hardware: None,
            embed_endpoint: None,
            embed_model: default_embed_model(),
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
            gossip_cards: true,
            dht_server: DhtMode::Auto,
            relays: vec![],
            external_addrs: vec![],
            relay_server: false,
            relay_limits: RelayLimits::default(),
            trust: Policy::default(),
        }
    }
}

impl Config {
    pub fn load(home: &Path) -> anyhow::Result<Self> {
        let path = home.join("systema.toml");
        let mut cfg: Config = if path.exists() { toml::from_str(&std::fs::read_to_string(&path)?)? } else { Config::default() };
        cfg.home = home.to_path_buf();
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
