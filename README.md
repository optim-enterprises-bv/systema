# Systema Agentium

**A local-first agentic OS.** Capable agents run on your own machine over one shared GPU (via
[titan-engine](../src/titan-engine)), coordinate to get work done, and discover and delegate to
agents on other people's machines — forming a swarm across the internet. Every action an agent takes
goes through a capability broker that enforces a hard security model, so "capable" never means
"unsafe". A browser console (`systema-console`) gives the whole thing a live dashboard alongside the
cockpit TUI.

> *systema agentium*: Latin, "a system of agents".

Not a sandbox that locks agents away from your machine, and not a cloud service that ships your data
off it — your agents are as capable as you let them be, on hardware you own, under rules enforced in
code.

## What it can do

**Capable local agents.** Agents read, write, `patch` and `search_files`; run commands and a real
`shell`; `fetch_url`; keep their own persistent `memory`. Each has a workspace and a manifest that is
the hard ceiling on what it can reach. `systema do "<anything>"` routes to the right agent, and a
specialist that hits its limits hands the task off to the capable `operator` automatically.

**Multi-agent coordination.** Agents hand sub-tasks to each other (`delegate_task`) — tracked on a
durable task board — and coordinate through a shared message board. Across the swarm,
`delegate_remote` runs a task on another node that offers a skill, and `delegate_parallel` fans work
out to many nodes' GPUs at once.

**A swarm across the internet.** Nodes find each other by skill over mDNS and a Kademlia DHT, reach
each other through circuit relays and DCUtR hole-punching (no port-forwarding), and exchange signed
agent cards and requests. Cards advertise each node's models, context window and hardware, so
delegation routes heavy or long-context work to the most capable node.

**Runs on its own.** The node fires agent turns from `schedules.toml` on an interval, a file change,
or a new board post. `systema propose` reviews the audit log and drafts improvement suggestions for
you to apply — it can change nothing itself.

**Knowledge and memory that persist.** A shared knowledge base (`kb_add`/`kb_search`) retrieves by
BM25, or by **semantic vector search** when an embedding service is running (one for the Intel NPU
ships in [`contrib/npu-embeddings/`](contrib/npu-embeddings/), so embeddings never fight the GPU).
Sessions are saved and restored across restarts, keeping context — and the prefix cache — warm.

**Web access.** `web_search` queries a local SearXNG instance ([`contrib/searxng/`](contrib/searxng/))
and `web_extract` reads a page as clean text — so agents can research, not just fetch one URL.

**Safe by construction.** The capability broker brokers every tool call against the agent's manifest;
credentials, Systema's own state and `/proc` are hard-denied. Per-session taint tracking enforces the
**Rule of Two** (lethal trifecta): a session that would hold private data *and* untrusted content
*and* an outbound channel at once is escalated or denied — so an approved stranger can't pull your
files back out through an agent. (See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).)

**Operable.** A Tokyo-Night cockpit TUI (`systema tui`) to monitor, steer and approve agents;
`systema history` / `replay` to see any turn's full trace; `systema eval` to regression-test agents
(substring + LLM-judge assertions); MCP servers behind the same broker.

## Quick start

```sh
cargo build --release
sudo install -m755 target/release/systema /usr/local/bin/systema
sudo install -m755 target/release/systema-tui /usr/local/bin/systema-tui

systema init                       # ~/.systema: ed25519 identity, config, example agents
systema status                     # is titan-engine up on :1234?
systema do "how much disk is free, and what's the biggest file in ~/Downloads?"
systema node                       # join the swarm (LAN via mDNS; add bootstrap peers for WAN)
systema tui                        # the cockpit
```

Run the node on login (user service, no root):

```sh
ln -sf "$PWD/contrib/systema.service" ~/.config/systemd/user/systema.service
systemctl --user daemon-reload && systemctl --user enable --now systema.service
```

Two nodes talking to each other (or `scripts/e2e-two-nodes.sh` for the whole flow):

```sh
systema --home /tmp/a init && systema --home /tmp/b init
systema --home /tmp/a node &  systema --home /tmp/b node &
systema --home /tmp/a peers                                   # B's coder, skill code-review
systema --home /tmp/a send <B-peer-id> coder --skill code-review "fn add(a:i32,b:i32)->i32{a-b}"
# → pending: held in B's inbox until B approves; then B's coder reviews it and signs the reply
```

## Running a public bootstrap + relay (a small VPS)

By default a new node bootstraps from the project's hosted hub (`hub.optimcloud.com`; see [docs/HUB.md](docs/HUB.md), including how to opt out). To host your own, run the slim [`systema-hub`](docs/HUB.md) on a public machine: it serves only the DHT and the relay, with no
agents or state to defend. A full node can do the same job:

```toml
[node]
listen = ["/ip4/0.0.0.0/tcp/4001", "/ip4/0.0.0.0/udp/4001/quic-v1"]
external_addrs = ["/ip4/<public-ip>/tcp/4001", "/ip4/<public-ip>/udp/4001/quic-v1"]
dht_server = "on"
relay_server = true
```

Nodes behind NAT then set `bootstrap` and `relays` to `/ip4/<public-ip>/tcp/4001/p2p/<relay-id>`
(`systema id` on the relay) and `dht_server = "off"`. DCUtR upgrades the relayed link to a direct one
where the NATs allow it — validate across two real NATs with `scripts/holepunch-cross-nat.sh`
([runbook](docs/CROSS-NAT.md)).

## Documentation

- **[docs/CAPABILITIES.md](docs/CAPABILITIES.md)** — the full reference: every tool, CLI command and
  config key, and the state files.
- **[docs/HUB.md](docs/HUB.md)** — the public bootstrap and relay (`systema-hub`).
- **[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)** — the security model, enforced in code.
- **[docs/ROADMAP.md](docs/ROADMAP.md)** — where Systema stands against other agentic systems, what's
  shipped and what's next.

**Not yet:** an HTTP A2A gateway, contract-net negotiation, browser/computer-use.

## Layout

| crate | role |
|---|---|
| `systema-proto` | A2A-shaped `AgentCard` / `Message`, `Signed<T>` envelopes (ed25519, nonce, timestamp, domain-separated) |
| `systema-trust` | the trust gate: grants per peer×skill, rate limits, expiry, blocklist, replay guard, no model-initiated forwarding |
| `systema-kernel` | config, agent manifests, the capability broker, priority GPU scheduler, append-only + persisted sessions, tools, board/tasks/KB/runs, titan-engine client |
| `systema-node` | the libp2p swarm: discovery, NAT traversal, card gossip, scheduling, the A2A/cross-swarm bridge |
| `systema-cli` | the `systema` binary, and the `systema-tui` cockpit |

Agent manifests live in `~/.systema/agents/*.toml` (examples in [`agents/`](agents/)).
