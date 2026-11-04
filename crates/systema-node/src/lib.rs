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

use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

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
use systema_kernel::{
    Answer, Approver, Event, EventKind, Kernel, ToolRequest, TrustState,
    config::{DhtMode, NodeConfig},
};
use tokio_util::sync::CancellationToken;
use systema_proto::{
    A2A_PROTOCOL, A2aResponse, AgentCard, DEFAULT_MAX_AGE_MS, MAX_REQUEST_TEXT, Message, Part, Role, Signed, now_ms,
    random_id,
};
use systema_trust::{Decision, Grant, Origin, Policy, TrustGate};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

use control::{Control, ControlReply};

pub const CARDS_TOPIC: &str = "systema/v1/cards";
/// Gossip topic for the swarm-wide blackboard (board posts shared across nodes).
pub const BOARD_TOPIC: &str = "systema/v1/board";

/// A board post gossiped to the swarm. Signed by the originating node; peers add it to their local
/// board marked as a remote (untrusted) author.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BoardMsg {
    pub author: String,
    pub topic: String,
    pub text: String,
}
pub const KAD_PROTOCOL: StreamProtocol = StreamProtocol::new("/systema/kad/1.0.0");
pub const IDENTIFY_PROTOCOL: &str = "/systema/id/1.0.0";
/// Remote agents run on a shared GPU at background priority; give them time.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(600);

type A2a = request_response::json::Behaviour<Signed<Message>, A2aResponse>;
type Cards = request_response::json::Behaviour<CardQuery, Vec<Signed<AgentCard>>>;

pub const CARD_PROTOCOL: &str = "/systema/card/1.0.0";
/// How long `find` waits for DHT answers and card fetches.
pub const FIND_TIMEOUT: Duration = Duration::from_secs(20);
/// Minimum gap between re-advertising our skills in the DHT.
const REPROVIDE_GAP: Duration = Duration::from_secs(60);

/// Ask a node for its public agent cards, optionally only those offering `skill`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CardQuery {
    #[serde(default)]
    pub skill: Option<String>,
}

/// A relay we want a reservation on. The reservation is requested only once a connection
/// to the relay exists (asking while a dial is in flight makes the relay client's own dial
/// fail and the listener close), and re-requested if it lapses.
struct RelayState {
    peer: PeerId,
    addr: Multiaddr,
    listener: Option<libp2p::core::transport::ListenerId>,
    last_attempt: Option<Instant>,
}

const RELAY_RETRY: Duration = Duration::from_secs(30);

/// An in-flight `systema find`: DHT provider lookup, then card fetches.
struct FindJob {
    skill: String,
    reply: Option<oneshot::Sender<ControlReply>>,
    providers: HashSet<PeerId>,
    dht_done: bool,
    cards_pending: HashSet<OutboundRequestId>,
    cards: HashMap<PeerId, Vec<AgentCard>>,
    errors: Vec<String>,
    deadline: Instant,
}

#[derive(NetworkBehaviour)]
pub struct Behaviour {
    relay_client: relay::client::Behaviour,
    relay_server: Toggle<relay::Behaviour>,
    dcutr: dcutr::Behaviour,
    identify: identify::Behaviour,
    ping: ping::Behaviour,
    mdns: Toggle<mdns::tokio::Behaviour>,
    kad: kad::Behaviour<kad::store::MemoryStore>,
    gossipsub: gossipsub::Behaviour,
    a2a: A2a,
    cards: Cards,
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

/// A tool approval awaiting an operator answer: the answer channel plus the details the UI
/// needs to render it (so a UI that connects mid-wait can still show and answer it).
struct PendingApproval {
    tx: std::sync::mpsc::Sender<Answer>,
    agent: String,
    request: String,
    reason: String,
    rememberable: bool,
}

/// Shared registry of pending tool approvals, keyed by approval id.
type PendingApprovals = Arc<std::sync::Mutex<HashMap<String, PendingApproval>>>;

/// [`Approver`] for the daemon: emits an `ApprovalNeeded` event and blocks (on a dedicated
/// worker thread, via the kernel's `block_in_place`) until the operator answers via the
/// control socket, or a timeout elapses.
struct DaemonApprover {
    events: tokio::sync::broadcast::Sender<Event>,
    pending: PendingApprovals,
}

/// How long a tool approval waits for the operator before defaulting to deny.
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(600);

impl Approver for DaemonApprover {
    fn approve(&self, agent: &str, request: &ToolRequest, reason: &str, rememberable: bool) -> Answer {
        let id = random_id()[..8].to_string();
        let (tx, rx) = std::sync::mpsc::channel();
        self.pending.lock().unwrap().insert(
            id.clone(),
            PendingApproval { tx, agent: agent.to_string(), request: request.summary(), reason: reason.to_string(), rememberable },
        );
        let _ = self.events.send(Event {
            ts: now_ms(),
            agent: Some(agent.to_string()),
            session: None,
            kind: EventKind::ApprovalNeeded {
                id: id.clone(),
                request: request.summary(),
                reason: reason.to_string(),
                rememberable,
            },
        });
        let answer = rx.recv_timeout(APPROVAL_TIMEOUT).unwrap_or(Answer::Deny);
        self.pending.lock().unwrap().remove(&id);
        let _ = self.events.send(Event {
            ts: now_ms(),
            agent: Some(agent.to_string()),
            session: None,
            kind: EventKind::ApprovalResolved { id, decision: format!("{answer:?}") },
        });
        answer
    }
}

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
    finds: HashMap<u64, FindJob>,
    next_find: u64,
    find_by_query: HashMap<kad::QueryId, u64>,
    find_by_card: HashMap<OutboundRequestId, u64>,
    last_provide: Option<Instant>,
    relays: Vec<RelayState>,
    /// Operator turns in flight, keyed by session, with a token to cancel them.
    running: HashMap<String, CancellationToken>,
    pending_approvals: PendingApprovals,
    /// Last-run times for scheduled jobs (persisted), so restarts don't double-fire.
    sched_state: systema_kernel::schedule::State,
    /// Per-skill round-robin cursor for spreading parallel delegations across providers.
    rr_cursor: HashMap<String, usize>,
    /// How well each peer has served our requests (persisted), used to rank swarm providers.
    reputation: HashMap<String, Rep>,
}

