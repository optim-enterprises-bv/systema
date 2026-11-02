# Systema Agentium

A local agentic OS. Agents run as managed processes over one shared GPU (via
[titan-engine](../src/titan-engine)), and find and message agents on other Systema nodes
across the internet, forming a swarm.

> *systema agentium*: Latin, "a system of agents".

## Status

Scaffold, phase 1–2. Works today:

- `systema init`: creates `~/.systema` with an ed25519 node identity (`0600`), config and example agents.
- `systema ask <agent> <prompt>`: one turn through the kernel: append-only session → priority GPU scheduler → titan-engine.
- `systema node`: libp2p node (QUIC+TCP, relay client + DCUtR, mDNS, Kademlia, GossipSub) that announces
  signed agent cards and records verified cards from peers.
- **Agent ↔ agent requests across nodes** (`/systema/a2a/1.0.0`): `systema send <peer> <agent> --skill <id> <text>`.
  Unknown peers land in your **approvals inbox**; approved peers get rate-limited, per-skill grants; replies are
  signed by the answering node and verified.
- `systema peers | inbox | approve | reject | block | grants`: drive the running node over a private Unix socket.
- `systema status | agents | cards | id`. Every inbound decision is logged to `~/.systema/audit.jsonl`.

Not yet: tools/MCP, podman sandbox, memory, DHT skill lookup across the internet, task boards, HTTP A2A gateway,
reputation. See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the roadmap.

## Quick start

```sh
cargo build --release
./target/release/systema init
./target/release/systema status            # is titan-engine up on :1234?
./target/release/systema ask coder "explain this regex: ^a+b?$"
./target/release/systema node              # join the swarm (LAN via mDNS; add bootstrap peers in systema.toml)
```

Two nodes on one machine (or run `scripts/e2e-two-nodes.sh` for the whole flow):

```sh
systema --home /tmp/a init && systema --home /tmp/b init
systema --home /tmp/a node &  systema --home /tmp/b node &
systema --home /tmp/a peers                                   # B's coder, skill code-review
systema --home /tmp/a send <B-peer-id> coder --skill code-review "fn add(a:i32,b:i32)->i32{a-b}"
# → pending: held in the remote inbox as 84fae4b0
systema --home /tmp/b inbox
systema --home /tmp/b approve 84fae4b0 --per-hour 10
systema --home /tmp/a send <B-peer-id> coder --skill code-review "fn add(a:i32,b:i32)->i32{a-b}"
# → "The function is named `add` but performs subtraction…"
```

## Layout

| crate | role |
|---|---|
| `systema-proto` | A2A-shaped `AgentCard` / `Message`, `Signed<T>` envelopes (ed25519, nonce, timestamp, domain-separated) |
| `systema-trust` | trust gate: grants per peer×skill, rate limits, expiry, blocklist, replay guard, no model-initiated forwarding |
| `systema-kernel` | config, agent manifests, priority GPU scheduler, append-only sessions, local bus, titan-engine client |
| `systema-node` | libp2p swarm: discovery, NAT traversal, card gossip |
| `systema-cli` | the `systema` binary |

Agent manifests live in `~/.systema/agents/*.toml` (examples in [`agents/`](agents/)).
