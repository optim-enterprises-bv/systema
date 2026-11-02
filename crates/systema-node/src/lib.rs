//! Swarm node. One ed25519 identity = libp2p PeerId = agent-card signing key.
//!
//! - Transports: QUIC + TCP (noise, yamux), DNS, relay client for peers behind NAT.
//! - NAT traversal: identify + relay v2 + DCUtR hole punching.
//! - Discovery: mDNS on the LAN, Kademlia DHT on the internet (provider records per skill).
//! - Announce: signed agent cards on the `systema/v1/cards` gossipsub topic.
//! - Direct requests: `/systema/a2a/1.0.0` request-response carrying `Signed<Message>`.
//!
//! Inbound request pipeline, all before any model sees the text:
//! signature → signer == connected peer → replay → target agent is public and offers the
//! skill → size → trust gate (allow / quarantine / approvals inbox / deny).
//! Every decision is appended to `$SYSTEMA_HOME/audit.jsonl`.

pub mod control;

use std::collections::HashMap;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use futures::StreamExt as _;
use libp2p::{
    Multiaddr, PeerId, StreamProtocol, Swarm, dcutr, gossipsub, identify, identity::Keypair, kad, mdns, noise, ping,
    relay,
    request_response::{self, OutboundRequestId, ProtocolSupport, ResponseChannel},
    swarm::{NetworkBehaviour, SwarmEvent, behaviour::toggle::Toggle},
    tcp, yamux,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use systema_kernel::{Kernel, config::NodeConfig};
use systema_proto::{
    A2A_PROTOCOL, A2aResponse, AgentCard, DEFAULT_MAX_AGE_MS, MAX_REQUEST_TEXT, Message, Part, Role, Signed, now_ms,
    random_id,
};
use systema_trust::{Decision, Grant, Origin, Policy, TrustGate};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

use control::{Control, ControlReply};

pub const CARDS_TOPIC: &str = "systema/v1/cards";
pub const KAD_PROTOCOL: StreamProtocol = StreamProtocol::new("/systema/kad/1.0.0");
pub const IDENTIFY_PROTOCOL: &str = "/systema/id/1.0.0";
/// Remote agents run on a shared GPU at background priority; give them time.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(600);

type A2a = request_response::json::Behaviour<Signed<Message>, A2aResponse>;

#[derive(NetworkBehaviour)]
pub struct Behaviour {
    relay_client: relay::client::Behaviour,
    dcutr: dcutr::Behaviour,
    identify: identify::Behaviour,
    ping: ping::Behaviour,
    mdns: Toggle<mdns::tokio::Behaviour>,
    kad: kad::Behaviour<kad::store::MemoryStore>,
    gossipsub: gossipsub::Behaviour,
    a2a: A2a,
}

/// A request from a peer with no grant, held for the operator.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxItem {
    pub id: String,
    pub peer: String,
    pub agent: String,
    pub skill: String,
    pub text: String,
    pub received_ms: u64,
}

type Finished = (ResponseChannel<A2aResponse>, A2aResponse);

pub struct Node {
    swarm: Swarm<Behaviour>,
    cfg: NodeConfig,
    trust: TrustGate,
    kernel: Arc<Kernel>,
    home: PathBuf,
    /// Our own public agents, re-signed on every announce.
    local_cards: Vec<AgentCard>,
    keypair: Keypair,
    /// Verified cards from other nodes.
    pub discovered: HashMap<PeerId, Vec<AgentCard>>,
    inbox: Vec<InboxItem>,
    pending_out: HashMap<OutboundRequestId, (PeerId, oneshot::Sender<ControlReply>)>,
    done_tx: mpsc::Sender<Finished>,
    done_rx: Option<mpsc::Receiver<Finished>>,
}

