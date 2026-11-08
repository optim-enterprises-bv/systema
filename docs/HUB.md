# systema-hub: the public bootstrap and relay

`systema-hub` is a small binary for the one machine that has to be reachable from the internet.
It does two jobs and nothing else:

- **Kademlia DHT server.** Nodes announce the skills they offer as provider records; other nodes
  look a skill up and get back who offers it.
- **Circuit relay v2 server.** A node behind NAT holds a reservation on the hub, and callers reach it
  through the hub. DCUtR then tries to upgrade that path to a direct connection.

It has no agents, no kernel, no trust or audit state, no gossip, no request handler and no model
client. A stranger who connects cannot run, approve or read anything on it. Its only file on disk is
`identity.key` (same format as `systema init`), so the peer id survives restarts. It speaks the exact
protocol names a `systema node` does (`/systema/kad/1.0.0`, `/systema/id/1.0.0`); a test pins that.

Use it instead of a full node whenever the machine is public. A full node on a public address carries
agents and state you would then have to defend.

## Run it

```sh
systema-hub --home /var/lib/systema-hub \
  --external /dns4/hub.example.com/tcp/4001 \
  --external /dns4/hub.example.com/udp/4001/quic-v1
```

`--print-peer-id` prints the peer id and exits. `--external` (repeatable) is the public address the
hub advertises; **set it**, or relay reservations carry no usable address. `--listen` and `--bootstrap`
are repeatable too, and `--no-relay` turns the relay off. `SYSTEMA_HUB_HOME` sets the state directory.

A container image is built from [`deploy/hub/Containerfile`](../deploy/hub/Containerfile) (distroless,
non-root, about 40 MB), and [`deploy/hub/k8s.yaml`](../deploy/hub/k8s.yaml) is a Kubernetes example: a
Deployment with a small volume for the identity key, and a `LoadBalancer` for TCP and UDP 4001.
Forward both protocols from your router or firewall, and add the matching firewall pass rule if your
firewall needs one for forwarded ports.

## Configuration (`<home>/hub.toml`)

Everything is optional; flags override the file.

```toml
listen = ["/ip4/0.0.0.0/tcp/4001", "/ip4/0.0.0.0/udp/4001/quic-v1"]
external_addrs = []   # public addresses to advertise
bootstrap = []        # other hubs, "/…/p2p/<id>", to share a routing table
relay = true

[relay_limits]
max_reservations = 128
max_circuits = 32
max_circuits_per_peer = 4
max_circuit_secs = 900
max_circuit_bytes = 4194304

[limits]
max_established_incoming = 512
max_pending_incoming = 64     # handshakes in flight: the cheapest flood to mount
max_established_per_peer = 4
max_providers_per_key = 20
max_provided_keys = 4096
max_records = 1024
```

The DHT store and the relay are both bounded, so a flood costs the hub a fixed amount.

## Point nodes at it

On each node, set the hub as bootstrap and relay. A node behind NAT needs no listeners at all:

```toml
[node]
listen = []
bootstrap = ["/dns4/hub.example.com/tcp/4001/p2p/<hub-peer-id>"]
relays    = ["/dns4/hub.example.com/tcp/4001/p2p/<hub-peer-id>"]
dht_server = "off"
mdns = false
```

The node then holds a relay reservation, announces its public agents' skills in the DHT, and answers
requests through the hub. First contact from an unknown peer still lands in the inbox as pending until
you approve it. `scripts/e2e-hub.sh` runs this end to end on loopback with two real nodes.

## What it does not do

It does not prove hole punching, which needs two nodes behind different real NATs
([runbook](CROSS-NAT.md)). Run several hubs and list each as another's `bootstrap` for redundancy; a
single hub is a single point of discovery.
