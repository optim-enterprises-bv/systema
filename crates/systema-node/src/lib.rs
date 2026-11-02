//! Swarm node. One ed25519 identity = libp2p PeerId = agent-card signing key.
//!
//! - Transports: QUIC + TCP (noise, yamux), DNS, relay client for peers behind NAT.
//! - NAT traversal: identify + relay v2 + DCUtR hole punching.
//! - Discovery: mDNS on the LAN, Kademlia DHT on the internet (provider records per skill).
//! - Announce: signed agent cards on the `systema/v1/cards` gossipsub topic.
//!
//! Every inbound card is verified (signature, freshness, signer == card.peer_id == gossip
//! source) and replay-checked before it is recorded. Recording a card grants nothing:
//! talking to a discovered agent still goes through the trust gate.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::Context as _;
use futures::StreamExt as _;
use libp2p::{
    Multiaddr, PeerId, StreamProtocol, Swarm, dcutr, gossipsub, identify, identity::Keypair, kad, mdns, noise, ping,
    relay,
    swarm::{NetworkBehaviour, SwarmEvent, behaviour::toggle::Toggle},
    tcp, yamux,
};
use systema_kernel::config::NodeConfig;
use systema_proto::{AgentCard, DEFAULT_MAX_AGE_MS, Signed, now_ms};
use systema_trust::TrustGate;
use tracing::{debug, info, warn};

pub const CARDS_TOPIC: &str = "systema/v1/cards";
pub const KAD_PROTOCOL: StreamProtocol = StreamProtocol::new("/systema/kad/1.0.0");
pub const IDENTIFY_PROTOCOL: &str = "/systema/id/1.0.0";

#[derive(NetworkBehaviour)]
pub struct Behaviour {
    relay_client: relay::client::Behaviour,
    dcutr: dcutr::Behaviour,
    identify: identify::Behaviour,
    ping: ping::Behaviour,
    mdns: Toggle<mdns::tokio::Behaviour>,
    kad: kad::Behaviour<kad::store::MemoryStore>,
    gossipsub: gossipsub::Behaviour,
}

pub struct Node {
    swarm: Swarm<Behaviour>,
    cfg: NodeConfig,
    trust: TrustGate,
    /// Our own public agents, re-signed on every announce.
    local_cards: Vec<AgentCard>,
    keypair: Keypair,
    /// Verified cards from other nodes.
    pub discovered: HashMap<PeerId, Vec<AgentCard>>,
}