impl Node {
    pub fn new(keypair: Keypair, cfg: NodeConfig, kernel: Arc<Kernel>, home: PathBuf) -> anyhow::Result<Self> {
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

                let a2a = A2a::new(
                    [(StreamProtocol::new(A2A_PROTOCOL), ProtocolSupport::Full)],
                    request_response::Config::default().with_request_timeout(REQUEST_TIMEOUT),
                );

                Ok(Behaviour {
                    relay_client,
                    dcutr: dcutr::Behaviour::new(peer_id),
                    identify: identify::Behaviour::new(identify::Config::new(IDENTIFY_PROTOCOL.into(), key.public())),
                    ping: ping::Behaviour::default(),
                    mdns: Toggle::from(mdns),
                    kad,
                    gossipsub,
                    a2a,
                })
            })?
            .with_swarm_config(|c| c.with_idle_connection_timeout(REQUEST_TIMEOUT))
            .build();

        // Persisted trust state (grants made via `approve`) wins over the config file's.
        let policy: Policy = read_json(&home.join("trust.json"))?.unwrap_or_else(|| cfg.trust.clone());
        let inbox: Vec<InboxItem> = read_json(&home.join("inbox.json"))?.unwrap_or_default();
        let (done_tx, done_rx) = mpsc::channel(64);
        Ok(Self {
            swarm,
            cfg,
            trust: TrustGate::new(policy),
            kernel,
            home,
            local_cards: Vec::new(),
            keypair,
            discovered: HashMap::new(),
            inbox,
            pending_out: HashMap::new(),
            done_tx,
            done_rx: Some(done_rx),
        })
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

        let (ctl_tx, mut ctl_rx) = mpsc::channel(16);
        control::serve(&self.home, ctl_tx)?;
        let mut done_rx = self.done_rx.take().expect("run called twice");

        let mut announce = tokio::time::interval(Duration::from_secs(self.cfg.announce_secs.max(5)));
        info!(peer_id = %self.peer_id(), "systema node up");

        loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    info!("shutting down");
                    let _ = std::fs::remove_file(control::socket_path(&self.home));
                    return Ok(());
                }
                _ = announce.tick() => self.announce(&topic),
                Some((channel, response)) = done_rx.recv() => {
                    if self.swarm.behaviour_mut().a2a.send_response(channel, response).is_err() {
                        debug!("requester went away before the reply was ready");
                    }
                }
                Some((cmd, reply)) = ctl_rx.recv() => self.on_control(cmd, reply),
                ev = self.swarm.select_next_some() => self.handle(ev),
            }
        }
    }

    // ---------------------------------------------------------------------------------
    // Discovery
    // ---------------------------------------------------------------------------------

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
                Err(e) => {
                    warn!("sign card: {e}");
                    continue;
                }
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
            Err(e) => {
                debug!("junk on cards topic: {e}");
                return;
            }
        };
        let signer = match signed.verify_card(DEFAULT_MAX_AGE_MS) {
            Ok(p) => p,
            Err(e) => {
                warn!("rejected card: {e}");
                return;
            }
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
        let entry = self.discovered.entry(signer).or_default();
        let is_new = !entry.iter().any(|c| c.name == card.name);
        if is_new {
            let skills: Vec<_> = card.skills.iter().map(|s| s.id.as_str()).collect();
            info!(peer = %signer, agent = %card.name, ?skills, "discovered agent");
        }
        entry.retain(|c| c.name != card.name);
        entry.push(card);
    }

    // ---------------------------------------------------------------------------------
    // Direct requests: inbound
    // ---------------------------------------------------------------------------------

    fn respond(&mut self, channel: ResponseChannel<A2aResponse>, response: A2aResponse) {
        let _ = self.swarm.behaviour_mut().a2a.send_response(channel, response);
    }

    fn deny(&mut self, channel: ResponseChannel<A2aResponse>, peer: &PeerId, agent: &str, skill: &str, reason: String) {
        self.audit("in", peer, agent, skill, &format!("denied: {reason}"));
        self.respond(channel, A2aResponse::Denied { reason });
    }

    fn on_request(&mut self, peer: PeerId, request: Signed<Message>, channel: ResponseChannel<A2aResponse>) {
        match request.verify(DEFAULT_MAX_AGE_MS) {
            Ok(signer) if signer == peer => {}
            Ok(_) => return self.deny(channel, &peer, "?", "?", "request not signed by the connected peer".into()),
            Err(e) => return self.deny(channel, &peer, "?", "?", format!("bad signature: {e}")),
        }
        if !self.trust.check_replay(&request.nonce, now_ms()) {
            return self.deny(channel, &peer, "?", "?", "replayed request".into());
        }
        let msg = request.payload;
        let mut to = msg.to.splitn(2, '@');
        let agent = to.next().unwrap_or("").to_string();
        if to.next().is_some_and(|p| p != self.peer_id().to_string()) {
            return self.deny(channel, &peer, &agent, "?", "addressed to a different node".into());
        }
        let skill = msg.skill.clone().unwrap_or_default();
        let Some(manifest) = self.kernel.agent(&agent).filter(|m| m.capabilities.public) else {
            return self.deny(channel, &peer, &agent, &skill, "no such public agent".into());
        };
        if !manifest.skills.iter().any(|s| s.id == skill) {
            return self.deny(channel, &peer, &agent, &skill, "agent does not offer that skill".into());
        }
        let text: String = msg
            .parts
            .iter()
            .filter_map(|p| if let Part::Text { text } = p { Some(text.as_str()) } else { None })
            .collect::<Vec<_>>()
            .join("\n");
        if text.trim().is_empty() || text.len() > MAX_REQUEST_TEXT {
            return self.deny(channel, &peer, &agent, &skill, format!("text must be 1..={MAX_REQUEST_TEXT} bytes"));
        }

        match self.trust.evaluate(&Origin::Peer(peer), Some(&skill), now_ms()) {
            Decision::Allow => {
                self.audit("in", &peer, &agent, &skill, "allowed");
                self.spawn_answer(peer, agent, skill, text, msg, channel, false);
            }
            Decision::Quarantine => {
                self.audit("in", &peer, &agent, &skill, "quarantined");
                self.spawn_answer(peer, agent, skill, text, msg, channel, true);
            }
            Decision::AskHuman => {
                let p = peer.to_string();
                let id = match self.inbox.iter_mut().find(|i| i.peer == p && i.agent == agent && i.skill == skill) {
                    Some(item) => {
                        item.text = text;
                        item.received_ms = now_ms();
                        item.id.clone()
                    }
                    None => {
                        let id = random_id()[..8].to_string();
                        self.inbox.push(InboxItem {
                            id: id.clone(),
                            peer: p,
                            agent: agent.clone(),
                            skill: skill.clone(),
                            text,
                            received_ms: now_ms(),
                        });
                        id
                    }
                };
                self.save_state();
                info!(%peer, %agent, %skill, inbox_id = %id, "request held for approval (`systema inbox`)");
                self.audit("in", &peer, &agent, &skill, &format!("held: inbox {id}"));
                self.respond(channel, A2aResponse::Pending { inbox_id: id, reason: "awaiting operator approval".into() });
            }
            Decision::Deny(reason) => self.deny(channel, &peer, &agent, &skill, reason.into()),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_answer(
        &self,
        peer: PeerId,
        agent: String,
        skill: String,
        text: String,
        msg: Message,
        channel: ResponseChannel<A2aResponse>,
        quarantined: bool,
    ) {
        let kernel = self.kernel.clone();
        let keypair = self.keypair.clone();
        let done = self.done_tx.clone();
        let me = self.peer_id();
        tokio::spawn(async move {
            let p = peer.to_string();
            let result = if quarantined {
                kernel.quarantine(&p, &text).await
            } else {
                kernel.handle_remote(&p, &agent, &skill, &text).await
            };
            let response = match result {
                Ok(answer) => {
                    let from = if quarantined { format!("quarantine@{me}") } else { format!("{agent}@{me}") };
                    let reply = Message {
                        id: random_id(),
                        from,
                        to: msg.from,
                        role: Role::Agent,
                        skill: Some(skill),
                        task_id: Some(msg.id),
                        parts: vec![Part::Text { text: answer }],
                    };
                    match Signed::sign(&keypair, reply) {
                        Ok(reply) => A2aResponse::Ok { reply },
                        Err(e) => A2aResponse::Error { reason: format!("signing failed: {e}") },
                    }
                }
                Err(e) => {
                    warn!(%peer, "agent failed on remote request: {e:#}");
                    A2aResponse::Error { reason: "agent failed to answer".into() }
                }
            };
            let _ = done.send((channel, response)).await;
        });
    }

    // ---------------------------------------------------------------------------------
    // Direct requests: outbound and control
    // ---------------------------------------------------------------------------------

    fn on_response(&mut self, request_id: OutboundRequestId, response: A2aResponse) {
        let Some((peer, reply)) = self.pending_out.remove(&request_id) else { return };
        let out = match response {
            A2aResponse::Ok { reply: r } => match r.verify(DEFAULT_MAX_AGE_MS) {
                Ok(signer) if signer == peer => {
                    Ok(json!({ "status": "ok", "from": r.payload.from, "text": r.payload.first_text() }))
                }
                Ok(_) => Err("reply was signed by a different peer".to_string()),
                Err(e) => Err(format!("reply failed verification: {e}")),
            },
            other => serde_json::to_value(&other).map_err(|e| e.to_string()),
        };
        let _ = reply.send(out);
    }

    fn on_control(&mut self, cmd: Control, reply: oneshot::Sender<ControlReply>) {
        let result: ControlReply = match cmd {
            Control::Peers => Ok(json!(
                self.discovered.iter().map(|(p, cards)| json!({ "peer": p.to_string(), "agents": cards })).collect::<Vec<_>>()
            )),
            Control::Send { peer, agent, skill, text } => {
                let peer: PeerId = match peer.parse() {
                    Ok(p) => p,
                    Err(_) => return drop(reply.send(Err("bad peer id".into()))),
                };
                if text.len() > MAX_REQUEST_TEXT {
                    return drop(reply.send(Err(format!("text over {MAX_REQUEST_TEXT} bytes"))));
                }
                // Teach the DHT/dialer every address the peer's cards advertise.
                let addrs: Vec<Multiaddr> = self
                    .discovered
                    .get(&peer)
                    .into_iter()
                    .flatten()
                    .flat_map(|c| &c.transports)
                    .filter(|t| t.kind == "libp2p")
                    .filter_map(|t| t.address.parse().ok())
                    .collect();
                for a in addrs {
                    self.swarm.behaviour_mut().kad.add_address(&peer, a);
                }
                let msg = Message {
                    id: random_id(),
                    from: format!("operator@{}", self.peer_id()),
                    to: format!("{agent}@{peer}"),
                    role: Role::User,
                    skill: Some(skill.clone()),
                    task_id: None,
                    parts: vec![Part::Text { text }],
                };
                match Signed::sign(&self.keypair, msg) {
                    Ok(signed) => {
                        let id = self.swarm.behaviour_mut().a2a.send_request(&peer, signed);
                        self.audit("out", &peer, &agent, &skill, "sent");
                        self.pending_out.insert(id, (peer, reply));
                        return;
                    }
                    Err(e) => Err(format!("signing failed: {e}")),
                }
            }
            Control::Inbox => Ok(json!(self.inbox)),
            Control::Approve { id, per_hour, hours } => match self.inbox.iter().position(|i| i.id == id) {
                None => Err(format!("no inbox item {id}")),
                Some(pos) => {
                    let item = self.inbox.remove(pos);
                    let grant = Grant {
                        peer: item.peer.clone(),
                        skill: item.skill.clone(),
                        max_per_hour: per_hour.max(1),
                        expires_at_ms: hours.map(|h| now_ms() + h * 3_600_000),
                    };
                    self.trust.grant(grant.clone());
                    self.inbox.retain(|i| !(i.peer == item.peer && i.skill == item.skill));
                    self.save_state();
                    if let Ok(p) = item.peer.parse() {
                        self.audit("ctl", &p, &item.agent, &item.skill, &format!("approved {}/h", grant.max_per_hour));
                    }
                    Ok(json!(grant))
                }
            },
            Control::Reject { id } => {
                let before = self.inbox.len();
                self.inbox.retain(|i| i.id != id);
                self.save_state();
                if self.inbox.len() < before { Ok(json!({ "rejected": id })) } else { Err(format!("no inbox item {id}")) }
            }
            Control::Block { peer } => match peer.parse::<PeerId>() {
                Err(_) => Err("bad peer id".into()),
                Ok(p) => {
                    self.trust.block(&p);
                    self.inbox.retain(|i| i.peer != peer);
                    self.save_state();
                    let _ = self.swarm.disconnect_peer_id(p);
                    self.audit("ctl", &p, "*", "*", "blocked");
                    Ok(json!({ "blocked": peer }))
                }
            },
            Control::Grants => Ok(json!(self.trust.policy())),
        };
        let _ = reply.send(result);
    }

    // ---------------------------------------------------------------------------------
    // State
    // ---------------------------------------------------------------------------------

    fn save_state(&self) {
        if let Err(e) = write_json(&self.home.join("trust.json"), self.trust.policy())
            .and_then(|_| write_json(&self.home.join("inbox.json"), &self.inbox))
        {
            warn!("saving trust/inbox state: {e}");
        }
    }

    fn audit(&self, dir: &str, peer: &PeerId, agent: &str, skill: &str, outcome: &str) {
        let line = json!({ "ts": now_ms(), "dir": dir, "peer": peer.to_string(), "agent": agent, "skill": skill, "outcome": outcome });
        let res = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.home.join("audit.jsonl"))
            .and_then(|mut f| writeln!(f, "{line}"));
        if let Err(e) = res {
            warn!("audit log: {e}");
        }
    }

    fn handle(&mut self, ev: SwarmEvent<BehaviourEvent>) {
        match ev {
            SwarmEvent::NewListenAddr { address, .. } => info!("listening on {address}/p2p/{}", self.peer_id()),
            SwarmEvent::ConnectionEstablished { peer_id, endpoint, .. } => {
                if self.trust.policy().blocked.contains(&peer_id.to_string()) {
                    let _ = self.swarm.disconnect_peer_id(peer_id);
                    return;
                }
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
            SwarmEvent::Behaviour(BehaviourEvent::A2a(request_response::Event::Message { peer, message, .. })) => {
                match message {
                    request_response::Message::Request { request, channel, .. } => self.on_request(peer, request, channel),
                    request_response::Message::Response { request_id, response } => self.on_response(request_id, response),
                }
            }
            SwarmEvent::Behaviour(BehaviourEvent::A2a(request_response::Event::OutboundFailure {
                request_id, error, ..
            })) => {
                if let Some((_, reply)) = self.pending_out.remove(&request_id) {
                    let _ = reply.send(Err(format!("request failed: {error}")));
                }
            }
            SwarmEvent::Behaviour(BehaviourEvent::A2a(request_response::Event::InboundFailure { peer, error, .. })) => {
                debug!(%peer, "inbound a2a failure: {error}")
            }
            SwarmEvent::Behaviour(BehaviourEvent::Dcutr(e)) => info!(?e, "hole punch"),
            SwarmEvent::Behaviour(BehaviourEvent::Kad(kad::Event::RoutingUpdated { peer, .. })) => debug!(%peer, "kad routing"),
            _ => {}
        }
    }
}

fn read_json<T: serde::de::DeserializeOwned>(path: &std::path::Path) -> anyhow::Result<Option<T>> {
    match std::fs::read(path) {
        Ok(b) => Ok(Some(serde_json::from_slice(&b).with_context(|| format!("parse {}", path.display()))?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn write_json<T: Serialize>(path: &std::path::Path, value: &T) -> anyhow::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(value)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}
