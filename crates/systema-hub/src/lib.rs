//! A public bootstrap for the Systema swarm.
//!
//! It does exactly two jobs for other nodes and nothing else:
//!
//! * **Kademlia DHT server**: nodes bootstrap to it, publish provider records per skill, and
//!   look skills up through it.
//! * **Circuit relay v2 server**: nodes behind NAT reserve a slot and become reachable at
//!   `<hub>/p2p-circuit/p2p/<self>`; DCUtR then upgrades the relayed link to a direct one.
//!
//! It has no agents, no kernel, no trust or audit state, no gossip, no a2a handler and no
//! inference client, so there is nothing for a stranger to run, approve or read. The only state
//! on disk is its identity key, which fixes the peer id everyone bootstraps to.
//!
//! It is internet-facing, so every resource is bounded: connections (total, pending, per peer),
//! relay reservations and circuits, and the DHT record store.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, anyhow};
use futures::StreamExt as _;
use libp2p::connection_limits::{self, ConnectionLimits};
use libp2p::identity::Keypair;
use libp2p::kad::store::{MemoryStore, MemoryStoreConfig};
use libp2p::swarm::behaviour::toggle::Toggle;
use libp2p::swarm::{NetworkBehaviour, SwarmEvent};
use libp2p::{Multiaddr, PeerId, StreamProtocol, Swarm, identify, kad, noise, ping, relay, tcp, yamux};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, watch};
use tracing::{debug, info, warn};

