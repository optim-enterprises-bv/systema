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

## Security model: broker the access, sandbox the untrusted input

An agentic OS needs agents with real power over the machine. The danger is not power itself but the
**lethal trifecta** (Simon Willison): one session that has (1) private data, (2) untrusted content in its context,
and (3) a way to act or communicate outward. Prompt injection turns that combination into exfiltration or
takeover, and no model or guardrail reliably refuses. Meta's **Agents Rule of Two** states the fix: at most two
of the three per session without a human decision.

Systema applies it in the kernel:

- **Capability broker.** Agents never touch the host directly. Every tool call (read, list, write, run, fetch)
  is a request to the kernel, which checks it against the agent's manifest, executes it on the agent's behalf
  and audits it. The manifest is the ceiling; nothing outside it is ever granted.
- **Hard denials** regardless of manifest: the Systema home (identity key, grants, control socket, audit
  log; agents can't read or edit their own policy), `~/.ssh`, `~/.gnupg`, keyrings, browser profiles,
  `/etc/shadow` and similar. Paths are canonicalized first, so `..` and symlinks can't escape.
- **Per-session taint (the Rule of Two).** Each session tracks which legs it holds:
  - *untrusted*: it read a remote peer's message or fetched content (a fetch taints the session);
  - *private*: it read outside its workspace, or ran a program;
  - *outbound*: it fetched a URL, ran a program, or its replies go to a remote peer.
  A request that would make a session hold all three is escalated to **ask the operator**. Interactively that's
  a y/N prompt; on the node, with no human present, it is **denied**. Remote sessions start untrusted *and*
  outbound (the reply goes to the stranger), so a stranger can never get your private data back through an
  agent without you approving it.
- **Workspaces.** Each agent has `~/.systema/workspaces/<agent>/`, always readable and writable, not private.
- **Execution without a shell.** `run` executes one program with an argument vector (no shell), with a scrubbed
  environment (no credentials), cwd set to the workspace, a timeout and an output cap. Each manifest
  `exec` grant names a program and an argument prefix, with mode `allow` or `ask`.
- **Fetch guards.** http(s) only; loopback, private, link-local and CGNAT addresses are refused, so a fetch can't
  reach titan-engine, the control socket's neighbours or your LAN.

- **MCP servers behind the broker.** The operator configures servers in `systema.toml` and declares which
  trifecta legs each touches (`untrusted`, `private`, `outbound`; **default: all three**, so an unclassified
  server always needs approval). Agents opt in with `capabilities.mcp`; tools appear as
  `mcp__<server>__<tool>`; every call gets the same allowlist / `ask_tools` / Rule of Two / audit treatment.
  Servers run as the user with a scrubbed environment (only the variables you list): the broker governs which
  calls are made, not what a server does internally, so install only servers you trust.

This is session-level taint, a conservative approximation of **CaMeL** (Google DeepMind, *Defeating Prompt
Injections by Design*): CaMeL tracks provenance per value through a planner/interpreter split (a hardened
**dual-LLM pattern**), so it can allow more without risk. The planned step there is a quarantined reader whose
output reaches privileged agents only as opaque references.

**Sandboxing** (microVM via libkrun) is reserved for code that processes strangers' input: public agents and
programs they write and run. Operator-only agents get host access through the broker.

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
2. ~~Capability broker, tool-calling agent loop, per-session taint (Rule of Two), MCP servers behind the broker~~;
   libkrun microVMs for public agents (and optionally MCP servers); persistent "always allow" approvals; memory store; quarantined reader with opaque references (CaMeL-style).
3. ~~Direct agent↔agent requests over libp2p request-response, through the trust gate; approvals inbox (CLI)~~;
   TUI inbox with live notifications.
4. ~~`systema find <skill>`: DHT provider lookup + direct signed-card fetch; relay server mode, relay
   reservations for NAT'd nodes, relayed addresses in DHT records~~; deploy a real public bootstrap/relay and
   test NAT traversal and DCUtR hole punching across real networks (loopback tests can't exercise hole punching).
5. Swarm task boards on gossip: post → bid → assign → result → verify.
6. Reputation (local history + vouches + gossipsub peer scoring), proof-of-work join cost, HTTP A2A gateway.
7. Harness self-improvement (cf. Google Research Harness², Oct 2026): propose prompt/skill/tool-guidance edits
   backed by run evidence, **as proposals the operator approves**. Never capabilities, grants or exec rules: an
   agent that can widen its own permissions is the takeover path the broker exists to block.