/// A peer's track record serving our outbound requests. Laplace-smoothed so a peer with no
/// history sits at a neutral 0.5 rather than a misleading 0 or 1.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Rep {
    pub ok: u32,
    pub fail: u32,
}

impl Rep {
    pub fn score(&self) -> f64 {
        (self.ok as f64 + 1.0) / (self.ok as f64 + self.fail as f64 + 2.0)
    }
}

impl Node {
    pub fn new(keypair: Keypair, cfg: NodeConfig, kernel: Arc<Kernel>, home: PathBuf) -> anyhow::Result<Self> {
        let mdns_on = cfg.mdns;
        let relay_server_on = cfg.relay_server;
        let limits = cfg.relay_limits.clone();
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
                let cards = Cards::new(
                    [(StreamProtocol::new(CARD_PROTOCOL), ProtocolSupport::Full)],
                    request_response::Config::default().with_request_timeout(Duration::from_secs(15)),
                );

                let relay_server = relay_server_on.then(|| {
                    relay::Behaviour::new(
                        peer_id,
                        relay::Config {
                            max_reservations: limits.max_reservations,
                            max_circuits: limits.max_circuits,
                            max_circuits_per_peer: limits.max_circuits_per_peer,
                            max_circuit_duration: Duration::from_secs(limits.max_circuit_secs),
                            max_circuit_bytes: limits.max_circuit_bytes,
                            ..Default::default()
                        },
                    )
                });

                Ok(Behaviour {
                    relay_client,
                    relay_server: Toggle::from(relay_server),
                    dcutr: dcutr::Behaviour::new(peer_id),
                    identify: identify::Behaviour::new(identify::Config::new(IDENTIFY_PROTOCOL.into(), key.public())),
                    ping: ping::Behaviour::default(),
                    mdns: Toggle::from(mdns),
                    kad,
                    gossipsub,
                    a2a,
                    cards,
                })
            })?
            .with_swarm_config(|c| c.with_idle_connection_timeout(REQUEST_TIMEOUT))
            .build();
        let mut swarm = swarm;
        swarm.behaviour_mut().kad.set_mode(match cfg.dht_server {
            DhtMode::Auto => None,
            DhtMode::On => Some(kad::Mode::Server),
            DhtMode::Off => Some(kad::Mode::Client),
        });

        // Persisted trust state (grants made via `approve`) wins over the config file's.
        let policy: Policy = read_json(&home.join("trust.json"))?.unwrap_or_else(|| cfg.trust.clone());
        let inbox: Vec<InboxItem> = read_json(&home.join("inbox.json"))?.unwrap_or_default();
        let (done_tx, done_rx) = mpsc::channel(64);
        let sched_state = systema_kernel::schedule::load_state(&home);
        let reputation: HashMap<String, Rep> = read_json(&home.join("reputation.json"))?.unwrap_or_default();
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
            finds: HashMap::new(),
            next_find: 0,
            find_by_query: HashMap::new(),
            find_by_card: HashMap::new(),
            last_provide: None,
            relays: Vec::new(),
            running: HashMap::new(),
            pending_approvals: Arc::default(),
            sched_state,
            rr_cursor: HashMap::new(),
            reputation,
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

    /// Freshly signed public cards, with current listen addresses, optionally filtered by skill.
    fn cards_for_peers(&self, skill: Option<&str>) -> Vec<Signed<AgentCard>> {
        let addrs = self.listen_addrs();
        self.local_cards
            .iter()
            .filter(|c| skill.is_none_or(|s| c.skills.iter().any(|x| x.id == s)))
            .filter_map(|c| {
                let mut c = c.clone();
                c.transports = addrs.iter().map(|a| systema_proto::Transport { kind: "libp2p".into(), address: a.clone() }).collect();
                Signed::sign(&self.keypair, c).ok()
            })
            .collect()
    }

    /// Run until ctrl-c.
    pub async fn run(mut self) -> anyhow::Result<()> {
        for l in &self.cfg.listen {
            self.swarm.listen_on(l.parse().with_context(|| format!("bad listen addr {l}"))?)?;
        }
        for e in &self.cfg.external_addrs {
            let addr: Multiaddr = e.parse().with_context(|| format!("bad external addr {e}"))?;
            self.swarm.add_external_address(addr);
        }
        for r in &self.cfg.relays {
            let addr: Multiaddr = r.parse().with_context(|| format!("bad relay addr {r}"))?;
            let Some(libp2p::multiaddr::Protocol::P2p(relay_peer)) = addr.iter().last() else {
                anyhow::bail!("relay addr {r} must end in /p2p/<peer id>");
            };
            self.swarm.behaviour_mut().kad.add_address(&relay_peer, addr.clone());
            self.relays.push(RelayState { peer: relay_peer, addr, listener: None, last_attempt: None });
        }
        self.tend_relays();
        if self.cfg.relay_server {
            info!("acting as a circuit relay for other nodes");
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
        self.swarm.behaviour_mut().gossipsub.subscribe(&gossipsub::IdentTopic::new(BOARD_TOPIC))?;
        self.provide_skills(true);

        let (ctl_tx, mut ctl_rx) = mpsc::channel(16);
        control::serve(&self.home, ctl_tx, self.kernel.event_sender())?;
        // Bridge for cross-swarm delegation: the kernel's `delegate_remote` tool sends here.
        let (remote_tx, mut remote_rx) = mpsc::channel::<systema_kernel::RemoteCall>(16);
        self.kernel.set_swarm(remote_tx);
        let mut done_rx = self.done_rx.take().expect("run called twice");
        let mut kernel_events = self.kernel.subscribe();

        let mut announce = tokio::time::interval(Duration::from_secs(self.cfg.announce_secs.max(5)));
        let mut housekeeping = tokio::time::interval(Duration::from_secs(1));
        let mut schedules = tokio::time::interval(Duration::from_secs(15));
        info!(peer_id = %self.peer_id(), "systema node up");

        loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    info!("shutting down");
                    let _ = std::fs::remove_file(control::socket_path(&self.home));
                    return Ok(());
                }
                _ = announce.tick() => if self.cfg.gossip_cards { self.announce(&topic) },
                _ = housekeeping.tick() => { self.expire_finds(); self.tend_relays(); }
                _ = schedules.tick() => self.check_schedules(),
                Some(call) = remote_rx.recv() => self.on_remote_call(call),
                Some((channel, response)) = done_rx.recv() => {
                    if self.swarm.behaviour_mut().a2a.send_response(channel, response).is_err() {
                        debug!("requester went away before the reply was ready");
                    }
                }
                ev = kernel_events.recv() => if let Ok(ev) = ev { self.on_kernel_event(ev); },
                Some((cmd, reply)) = ctl_rx.recv() => self.on_control(cmd, reply),
                ev = self.swarm.select_next_some() => self.handle(ev),
            }
        }
    }

    /// Prune finished operator turns so `Sessions`/`Cancel` reflect reality.
    fn on_kernel_event(&mut self, ev: Event) {
        match &ev.kind {
            EventKind::TurnFinished { .. } => {
                if let Some(session) = ev.session {
                    self.running.remove(&session);
                }
            }
            // A local agent posted to the board: gossip it to the swarm's shared blackboard.
            EventKind::BoardPosted { topic, text } => {
                let msg = BoardMsg {
                    author: ev.agent.clone().unwrap_or_else(|| "agent".into()),
                    topic: topic.clone(),
                    text: text.clone(),
                };
                if let Ok(bytes) = Signed::sign(&self.keypair, msg).and_then(|s| s.to_json()) {
                    let t = gossipsub::IdentTopic::new(BOARD_TOPIC);
                    match self.swarm.behaviour_mut().gossipsub.publish(t, bytes) {
                        Ok(_) | Err(gossipsub::PublishError::NoPeersSubscribedToTopic) => {}
                        Err(e) => debug!("publish board post: {e}"),
                    }
                }
            }
            _ => {}
        }
    }

    /// A board post gossiped by another node: verify it and add it to our local board as a remote
    /// (untrusted) author, so our agents see the swarm's blackboard.
    fn on_board_gossip(&mut self, bytes: &[u8]) {
        let Ok(signed) = Signed::<BoardMsg>::from_json(bytes) else { return };
        let Ok(signer) = signed.verify(DEFAULT_MAX_AGE_MS) else { return };
        if self.trust.policy().blocked.contains(&signer.to_string()) {
            return;
        }
        let m = signed.payload;
        let peer = signer.to_string();
        let author = format!("{}@{}", m.author, &peer[..peer.len().min(10)]);
        // public = true marks it untrusted to local readers (it came from a stranger's node).
        let _ = systema_kernel::board::post(&self.home, &author, true, false, &m.topic, &m.text);
    }

    // ---------------------------------------------------------------------------------
    // Discovery
    // ---------------------------------------------------------------------------------

    /// Advertise our public skills as DHT provider records. Re-run (at most once a minute)
    /// when new peers join, since records published into an empty routing table reach nobody.
    /// Keep a reservation on every configured relay: dial when disconnected, reserve
    /// when connected without one, at most every RELAY_RETRY per relay.
    fn tend_relays(&mut self) {
        for i in 0..self.relays.len() {
            let r = &self.relays[i];
            if r.listener.is_some() || r.last_attempt.is_some_and(|t| t.elapsed() < RELAY_RETRY) {
                continue;
            }
            let (peer, addr) = (r.peer, r.addr.clone());
            self.relays[i].last_attempt = Some(Instant::now());
            if self.swarm.is_connected(&peer) {
                match self.swarm.listen_on(addr.clone().with(libp2p::multiaddr::Protocol::P2pCircuit)) {
                    Ok(id) => self.relays[i].listener = Some(id),
                    Err(e) => warn!(relay = %peer, "relay listen: {e}"),
                }
            } else if let Err(e) = self.swarm.dial(addr) {
                debug!(relay = %peer, "relay dial: {e}");
            }
        }
    }

    fn provide_skills(&mut self, startup: bool) {
        if !startup {
            if self.last_provide.is_some_and(|t| t.elapsed() < REPROVIDE_GAP) {
                return;
            }
            self.last_provide = Some(Instant::now());
        }
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
        // Track how well this peer served us, for provider ranking.
        let rep = self.reputation.entry(peer.to_string()).or_default();
        if out.is_ok() {
            rep.ok += 1;
        } else {
            rep.fail += 1;
        }
        let _ = write_json(&self.home.join("reputation.json"), &self.reputation);
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
            Control::Find { skill } => {
                let key = kad::RecordKey::new(&AgentCard::skill_key(&skill));
                let qid = self.swarm.behaviour_mut().kad.get_providers(key);
                let id = self.next_find;
                self.next_find += 1;
                self.find_by_query.insert(qid, id);
                self.finds.insert(
                    id,
                    FindJob {
                        skill,
                        reply: Some(reply),
                        providers: HashSet::new(),
                        dht_done: false,
                        cards_pending: HashSet::new(),
                        cards: HashMap::new(),
                        errors: Vec::new(),
                        deadline: Instant::now() + FIND_TIMEOUT,
                    },
                );
                return;
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

            // --- agent control plane ---
            Control::Ask { agent, text } => {
                if self.kernel.agent(&agent).is_none() {
                    Err(format!("no agent named {agent}"))
                } else if self.running.contains_key(&agent) {
                    Err(format!("{agent} already has a turn running; steer or cancel it first"))
                } else {
                    self.spawn_operator_turn(agent.clone(), agent, text);
                    Ok(json!({ "started": true }))
                }
            }
            Control::Route { text } => {
                // Routing is a model call; run it off the event loop and reply when done.
                let kernel = self.kernel.clone();
                tokio::spawn(async move {
                    let r = kernel.route(&text).await.map(|a| json!({ "agent": a })).map_err(|e| e.to_string());
                    let _ = reply.send(r);
                });
                return;
            }
            Control::Steer { session, text } => {
                if self.running.contains_key(&session) {
                    let kernel = self.kernel.clone();
                    let s = session.clone();
                    tokio::spawn(async move { kernel.steer(&s, &text).await });
                    Ok(json!({ "steered": session }))
                } else if self.kernel.agent(&session).is_some() {
                    // Idle agent: steering starts a fresh turn with the text as the prompt.
                    self.spawn_operator_turn(session.clone(), session.clone(), text);
                    Ok(json!({ "started": session }))
                } else {
                    Err(format!("no running session or agent named {session}"))
                }
            }
            Control::Cancel { session } => match self.running.remove(&session) {
                Some(token) => {
                    token.cancel();
                    Ok(json!({ "cancelled": session }))
                }
                None => Err(format!("no running turn for {session}")),
            },
            Control::Sessions => Ok(json!({
                "running": self.running.keys().cloned().collect::<Vec<_>>(),
            })),
            Control::Resolve { id, answer, remember } => {
                let answer = if remember && matches!(answer, Answer::Once) { Answer::Always } else { answer };
                match self.pending_approvals.lock().unwrap().get(&id).map(|p| p.tx.clone()) {
                    Some(tx) => {
                        let _ = tx.send(answer);
                        Ok(json!({ "resolved": id }))
                    }
                    None => Err(format!("no pending approval {id} (it may have timed out)")),
                }
            }
            Control::Approvals => Ok(json!(
                self.pending_approvals
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|(id, p)| json!({
                        "id": id,
                        "agent": p.agent,
                        "request": p.request,
                        "reason": p.reason,
                        "rememberable": p.rememberable,
                    }))
                    .collect::<Vec<_>>()
            )),
            Control::Rules => Ok(json!(self.kernel.approvals.rules())),
            Control::Forget { agent, key } => {
                if self.kernel.approvals.forget(&agent, &key) {
                    Ok(json!({ "forgot": key }))
                } else {
                    Err("no such remembered rule".into())
                }
            }
            Control::Agents => Ok(json!(
                self.kernel.agents().map(|m| json!({
                    "name": m.name,
                    "description": m.description,
                    "model": format!("{:?}", m.model).to_lowercase(),
                    "priority": format!("{:?}", m.priority).to_lowercase(),
                    "public": m.capabilities.public,
                    "skills": m.skills.iter().map(|s| s.id.clone()).collect::<Vec<_>>(),
                })).collect::<Vec<_>>()
            )),
            Control::Status => {
                let addrs = self.listen_addrs();
                let reservations = self.relays.iter().filter(|r| r.listener.is_some()).count();
                Ok(json!({
                    "peer_id": self.peer_id().to_string(),
                    "listen": addrs,
                    "running": self.running.keys().cloned().collect::<Vec<_>>(),
                    "peers": self.discovered.len(),
                    "agents": self.kernel.agents().count(),
                    "inbox": self.inbox.len(),
                    "grants": self.trust.policy().grants.len(),
                    "blocked": self.trust.policy().blocked.len(),
                    "relay_server": self.cfg.relay_server,
                    "relay_reservations": reservations,
                    "pending_approvals": self.pending_approvals.lock().unwrap().len(),
                }))
            }
            Control::FireWebhook { name } => {
                let mut fired = Vec::new();
                let mut skipped = Vec::new();
                for job in systema_kernel::schedule::http_jobs(&self.home, &name) {
                    let session = format!("sched:{}", job.name);
                    if self.running.contains_key(&session) {
                        debug!(job = %job.name, "webhook-triggered job still running; skipping this fire");
                        skipped.push(job.name);
                        continue;
                    }
                    if self.kernel.agent(&job.agent).is_none() {
                        warn!(job = %job.name, agent = %job.agent, "webhook-triggered job names an unknown agent");
                        skipped.push(job.name);
                        continue;
                    }
                    info!(job = %job.name, agent = %job.agent, webhook = %name, "firing webhook-triggered job");
                    self.spawn_operator_turn(job.agent.clone(), session, job.prompt.clone());
                    fired.push(job.name);
                }
                Ok(json!({ "fired": fired, "skipped": skipped }))
            }
            // Streamed by the socket handler, never dispatched here.
            Control::Watch => Err("watch is a streaming command".into()),
        };
        let _ = reply.send(result);
    }

    /// Spawn an operator turn, tracking its cancellation token under `session`.
    /// Fire any scheduled jobs that are due. Jobs are reloaded from `schedules.toml` each tick,
    /// so edits take effect without a restart. A job already running (its previous turn hasn't
    /// finished) is skipped rather than overlapped.
    fn check_schedules(&mut self) {
        let jobs = systema_kernel::schedule::load(&self.home);
        if jobs.is_empty() {
            return;
        }
        let now = systema_proto::now_ms();
        let due = systema_kernel::schedule::check(&jobs, &mut self.sched_state, &self.home, now);
        systema_kernel::schedule::save_state(&self.home, &self.sched_state);
        for job in due {
            let session = format!("sched:{}", job.name);
            if self.running.contains_key(&session) {
                debug!(job = %job.name, "scheduled job still running; skipping this tick");
                continue;
            }
            if self.kernel.agent(&job.agent).is_none() {
                warn!(job = %job.name, agent = %job.agent, "scheduled job names an unknown agent");
                continue;
            }
            info!(job = %job.name, agent = %job.agent, "firing scheduled job");
            self.spawn_operator_turn(job.agent.clone(), session, job.prompt.clone());
        }
    }

    /// Handle a cross-swarm delegation from the kernel: find a discovered peer whose agent offers
    /// the requested skill, send it the task over A2A, and forward its reply back to the kernel.
    fn on_remote_call(&mut self, call: systema_kernel::RemoteCall) {
        let systema_kernel::RemoteCall { skill, task, spread, reply } = call;
        if task.len() > MAX_REQUEST_TEXT {
            let _ = reply.send(Err(format!("task text over {MAX_REQUEST_TEXT} bytes")));
            return;
        }
        let blocked = self.trust.policy().blocked.clone();
        // Providers of this skill, most capable (largest advertised context) first. A single
        // delegation takes the most capable; parallel fan-out round-robins to spread over nodes.
        // Contract-net-style award: among providers of the skill, prefer a currently-connected
        // peer (liveness — a dead peer never wins), then best reputation, then most capable
        // (largest advertised context). Each candidate is scored (connected, reputation, context).
        let rep_score = |peer: &PeerId| self.reputation.get(&peer.to_string()).map_or(0.5, Rep::score);
        let candidates: Vec<(PeerId, u32, String)> = self
            .discovered
            .iter()
            .filter(|(peer, _)| !blocked.contains(&peer.to_string()))
            .flat_map(|(peer, cards)| cards.iter().map(move |c| (*peer, c.max_context.unwrap_or(0), c)))
            .filter(|(_, _, c)| c.skills.iter().any(|s| s.id == skill))
            .map(|(peer, ctx, c)| (peer, ctx, c.name.clone()))
            .collect();
        let mut scored: Vec<(bool, f64, u32, PeerId, String)> = candidates
            .into_iter()
            .map(|(peer, ctx, name)| (self.swarm.is_connected(&peer), rep_score(&peer), ctx, peer, name))
            .collect();
        scored.sort_by(|a, b| {
            b.0.cmp(&a.0) // connected first
                .then(b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal)) // reputation
                .then(b.2.cmp(&a.2)) // capability
        });
        // Downstream selection (single pick vs. round-robin) uses (_, _, peer, name) tuples.
        let ranked: Vec<(f64, u32, PeerId, String)> = scored.into_iter().map(|(_c, r, ctx, p, n)| (r, ctx, p, n)).collect();
        let chosen = if spread && !ranked.is_empty() {
            let i = self.rr_cursor.entry(skill.clone()).or_insert(0);
            let pick = ranked[*i % ranked.len()].clone();
            *i = i.wrapping_add(1);
            Some((pick.2, pick.3))
        } else {
            ranked.first().map(|(_, _, p, n)| (*p, n.clone()))
        };
        let Some((peer, agent)) = chosen else {
            let _ = reply.send(Err(format!(
                "no known swarm agent offers skill `{skill}` (run `find {skill}` or wait for discovery)"
            )));
            return;
        };
        // Teach the dialer the peer's advertised addresses.
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
            parts: vec![Part::Text { text: task }],
        };
        let signed = match Signed::sign(&self.keypair, msg) {
            Ok(s) => s,
            Err(e) => {
                let _ = reply.send(Err(format!("signing failed: {e}")));
                return;
            }
        };
        let id = self.swarm.behaviour_mut().a2a.send_request(&peer, signed);
        self.audit("out", &peer, &agent, &skill, "delegated");
        // Reuse the outbound-response path: stash a control-reply oneshot, and adapt its JSON
        // result to the plain text the kernel's `delegate_remote` expects.
        let (ctl_tx, ctl_rx) = oneshot::channel();
        self.pending_out.insert(id, (peer, ctl_tx));
        tokio::spawn(async move {
            let mapped = match ctl_rx.await {
                Ok(Ok(v)) => Ok(v["text"].as_str().unwrap_or("").to_string()),
                Ok(Err(e)) => Err(e),
                Err(_) => Err("the node dropped the request".to_string()),
            };
            let _ = reply.send(mapped);
        });
    }

    fn spawn_operator_turn(&mut self, agent: String, session: String, text: String) {
        let cancel = CancellationToken::new();
        self.running.insert(session.clone(), cancel.clone());
        let kernel = self.kernel.clone();
        let approver = Arc::new(DaemonApprover {
            events: self.kernel.event_sender(),
            pending: self.pending_approvals.clone(),
        });
        tokio::spawn(async move {
            let _ = kernel
                .ask_with_fallback(&agent, &session, &text, None, TrustState::operator(), approver.as_ref(), &cancel)
                .await;
            // The node prunes `running` on the TurnFinished event.
        });
    }

    // ---------------------------------------------------------------------------------
    // Find: DHT provider lookup + direct card fetch
    // ---------------------------------------------------------------------------------

    fn on_providers(&mut self, qid: kad::QueryId, found: Vec<PeerId>, last: bool) {
        let Some(&id) = self.find_by_query.get(&qid) else { return };
        let me = self.peer_id();
        let skill = match self.finds.get(&id) {
            Some(j) => j.skill.clone(),
            None => return,
        };
        for p in found {
            if p == me || self.trust.policy().blocked.contains(&p.to_string()) {
                continue;
            }
            let Some(job) = self.finds.get_mut(&id) else { return };
            if job.providers.insert(p) {
                let rid = self.swarm.behaviour_mut().cards.send_request(&p, CardQuery { skill: Some(skill.clone()) });
                self.find_by_card.insert(rid, id);
                if let Some(job) = self.finds.get_mut(&id) {
                    job.cards_pending.insert(rid);
                }
            }
        }
        if last {
            self.find_by_query.remove(&qid);
            if let Some(job) = self.finds.get_mut(&id) {
                job.dht_done = true;
            }
        }
        self.maybe_finish(id);
    }

    fn on_card_response(&mut self, peer: PeerId, rid: OutboundRequestId, cards: Result<Vec<Signed<AgentCard>>, String>) {
        let Some(id) = self.find_by_card.remove(&rid) else { return };
        let Some(job) = self.finds.get_mut(&id) else { return };
        job.cards_pending.remove(&rid);
        match cards {
            Err(e) => job.errors.push(format!("{peer}: {e}")),
            Ok(list) => {
                let mut good = Vec::new();
                for signed in list {
                    match signed.verify_card(DEFAULT_MAX_AGE_MS) {
                        Ok(signer) if signer == peer => {
                            if signed.payload.skills.iter().any(|s| s.id == job.skill) {
                                good.push(signed.payload);
                            }
                        }
                        Ok(_) => job.errors.push(format!("{peer}: card signed by another peer")),
                        Err(e) => job.errors.push(format!("{peer}: bad card: {e}")),
                    }
                }
                if !good.is_empty() {
                    job.cards.insert(peer, good.clone());
                    let entry = self.discovered.entry(peer).or_default();
                    for c in good {
                        entry.retain(|x| x.name != c.name);
                        entry.push(c);
                    }
                }
            }
        }
        self.maybe_finish(id);
    }

    fn maybe_finish(&mut self, id: u64) {
        let done = self.finds.get(&id).is_some_and(|j| j.dht_done && j.cards_pending.is_empty());
        if done {
            self.finish_find(id);
        }
    }

    fn expire_finds(&mut self) {
        let now = Instant::now();
        let expired: Vec<u64> = self.finds.iter().filter(|(_, j)| j.deadline <= now).map(|(id, _)| *id).collect();
        for id in expired {
            if let Some(j) = self.finds.get_mut(&id) {
                if !j.dht_done {
                    j.errors.push("dht lookup timed out".into());
                }
                if !j.cards_pending.is_empty() {
                    j.errors.push(format!("{} card fetches timed out", j.cards_pending.len()));
                }
            }
            self.finish_find(id);
        }
    }

    fn finish_find(&mut self, id: u64) {
        let Some(mut job) = self.finds.remove(&id) else { return };
        self.find_by_query.retain(|_, v| *v != id);
        self.find_by_card.retain(|_, v| *v != id);
        let results: Vec<_> = job
            .cards
            .iter()
            .map(|(p, cards)| json!({ "peer": p.to_string(), "agents": cards }))
            .collect();
        info!(skill = %job.skill, providers = job.providers.len(), verified = results.len(), "find finished");
        if let Some(reply) = job.reply.take() {
            let _ = reply.send(Ok(json!({
                "skill": job.skill,
                "providers": job.providers.len(),
                "results": results,
                "errors": job.errors,
            })));
        }
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
            SwarmEvent::NewListenAddr { address, .. } => {
                let relayed = address.iter().any(|p| matches!(p, libp2p::multiaddr::Protocol::P2pCircuit));
                if relayed {
                    // An accepted reservation is a confirmed way to reach us: advertise it
                    // (in DHT provider records and cards) like any external address.
                    info!("reachable via relay at {address}");
                    self.swarm.add_external_address(address);
                    // Provider records carry the addresses known when they were published. A
                    // record without addresses from a DHT client is dropped from answers (the
                    // serving node can't fill them in from its routing table), so re-advertise
                    // now that we can be reached.
                    self.last_provide = None;
                    self.provide_skills(false);
                } else {
                    info!("listening on {address}/p2p/{}", self.peer_id());
                }
            }
            SwarmEvent::ExternalAddrConfirmed { address } => {
                info!("external address confirmed: {address}");
                self.last_provide = None;
                self.provide_skills(false);
            }
            SwarmEvent::ListenerClosed { listener_id, reason, .. } => {
                if let Some(r) = self.relays.iter_mut().find(|r| r.listener == Some(listener_id)) {
                    warn!(relay = %r.peer, ?reason, "relay reservation lost; will retry");
                    r.listener = None;
                }
            }
            SwarmEvent::Behaviour(BehaviourEvent::RelayServer(e)) => match e {
                relay::Event::ReservationReqAccepted { src_peer_id, .. } => info!(peer = %src_peer_id, "relay: reservation accepted"),
                relay::Event::CircuitReqAccepted { src_peer_id, dst_peer_id } => {
                    info!(src = %src_peer_id, dst = %dst_peer_id, "relay: circuit opened")
                }
                relay::Event::CircuitClosed { src_peer_id, dst_peer_id, error } => {
                    debug!(src = %src_peer_id, dst = %dst_peer_id, ?error, "relay: circuit closed")
                }
                other => debug!(?other, "relay server"),
            },
            SwarmEvent::Behaviour(BehaviourEvent::RelayClient(e)) => match e {
                relay::client::Event::ReservationReqAccepted { relay_peer_id, .. } => {
                    info!(relay = %relay_peer_id, "relay: reservation granted")
                }
                other => debug!(?other, "relay client"),
            },
            SwarmEvent::ConnectionEstablished { peer_id, endpoint, .. } => {
                if self.trust.policy().blocked.contains(&peer_id.to_string()) {
                    let _ = self.swarm.disconnect_peer_id(peer_id);
                    return;
                }
                if let Some(r) = self.relays.iter_mut().find(|r| r.peer == peer_id && r.listener.is_none()) {
                    r.last_attempt = None; // reserve on the next tick, now that we're connected
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
                } else if message.topic.as_str() == BOARD_TOPIC {
                    self.on_board_gossip(&message.data);
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
            SwarmEvent::Behaviour(BehaviourEvent::Cards(request_response::Event::Message { peer, message, .. })) => {
                match message {
                    request_response::Message::Request { request, channel, .. } => {
                        let blocked = self.trust.policy().blocked.contains(&peer.to_string());
                        let cards: Vec<Signed<AgentCard>> = if blocked {
                            vec![]
                        } else {
                            self.cards_for_peers(request.skill.as_deref())
                        };
                        let _ = self.swarm.behaviour_mut().cards.send_response(channel, cards);
                    }
                    request_response::Message::Response { request_id, response } => {
                        self.on_card_response(peer, request_id, Ok(response))
                    }
                }
            }
            SwarmEvent::Behaviour(BehaviourEvent::Cards(request_response::Event::OutboundFailure {
                peer, request_id, error, ..
            })) => self.on_card_response(peer, request_id, Err(error.to_string())),
            SwarmEvent::Behaviour(BehaviourEvent::Kad(kad::Event::OutboundQueryProgressed {
                id,
                result: kad::QueryResult::GetProviders(res),
                step,
                ..
            })) => match res {
                Ok(kad::GetProvidersOk::FoundProviders { providers, .. }) => {
                    self.on_providers(id, providers.into_iter().collect(), step.last)
                }
                Ok(kad::GetProvidersOk::FinishedWithNoAdditionalRecord { .. }) => self.on_providers(id, vec![], true),
                Err(e) => {
                    debug!("get_providers: {e}");
                    self.on_providers(id, vec![], true)
                }
            },
            SwarmEvent::Behaviour(BehaviourEvent::Kad(kad::Event::OutboundQueryProgressed {
                result: kad::QueryResult::StartProviding(res),
                ..
            })) => match res {
                Ok(ok) => debug!(key = ?String::from_utf8_lossy(ok.key.as_ref()), "skill advertised in the DHT"),
                Err(e) => debug!("advertising skill failed: {e}"),
            },
            SwarmEvent::Behaviour(BehaviourEvent::Dcutr(e)) => info!(?e, "hole punch"),
            SwarmEvent::Behaviour(BehaviourEvent::Kad(kad::Event::RoutingUpdated { peer, is_new_peer, .. })) => {
                debug!(%peer, "kad routing");
                if is_new_peer {
                    self.provide_skills(false);
                }
            }
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

#[cfg(test)]
mod rep_tests {
    use super::Rep;

    #[test]
    fn reputation_score_is_laplace_smoothed() {
        assert!((Rep::default().score() - 0.5).abs() < 1e-9, "no history is neutral");
        assert!(Rep { ok: 9, fail: 0 }.score() > 0.8, "mostly-ok ranks high");
        assert!(Rep { ok: 0, fail: 9 }.score() < 0.2, "mostly-fail ranks low");
        assert!(Rep { ok: 9, fail: 0 }.score() > Rep { ok: 1, fail: 0 }.score(), "more wins > fewer");
    }
}