/// Must equal `systema_node::KAD_PROTOCOL` / `IDENTIFY_PROTOCOL` (a test pins it).
pub const KAD_PROTOCOL: StreamProtocol = StreamProtocol::new("/systema/kad/1.0.0");
pub const IDENTIFY_PROTOCOL: &str = "/systema/id/1.0.0";

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
    // The same defaults a `systema node` relay uses.
    fn default() -> Self {
        Self { max_reservations: 128, max_circuits: 32, max_circuits_per_peer: 4, max_circuit_secs: 900, max_circuit_bytes: 4 << 20 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Limits {
    /// Established inbound connections, all peers together.
    pub max_established_incoming: u32,
    /// Handshakes in flight; the cheapest flood to mount.
    pub max_pending_incoming: u32,
    pub max_established_per_peer: u32,
    /// DHT: provider records kept, per key and overall.
    pub max_providers_per_key: usize,
    pub max_provided_keys: usize,
    pub max_records: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_established_incoming: 512,
            max_pending_incoming: 64,
            max_established_per_peer: 4,
            max_providers_per_key: 20,
            max_provided_keys: 4096,
            max_records: 1024,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HubConfig {
    pub listen: Vec<String>,
    /// Public addresses to advertise (a relay must set these, or reservations carry none).
    pub external_addrs: Vec<String>,
    /// Other hubs, `/…/p2p/<id>`: gives a fleet of hubs a shared routing table.
    pub bootstrap: Vec<String>,
    /// Serve as a circuit relay.
    pub relay: bool,
    pub relay_limits: RelayLimits,
    pub limits: Limits,
}

impl Default for HubConfig {
    fn default() -> Self {
        Self {
            listen: vec!["/ip4/0.0.0.0/tcp/4001".into(), "/ip4/0.0.0.0/udp/4001/quic-v1".into()],
            external_addrs: Vec::new(),
            bootstrap: Vec::new(),
            relay: true,
            relay_limits: RelayLimits::default(),
            limits: Limits::default(),
        }
    }
}

impl HubConfig {
    /// `<home>/hub.toml`; a missing file means defaults.
    pub fn load(home: &Path) -> anyhow::Result<Self> {
        let path = home.join("hub.toml");
        match std::fs::read_to_string(&path) {
            Ok(text) if !text.trim().is_empty() => toml::from_str(&text).with_context(|| format!("parse {}", path.display())),
            Ok(_) => Ok(Self::default()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
        }
    }
}

/// `<home>/identity.key`: a protobuf-encoded ed25519 key, the format `systema init` writes.
/// Created (mode 0600) when missing, never overwritten.
pub fn ensure_identity(home: &Path) -> anyhow::Result<Keypair> {
    use std::io::Write as _;
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
    std::fs::create_dir_all(home).with_context(|| format!("create {}", home.display()))?;
    // Best effort: on a mounted volume the directory may belong to another uid (fsGroup access).
    let _ = std::fs::set_permissions(home, std::fs::Permissions::from_mode(0o700));
    let path: PathBuf = home.join("identity.key");
    match std::fs::read(&path) {
        Ok(bytes) => Keypair::from_protobuf_encoding(&bytes).map_err(|e| anyhow!("corrupt identity key {}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let kp = Keypair::generate_ed25519();
            let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path)?;
            f.write_all(&kp.to_protobuf_encoding()?)?;
            Ok(kp)
        }
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

#[derive(NetworkBehaviour)]
struct Behaviour {
    limits: connection_limits::Behaviour,
    relay: Toggle<relay::Behaviour>,
    identify: identify::Behaviour,
    ping: ping::Behaviour,
    kad: kad::Behaviour<MemoryStore>,
}

pub struct Hub {
    swarm: Swarm<Behaviour>,
    cfg: HubConfig,
}

impl Hub {
    pub fn new(keypair: Keypair, cfg: HubConfig) -> anyhow::Result<Self> {
        let relay_on = cfg.relay;
        let rl = cfg.relay_limits.clone();
        let lim = cfg.limits.clone();
        let swarm = libp2p::SwarmBuilder::with_existing_identity(keypair)
            .with_tokio()
            .with_tcp(tcp::Config::default(), noise::Config::new, yamux::Config::default)?
            .with_quic()
            .with_dns()?
            .with_behaviour(|key| {
                let peer_id = key.public().to_peer_id();
                let mut kad_cfg = kad::Config::new(KAD_PROTOCOL);
                kad_cfg.set_query_timeout(Duration::from_secs(30));
                let store = MemoryStore::with_config(
                    peer_id,
                    MemoryStoreConfig {
                        max_records: lim.max_records,
                        max_value_bytes: 64 * 1024,
                        max_providers_per_key: lim.max_providers_per_key,
                        max_provided_keys: lim.max_provided_keys,
                    },
                );
                let relay = relay_on.then(|| {
                    relay::Behaviour::new(
                        peer_id,
                        relay::Config {
                            max_reservations: rl.max_reservations,
                            max_circuits: rl.max_circuits,
                            max_circuits_per_peer: rl.max_circuits_per_peer,
                            max_circuit_duration: Duration::from_secs(rl.max_circuit_secs),
                            max_circuit_bytes: rl.max_circuit_bytes,
                            ..Default::default()
                        },
                    )
                });
                Ok(Behaviour {
                    limits: connection_limits::Behaviour::new(
                        ConnectionLimits::default()
                            .with_max_established_incoming(Some(lim.max_established_incoming))
                            .with_max_pending_incoming(Some(lim.max_pending_incoming))
                            .with_max_established_per_peer(Some(lim.max_established_per_peer)),
                    ),
                    relay: Toggle::from(relay),
                    identify: identify::Behaviour::new(identify::Config::new(IDENTIFY_PROTOCOL.into(), key.public())),
                    ping: ping::Behaviour::default(),
                    kad: kad::Behaviour::with_config(peer_id, store, kad_cfg),
                })
            })?
            .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(60)))
            .build();
        let mut hub = Self { swarm, cfg };
        // A hub is a DHT server whether or not it has confirmed an external address yet.
        hub.swarm.behaviour_mut().kad.set_mode(Some(kad::Mode::Server));
        Ok(hub)
    }

    pub fn peer_id(&self) -> PeerId {
        *self.swarm.local_peer_id()
    }

    /// Serve until `shutdown` turns true. Every address the hub starts listening on is sent to
    /// `ready` (tests use it to learn an ephemeral port).
    pub async fn run(mut self, mut shutdown: watch::Receiver<bool>, ready: Option<mpsc::UnboundedSender<Multiaddr>>) -> anyhow::Result<()> {
        for l in &self.cfg.listen {
            self.swarm.listen_on(l.parse().with_context(|| format!("bad listen address {l}"))?)?;
        }
        for e in &self.cfg.external_addrs {
            self.swarm.add_external_address(e.parse().with_context(|| format!("bad external address {e}"))?);
        }
        for b in &self.cfg.bootstrap {
            let addr: Multiaddr = b.parse().with_context(|| format!("bad bootstrap address {b}"))?;
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
        if self.cfg.relay {
            info!("acting as a circuit relay");
        }
        let mut refresh = tokio::time::interval(Duration::from_secs(300));
        refresh.tick().await;
        loop {
            tokio::select! {
                _ = shutdown.changed() => if *shutdown.borrow() { break },
                _ = refresh.tick() => if !self.cfg.bootstrap.is_empty() { let _ = self.swarm.behaviour_mut().kad.bootstrap(); },
                event = self.swarm.select_next_some() => self.on_event(event, ready.as_ref()),
            }
        }
        Ok(())
    }

    fn on_event(&mut self, event: SwarmEvent<BehaviourEvent>, ready: Option<&mpsc::UnboundedSender<Multiaddr>>) {
        match event {
            SwarmEvent::NewListenAddr { address, .. } => {
                info!(%address, "listening");
                if let Some(tx) = ready {
                    let _ = tx.send(address);
                }
            }
            // Like a node: learn a peer's addresses for the routing table, but only from peers
            // that speak the DHT protocol.
            SwarmEvent::Behaviour(BehaviourEvent::Identify(identify::Event::Received { peer_id, info, .. })) => {
                if info.protocols.contains(&KAD_PROTOCOL) {
                    for a in info.listen_addrs {
                        self.swarm.behaviour_mut().kad.add_address(&peer_id, a);
                    }
                }
            }
            SwarmEvent::Behaviour(BehaviourEvent::Relay(e)) => match e {
                relay::Event::ReservationReqAccepted { src_peer_id, .. } => info!(peer = %src_peer_id, "relay: reservation accepted"),
                relay::Event::CircuitReqAccepted { src_peer_id, dst_peer_id } => {
                    info!(src = %src_peer_id, dst = %dst_peer_id, "relay: circuit opened")
                }
                relay::Event::CircuitClosed { src_peer_id, dst_peer_id, error } => {
                    debug!(src = %src_peer_id, dst = %dst_peer_id, ?error, "relay: circuit closed")
                }
                other => debug!(?other, "relay event"),
            },
            SwarmEvent::Behaviour(BehaviourEvent::Kad(kad::Event::RoutingUpdated { peer, is_new_peer, .. })) if is_new_peer => {
                debug!(%peer, "dht: peer added");
            }
            SwarmEvent::Behaviour(BehaviourEvent::Kad(kad::Event::InboundRequest { request })) => debug!(?request, "dht: inbound request"),
            SwarmEvent::IncomingConnectionError { error, .. } => debug!(%error, "incoming connection refused"),
            _ => {}
        }
    }
}
