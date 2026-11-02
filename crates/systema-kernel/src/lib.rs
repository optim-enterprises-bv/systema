//! The Systema kernel: runs agents as managed processes over one shared GPU.

pub mod bus;
pub mod config;
pub mod inference;
pub mod manifest;
pub mod scheduler;
pub mod session;

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Context as _;
use systema_proto::{AgentCard, PROTOCOL_VERSION, Transport};
use tokio::sync::Mutex;

pub use config::Config;
pub use manifest::AgentManifest;

use bus::Bus;
use inference::InferenceClient;
use scheduler::Scheduler;
use session::Session;

pub struct Kernel {
    pub config: Config,
    pub bus: Bus,
    pub scheduler: Scheduler,
    pub inference: InferenceClient,
    agents: HashMap<String, AgentManifest>,
    sessions: Mutex<HashMap<String, Arc<Mutex<Session>>>>,
}

impl Kernel {
    pub fn new(config: Config) -> anyhow::Result<Self> {
        let agents = manifest::load_dir(&config.agents_dir)?.into_iter().map(|m| (m.name.clone(), m)).collect();
        Ok(Self {
            scheduler: Scheduler::new(config.inference.max_concurrent),
            inference: InferenceClient::new(config.inference.clone())?,
            bus: Bus::default(),
            agents,
            sessions: Mutex::new(HashMap::new()),
            config,
        })
    }

    pub fn agents(&self) -> impl Iterator<Item = &AgentManifest> {
        let mut v: Vec<_> = self.agents.values().collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v.into_iter()
    }

    pub fn agent(&self, name: &str) -> Option<&AgentManifest> {
        self.agents.get(name)
    }

    /// One turn with an agent: append to its session, queue for the GPU, append the reply.
    pub async fn ask(&self, agent: &str, prompt: &str) -> anyhow::Result<String> {
        let m = self.agent(agent).with_context(|| format!("no agent named {agent}"))?;
        let session = {
            let mut s = self.sessions.lock().await;
            s.entry(agent.to_string()).or_insert_with(|| Arc::new(Mutex::new(Session::new(&m.system_prompt)))).clone()
        };
        let mut session = session.lock().await;
        session.push_user(prompt);
        let reply = {
            let _slot = self.scheduler.acquire(m.priority).await;
            self.inference.chat(m.model, &session.messages()).await?
        };
        session.push_assistant(&reply);
        Ok(reply)
    }

    /// Agent cards for every agent marked `public`, hosted on `peer_id`.
    pub fn public_cards(&self, peer_id: &str, addrs: &[String]) -> Vec<AgentCard> {
        self.agents()
            .filter(|m| m.capabilities.public)
            .map(|m| AgentCard {
                name: m.name.clone(),
                description: m.description.clone(),
                version: env!("CARGO_PKG_VERSION").into(),
                protocol: PROTOCOL_VERSION.into(),
                peer_id: peer_id.to_string(),
                transports: addrs.iter().map(|a| Transport { kind: "libp2p".into(), address: a.clone() }).collect(),
                skills: m.skills.clone(),
            })
            .collect()
    }
}
