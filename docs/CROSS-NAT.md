# Cross-NAT hole-punching (DCUtR): how to test it for real

Systema nodes behind NAT reach each other without port-forwarding using the same
stack as libp2p and IPFS:

1. **Relay v2** — a public node (a VPS) relays a connection between two NAT'd peers.
2. **Identify** — each peer learns its own public (post-NAT) address as observed by others.
3. **DCUtR hole punching** — the two peers use the relayed connection to coordinate a
   simultaneous dial, punching through both NATs to a **direct** connection. The relay is
   then only needed for signalling, not for data.

This can only be validated with **two nodes behind different real NATs and one public
relay** — it cannot be reproduced on loopback, where peers already reach each other
directly, so there is no relayed connection to upgrade. (The relay *data path* on its own
is covered by `scripts/e2e-relay.sh`, which runs locally.)

## What you need

- **RELAY**: a cheap VPS with a public IPv4 and one open inbound TCP port (default `47401`).
- **A** and **B**: two machines on **different** networks behind NAT — e.g. home Wi-Fi and a
  phone hotspot. Two machines behind the *same* router share one NAT and won't exercise a
  real two-sided punch.
- SSH access (key-based, `BatchMode`) from where you run the script to all three hosts.

## Run it

```sh
cargo build --release   # the script scp's this binary to each host

RELAY=user@vps.example.com RELAY_IP=203.0.113.9 \
A=you@home-box B=you@hotspot-box \
./scripts/holepunch-cross-nat.sh
```

The script: deploys the binary, configures R as a public relay + DHT server, points A and B
at R, starts all three, waits for B to reserve a relay slot, then has A find B's skill over
the DHT and send a request through the relay. That opens the circuit that DCUtR tries to
upgrade.

Useful flags: `SKIP_DEPLOY=1` (binary already present, path in `BIN`), `KEEP=1` (leave nodes
running to inspect), `RELAY_PORT=...`.

## What success looks like

Both A and B log a DCUtR event with an **`Ok`** result:

```
hole punch  Event { remote_peer_id: 12D3Koo…, result: Ok(ConnectionId(3)) }
```

and the relay shows a short-lived circuit that is no longer carrying data afterward. The
script exits `0` only when it sees that `Ok`.

## If the punch fails (but the relay path works)

A `result: Err(...)` with the request still succeeding means data is flowing **through the
relay** — connectivity is fine, but no direct connection was established. The usual cause is
a **symmetric NAT** (it maps each destination to a different external port, so the address
each side predicts for the other is wrong). This is a property of the network, not of
Systema. Options:

- Put A or B on a different network (many home routers are endpoint-independent / full-cone
  and punch fine; carrier-grade and corporate NATs often don't).
- Keep using the relay as the data path — it works, it's just not peer-to-peer-direct.
- (Future) add a TURN-style always-relay fallback and QUIC/UDP hole punching, which succeeds
  across more NAT types than TCP.

## Notes

- The relay carries **encrypted** traffic it cannot read, and only for peers that reserved a
  slot; it is not an open proxy.
- Nodes never auto-deploy. This script uses your SSH access to set up a disposable test in
  `/tmp/systema-*` on each host; `KEEP=1` aside, it kills the nodes on exit (it does not
  remove the temp homes — delete `/tmp/systema-*` yourself if you want them gone).