impl Node {
    pub fn new(keypair: Keypair, cfg: NodeConfig) -> anyhow::Result<Self> {
        let mdns_on = cfg.mdns;
        let swarm = libp2p::SwarmBuilder::with_existing_identity(keypair.clone())
            .with_tokio()
            .with_tcp(tcp::Config::default(), noise::Config::new, yamux::Config::default)?
            .with_quic()
            .with_dns()?
            .with_relay_client(noise::Config::new, yamux::Config::default)?
            .with_behaviour(|key, relay_client| {
                let peer_id = key.public().to_peer_id();

                let gossip_cfg = gossipsub::ConfigBuilder::default()
                    .validation_mode(gossipsub::ValidationMode::Strict)
                    .max_transmit_size(64 * 1024)
                    .build()
                    .map_err(|e| anyhow::anyhow!("gossipsub config: {e}"))?;
                let gossipsub = gossipsub::Behaviour::new(gossipsub::MessageAuthenticity::Signed(key.clone()), gossip_cfg)
                    .map_err(|e| anyhow::anyhow!("gossipsub: {e}"))?;

                let mut kad_cfg = kad::Config::new(KAD_PROTOCOL);
                kad_cfg.set_query_timeout(Duration::from_secs(30));
                let kad = kad::Behaviour::with_config(peer_id, kad::store::MemoryStore::new(peer_id), kad_cfg);

                let mdns = if mdns_on { Some(mdns::tokio::Behaviour::new(mdns::Config::default(), peer_id)?) } else { None };

                Ok(Behaviour {
                    relay_client,
                    dcutr: dcutr::Behaviour::new(peer_id),
                    identify: identify::Behaviour::new(identify::Config::new(IDENTIFY_PROTOCOL.into(), key.public())),
                    ping: ping::Behaviour::default(),
                    mdns: Toggle::from(mdns),
                    kad,
                    gossipsub,
                })
            })?
            .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(120)))
            .build();

        let trust = TrustGate::new(cfg.trust.clone());
        Ok(Self { swarm, cfg, trust, local_cards: Vec::new(), keypair, discovered: HashMap::new() })
    }

    pub fn peer_id(&self) -> PeerId {
        *self.swarm.local_peer_id()
    }

    pub fn set_local_cards(&mut self, cards: Vec<AgentCard>) {
        self.local_cards = cards;
    }

    pub fn listen_addrs(&self) -> Vec<String> {
        self.swarm.listeners().map(|a| a.to_string()).collect()
    }

    /// Run until ctrl-c.
    pub async fn run(mut self) -> anyhow::Result<()> {
        for l in &self.cfg.listen {
            self.swarm.listen_on(l.parse().with_context(|| format!("bad listen addr {l}"))?)?;
        }
        for b in &self.cfg.bootstrap {
            let addr: Multiaddr = b.parse().with_context(|| format!("bad bootstrap addr {b}"))?;
            if let Some(libp2p::multiaddr::Protocol::P2p(peer)) = addr.iter().last() {
                self.swarm.behaviour_mut().kad.add_address(&peer, addr.clone());
            }
            if let Err(e) = self.swarm.dial(addr) {
                warn!("dial bootstrap {b}: {e}");
            }
        }
        if !self.cfg.bootstrap.is_empty() {
            let _ = self.swarm.behaviour_mut().kad.bootstrap();
        }

        let topic = gossipsub::IdentTopic::new(CARDS_TOPIC);
        self.swarm.behaviour_mut().gossipsub.subscribe(&topic)?;
        self.provide_skills();

        let mut announce = tokio::time::interval(Duration::from_secs(self.cfg.announce_secs.max(5)));
        info!(peer_id = %self.peer_id(), "systema node up");

        loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => { info!("shutting down"); return Ok(()); }
                _ = announce.tick() => self.announce(&topic),
                ev = self.swarm.select_next_some() => self.handle(ev),
            }
        }
    }

    fn provide_skills(&mut self) {
        let keys: Vec<_> = self.local_cards.iter().flat_map(|c| c.skills.iter().map(|s| AgentCard::skill_key(&s.id))).collect();
        for k in keys {
            if let Err(e) = self.swarm.behaviour_mut().kad.start_providing(kad::RecordKey::new(&k)) {
                warn!("start_providing: {e}");
            }
        }
    }

    fn announce(&mut self, topic: &gossipsub::IdentTopic) {
        let addrs = self.listen_addrs();
        for card in &mut self.local_cards {
            card.transports = addrs.iter().map(|a| systema_proto::Transport { kind: "libp2p".into(), address: a.clone() }).collect();
        }
        for card in self.local_cards.clone() {
            let bytes = match Signed::sign(&self.keypair, card).and_then(|s| s.to_json()) {
                Ok(b) => b,
                Err(e) => { warn!("sign card: {e}"); continue; }
            };
            match self.swarm.behaviour_mut().gossipsub.publish(topic.clone(), bytes) {
                Ok(_) | Err(gossipsub::PublishError::NoPeersSubscribedToTopic) => {}
                Err(e) => debug!("publish card: {e}"),
            }
        }
    }

    fn on_card(&mut self, source: Option<PeerId>, data: &[u8]) {
        let signed = match Signed::<AgentCard>::from_json(data) {
            Ok(s) => s,
            Err(e) => { debug!("junk on cards topic: {e}"); return; }
        };
        let signer = match signed.verify_card(DEFAULT_MAX_AGE_MS) {
            Ok(p) => p,
            Err(e) => { warn!("rejected card: {e}"); return; }
        };
        if source.is_some_and(|s| s != signer) {
            warn!(%signer, "card relayed under a different gossip source; ignoring");
            return;
        }
        if !self.trust.check_replay(&signed.nonce, now_ms()) {
            debug!(%signer, "replayed card");
            return;
        }
        let card = signed.payload;
        let skills: Vec<_> = card.skills.iter().map(|s| s.id.as_str()).collect();
        info!(peer = %signer, agent = %card.name, ?skills, "discovered agent");
        let entry = self.discovered.entry(signer).or_default();
        entry.retain(|c| c.name != card.name);
        entry.push(card);
    }

    fn handle(&mut self, ev: SwarmEvent<BehaviourEvent>) {
        match ev {
            SwarmEvent::NewListenAddr { address, .. } => info!("listening on {address}/p2p/{}", self.peer_id()),
            SwarmEvent::ConnectionEstablished { peer_id, endpoint, .. } => {
                debug!(%peer_id, addr = %endpoint.get_remote_address(), "connected")
            }
            SwarmEvent::Behaviour(BehaviourEvent::Mdns(mdns::Event::Discovered(list))) => {
                for (peer, addr) in list {
                    debug!(%peer, %addr, "mdns");
                    self.swarm.behaviour_mut().kad.add_address(&peer, addr.clone());
                    let _ = self.swarm.dial(addr);
                }
            }
            SwarmEvent::Behaviour(BehaviourEvent::Identify(identify::Event::Received { peer_id, info, .. })) => {
                if info.protocols.contains(&KAD_PROTOCOL) {
                    for a in info.listen_addrs {
                        self.swarm.behaviour_mut().kad.add_address(&peer_id, a);
                    }
                }
            }
            SwarmEvent::Behaviour(BehaviourEvent::Gossipsub(gossipsub::Event::Message { message, .. })) => {
                if message.topic.as_str() == CARDS_TOPIC {
                    self.on_card(message.source, &message.data);
                }
            }
            SwarmEvent::Behaviour(BehaviourEvent::Dcutr(e)) => info!(?e, "hole punch"),
            SwarmEvent::Behaviour(BehaviourEvent::Kad(kad::Event::RoutingUpdated { peer, .. })) => debug!(%peer, "kad routing"),
            _ => {}
        }
    }
}
