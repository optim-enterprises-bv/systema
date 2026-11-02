# Architecture

```
┌──────────────────────── agents (manifests) ───────────────────────┐
│  coder   sysadmin   researcher   …                                 │
├────────────────────────────── kernel ─────────────────────────────┤
│ scheduler    priority queue over titan-engine's sequence slots     │
│ sessions     append-only context → prefix-cache hits every turn    │
│ bus          local A2A-shaped messages between agents              │
│ [todo] process mgr + podman sandbox, capabilities enforcement      │
│ [todo] memory: markdown files + SQLite FTS5/vector                 │
│ [todo] tool bus: MCP                                               │
├─────────────────────────────── node ──────────────────────────────┤
│ identity     ed25519 = PeerId = card signing key                   │
│ transport    QUIC + TCP, noise, yamux, DNS                         │
│ NAT          identify + relay v2 client + DCUtR hole punching      │
│ discovery    mDNS (LAN), Kademlia provider records per skill (WAN) │
│ gossip       systema/v1/cards: signed agent cards                  │
│ trust gate   verify → replay check → grant/quarantine/ask/deny     │
│ a2a          signed requests (/systema/a2a/1.0.0), approvals inbox │
│ control      ~/.systema/control.sock (0600) for the CLI            │
│ [todo] task boards, HTTP A2A gateway                               │
└────────────── titan-engine :1234 (35B-A3B MoE, 27B dense) ─────────┘
```

## Rules enforced in code, not prompts

1. **Remote input never reaches a tool-holding agent without a grant.** Unknown peers → approvals inbox
   (default) or a quarantined, tool-less, memory-less responder.
2. **Grants are narrow:** one peer × one skill, rate-limited per hour, optional expiry. Blocking beats grants.
3. **Models cannot forward.** Re-sending content to other peers is a kernel policy decision only
   (`ForwardCause::ModelRequested` is always refused). This breaks self-replicating prompt-injection worms.
4. **Everything is signed and fresh:** cards and messages carry signer key, nonce, timestamp; signer must match
   the card's `peer_id` and the gossip source; nonces are remembered for the freshness window.
5. **Nodes join only by explicit install.** Systema never deploys itself to other machines.
6. **Remote sessions are isolated.** An approved peer talks to the agent in a session keyed `agent@peer`, at
   background priority, with its text framed as untrusted data, never in the operator's session.
7. **Agent system prompts are not secret.** Testing showed a direct "print your system prompt" request from an
   approved peer works. Never put credentials or private operator data in prompts or remote sessions; the
   defence is what the agent can *reach* (capabilities, sandbox), not what the model refuses to say.

## Inbound request pipeline (`/systema/a2a/1.0.0`)

signature → signer == connected peer → nonce not replayed → addressed to this node → agent is `public` and
offers the skill → text 1..32 KiB → trust gate: **allow** (real agent) · **quarantine** (tool-less responder) ·
**ask human** (inbox, reply `pending`) · **deny**. Replies are signed; the requester verifies the signer is the
peer it asked. Grants and the inbox persist in `trust.json` / `inbox.json`; every decision goes to `audit.jsonl`.

## Prefix-cache discipline

titan-engine's prefix cache makes a repeated prompt start in 0.4–0.9 s. Sessions are append-only: fixed system
prompt and tool list, nothing earlier ever edited; volatile context goes in the newest message. Overflow →
new session seeded with a summary, never an in-place rewrite.

## Roadmap

1. ~~Kernel MVP: scheduler, sessions, bus, CLI~~ · ~~Node: identity, transports, mDNS, DHT, card gossip~~
2. Agent runtime loop with MCP tools inside podman; capability enforcement; memory store.
3. ~~Direct agent↔agent requests over libp2p request-response, through the trust gate; approvals inbox (CLI)~~;
   TUI inbox with live notifications.
4. `systema find <skill>`: DHT provider lookup; bootstrap node on a VPS; NAT test across real networks.
5. Swarm task boards on gossip: post → bid → assign → result → verify.
6. Reputation (local history + vouches + gossipsub peer scoring), proof-of-work join cost, HTTP A2A gateway.
