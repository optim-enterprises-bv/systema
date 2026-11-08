//! The hub against real libp2p clients on loopback.

use std::os::unix::fs::PermissionsExt as _;
use std::time::Duration;

use futures::StreamExt as _;
use libp2p::kad::store::MemoryStore;
use libp2p::multiaddr::Protocol;
use libp2p::swarm::{NetworkBehaviour, SwarmEvent};
use libp2p::{Multiaddr, PeerId, Swarm, identify, kad, noise, ping, relay, tcp, yamux};
use systema_hub::{Hub, HubConfig, IDENTIFY_PROTOCOL, KAD_PROTOCOL, ensure_identity};
use tokio::sync::{mpsc, watch};

const WAIT: Duration = Duration::from_secs(20);

#[test]
fn the_hub_speaks_the_exact_protocols_of_a_real_node() {
    assert_eq!(KAD_PROTOCOL, systema_node::KAD_PROTOCOL);
    assert_eq!(IDENTIFY_PROTOCOL, systema_node::IDENTIFY_PROTOCOL);
}

#[test]
fn identity_is_created_private_and_then_kept() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("hub");
    let a = ensure_identity(&home).unwrap().public().to_peer_id();
    let b = ensure_identity(&home).unwrap().public().to_peer_id();
    assert_eq!(a, b, "the peer id everyone bootstraps to must not change");
    let mode = std::fs::metadata(home.join("identity.key")).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    assert_eq!(std::fs::metadata(&home).unwrap().permissions().mode() & 0o777, 0o700);
}

#[test]
fn a_corrupt_identity_is_an_error_not_a_new_identity() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("identity.key"), b"not a key").unwrap();
    assert!(ensure_identity(dir.path()).is_err());
    assert_eq!(std::fs::read(dir.path().join("identity.key")).unwrap(), b"not a key", "must not be overwritten");
}

#[test]
fn config_defaults_and_partial_files() {
    let d = HubConfig::default();
    assert!(d.relay && d.listen.len() == 2);
    assert_eq!(d.relay_limits.max_circuits_per_peer, 4);
    let dir = tempfile::tempdir().unwrap();
    assert!(HubConfig::load(dir.path()).unwrap().relay, "no file means defaults");
    std::fs::write(dir.path().join("hub.toml"), "relay = false\n[limits]\nmax_pending_incoming = 7\n").unwrap();
    let c = HubConfig::load(dir.path()).unwrap();
    assert!(!c.relay);
    assert_eq!(c.limits.max_pending_incoming, 7);
    assert_eq!(c.limits.max_established_incoming, 512, "the rest keeps its default");
    std::fs::write(dir.path().join("hub.toml"), "listen = 5").unwrap();
    assert!(HubConfig::load(dir.path()).is_err());
}

// ── helpers ──────────────────────────────────────────────────────────────

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

struct Running {
    peer: PeerId,
    addr: Multiaddr,
    stop: watch::Sender<bool>,
}

impl Running {
    /// `/ip4/127.0.0.1/tcp/<port>/p2p/<hub>`
    fn full(&self) -> Multiaddr {
        self.addr.clone().with(Protocol::P2p(self.peer))
    }
}

async fn start_hub(mut cfg: HubConfig) -> Running {
    let port = free_port();
    let addr: Multiaddr = format!("/ip4/127.0.0.1/tcp/{port}").parse().unwrap();
    cfg.listen = vec![addr.to_string()];
    cfg.external_addrs = vec![addr.to_string()];
    let dir = tempfile::tempdir().unwrap();
    let kp = ensure_identity(dir.path()).unwrap();
    let hub = Hub::new(kp, cfg).unwrap();
    let peer = hub.peer_id();
    let (stop, rx) = watch::channel(false);
    let (ready_tx, mut ready_rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let _keep = dir;
        hub.run(rx, Some(ready_tx)).await.unwrap();
    });
    tokio::time::timeout(WAIT, ready_rx.recv()).await.expect("hub listening").unwrap();
    Running { peer, addr, stop }
}

#[derive(NetworkBehaviour)]
struct Client {
    relay_client: relay::client::Behaviour,
    identify: identify::Behaviour,
    ping: ping::Behaviour,
    kad: kad::Behaviour<MemoryStore>,
}

fn client() -> Swarm<Client> {
    client_in(kad::Mode::Client)
}

fn client_in(mode: kad::Mode) -> Swarm<Client> {
    let mut swarm = libp2p::SwarmBuilder::with_new_identity()
        .with_tokio()
        .with_tcp(tcp::Config::default(), noise::Config::new, yamux::Config::default)
        .unwrap()
        .with_relay_client(noise::Config::new, yamux::Config::default)
        .unwrap()
        .with_behaviour(|key, relay_client| {
            let id = key.public().to_peer_id();
            Ok(Client {
                relay_client,
                identify: identify::Behaviour::new(identify::Config::new(IDENTIFY_PROTOCOL.into(), key.public())),
                ping: ping::Behaviour::default(),
                kad: kad::Behaviour::with_config(id, MemoryStore::new(id), kad::Config::new(KAD_PROTOCOL)),
            })
        })
        .unwrap()
        .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(30)))
        .build();
    swarm.behaviour_mut().kad.set_mode(Some(mode));
    swarm
}

/// Poll `swarm` until `pick` returns something.
async fn until<T>(swarm: &mut Swarm<Client>, what: &str, mut pick: impl FnMut(&SwarmEvent<ClientEvent>) -> Option<T>) -> T {
    tokio::time::timeout(WAIT, async {
        loop {
            let ev = swarm.select_next_some().await;
            if let Some(v) = pick(&ev) {
                return v;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

fn connect_to_hub(swarm: &mut Swarm<Client>, hub: &Running) {
    swarm.behaviour_mut().kad.add_address(&hub.peer, hub.addr.clone());
    swarm.dial(hub.full()).unwrap();
}

// ── behaviour ────────────────────────────────────────────────────────────

async fn skill_roundtrip(mode: kad::Mode) {
    let hub = start_hub(HubConfig::default()).await;
    let key = kad::RecordKey::new(&"skill/rust-review");

    // Provider B publishes; the record must live on the hub, not depend on B answering.
    let mut b = client_in(mode);
    let b_peer = *b.local_peer_id();
    b.listen_on("/ip4/127.0.0.1/tcp/0".parse().unwrap()).unwrap();
    let b_addr = until(&mut b, "B listening", |e| match e {
        SwarmEvent::NewListenAddr { address, .. } => Some(address.clone()),
        _ => None,
    })
    .await;
    b.add_external_address(b_addr);
    connect_to_hub(&mut b, &hub);
    until(&mut b, "B connected", |e| matches!(e, SwarmEvent::ConnectionEstablished { .. }).then_some(())).await;
    b.behaviour_mut().kad.start_providing(key.clone()).unwrap();
    until(&mut b, "StartProviding", |e| match e {
        SwarmEvent::Behaviour(ClientEvent::Kad(kad::Event::OutboundQueryProgressed { result: kad::QueryResult::StartProviding(r), .. })) => {
            Some(r.is_ok())
        }
        _ => None,
    })
    .await
    .then_some(())
    .expect("the hub accepted the provider record");
    // A real provider stays online. ADD_PROVIDER is fire-and-forget: dropping B right after the
    // query reports success can discard the message before it reaches the hub.
    tokio::spawn(async move {
        loop {
            b.select_next_some().await;
        }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;

    // A is a stranger that was told only the hub's address.
    let mut a = client_in(mode);
    connect_to_hub(&mut a, &hub);
    until(&mut a, "A connected", |e| matches!(e, SwarmEvent::ConnectionEstablished { .. }).then_some(())).await;
    a.behaviour_mut().kad.get_providers(key);
    let mut found = std::collections::HashSet::new();
    until(&mut a, "lookup to finish", |e| match e {
        SwarmEvent::Behaviour(ClientEvent::Kad(kad::Event::OutboundQueryProgressed { result: kad::QueryResult::GetProviders(r), step, .. })) => {
            if let Ok(kad::GetProvidersOk::FoundProviders { providers, .. }) = r {
                found.extend(providers.iter().copied());
            }
            step.last.then_some(())
        }
        _ => None,
    })
    .await;
    assert!(found.contains(&b_peer), "A found {found:?}, expected {b_peer}");
    let _ = hub.stop.send(true);
}

#[tokio::test]
async fn a_skill_published_by_a_dht_server_is_found_by_a_node_that_only_knows_the_hub() {
    skill_roundtrip(kad::Mode::Server).await;
}

/// A node behind NAT runs with the DHT off (client mode) and must still be discoverable.
#[tokio::test]
async fn a_skill_published_by_a_dht_client_is_found_too() {
    skill_roundtrip(kad::Mode::Client).await;
}

#[tokio::test]
async fn a_node_with_no_listeners_is_reachable_through_the_hub_relay() {
    let hub = start_hub(HubConfig::default()).await;

    // B is "behind NAT": no listeners, only a reservation on the hub.
    let mut b = client();
    let b_peer = *b.local_peer_id();
    connect_to_hub(&mut b, &hub);
    until(&mut b, "B connected to the relay", |e| matches!(e, SwarmEvent::ConnectionEstablished { .. }).then_some(())).await;
    b.listen_on(hub.full().with(Protocol::P2pCircuit)).unwrap();
    until(&mut b, "relay reservation", |e| match e {
        SwarmEvent::Behaviour(ClientEvent::RelayClient(relay::client::Event::ReservationReqAccepted { .. })) => Some(()),
        _ => None,
    })
    .await;
    // B must keep being polled to accept the incoming circuit.
    let (seen_tx, mut seen_rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            if let SwarmEvent::ConnectionEstablished { peer_id, endpoint, .. } = b.select_next_some().await {
                let _ = seen_tx.send((peer_id, endpoint.get_remote_address().clone()));
            }
        }
    });

    // A knows only the hub and B's peer id, and dials B through the circuit.
    let mut a = client();
    connect_to_hub(&mut a, &hub);
    until(&mut a, "A connected", |e| matches!(e, SwarmEvent::ConnectionEstablished { .. }).then_some(())).await;
    a.dial(hub.full().with(Protocol::P2pCircuit).with(Protocol::P2p(b_peer))).unwrap();
    let via_circuit = until(&mut a, "circuit to B", |e| match e {
        SwarmEvent::ConnectionEstablished { peer_id, endpoint, .. } if *peer_id == b_peer => {
            Some(endpoint.get_remote_address().iter().any(|p| matches!(p, Protocol::P2pCircuit)))
        }
        _ => None,
    })
    .await;
    assert!(via_circuit, "A reached B, and it was through the relay");
    let (peer, _) = tokio::time::timeout(WAIT, seen_rx.recv()).await.unwrap().unwrap();
    assert!(peer == hub.peer || peer != b_peer, "B saw an inbound connection");
    let _ = hub.stop.send(true);
}

#[tokio::test]
async fn with_relay_off_a_reservation_is_refused() {
    let hub = start_hub(HubConfig { relay: false, ..HubConfig::default() }).await;
    let mut b = client();
    connect_to_hub(&mut b, &hub);
    until(&mut b, "B connected", |e| matches!(e, SwarmEvent::ConnectionEstablished { .. }).then_some(())).await;
    b.listen_on(hub.full().with(Protocol::P2pCircuit)).unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            match b.select_next_some().await {
                SwarmEvent::Behaviour(ClientEvent::RelayClient(relay::client::Event::ReservationReqAccepted { .. })) => return true,
                SwarmEvent::ListenerClosed { .. } | SwarmEvent::ListenerError { .. } => return false,
                _ => {}
            }
        }
    })
    .await;
    assert_ne!(outcome, Ok(true), "a hub started with relay=false must not relay");
    let _ = hub.stop.send(true);
}

#[tokio::test]
async fn connections_beyond_the_limit_are_not_kept() {
    let mut cfg = HubConfig::default();
    cfg.limits.max_established_incoming = 1;
    let hub = start_hub(cfg).await;

    let mut first = client();
    connect_to_hub(&mut first, &hub);
    until(&mut first, "first connected", |e| matches!(e, SwarmEvent::ConnectionEstablished { .. }).then_some(())).await;
    // Keep the first connection alive.
    tokio::spawn(async move {
        loop {
            first.select_next_some().await;
        }
    });

    let mut second = client();
    connect_to_hub(&mut second, &hub);
    let kept = tokio::time::timeout(Duration::from_secs(10), async {
        let mut established = false;
        loop {
            match second.select_next_some().await {
                SwarmEvent::ConnectionEstablished { .. } => established = true,
                SwarmEvent::ConnectionClosed { .. } | SwarmEvent::OutgoingConnectionError { .. } => return false,
                _ => {
                    let _ = established;
                }
            }
        }
    })
    .await;
    assert_eq!(kept, Ok(false), "the second connection must be refused or closed by the hub");
    let _ = hub.stop.send(true);
}
