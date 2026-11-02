# Systema Agentium

A local agentic OS. Agents run as managed processes over one shared GPU (via
[titan-engine](../src/titan-engine)), and find and message agents on other Systema nodes
across the internet, forming a swarm.

> *systema agentium*: Latin, "a system of agents".

## Status

Scaffold, phase 1–2. Works today:

- `systema init`: creates `~/.systema` with an ed25519 node identity (`0600`), config and example agents.
- `systema ask <agent> <prompt>`: one turn through the kernel: append-only session → priority GPU scheduler → titan-engine.
- `systema do <prompt>`: ask the OS — the orchestrator routes to the best agent (falling back to `operator`).
- `systema propose`: review the audit log and draft improvement proposals to `~/.systema/proposals/` (applied by hand, never automatically).
- `systema tasks`: show the task board — sub-tasks agents delegated to each other (via `delegate_task`) and their status.
- `systema schedule [name]`: list scheduled jobs (from `~/.systema/schedules.toml`), or run one now. The node fires jobs on their interval on its own.
- `systema kb <query>` / `systema kb-add --title T <text>`: search or seed the shared knowledge base agents use (`kb_search`/`kb_add` tools), BM25 retrieval.
- `systema node`: libp2p node (QUIC+TCP, relay client + DCUtR, mDNS, Kademlia, GossipSub) that announces
  signed agent cards and records verified cards from peers.
- **Agent ↔ agent requests across nodes** (`/systema/a2a/1.0.0`): `systema send <peer> <agent> --skill <id> <text>`.
  Unknown peers land in your **approvals inbox**; approved peers get rate-limited, per-skill grants; replies are
  signed by the answering node and verified.
- **Swarm-wide skill lookup:** `systema find <skill>` queries the Kademlia DHT for providers, fetches their signed
  cards directly (`/systema/card/1.0.0`) and verifies them; works with mDNS and gossip off, through a bootstrap
  node (`scripts/e2e-dht-find.sh`). Point nodes at one with `bootstrap = ["/ip4/…/tcp/…/p2p/<id>"]` and run
  the bootstrap itself with `dht_server = "on"`.
- **Relays for nodes behind NAT.** A public node can act as bootstrap + circuit relay; a node behind NAT reserves a
  slot on it and is advertised (DHT records and cards) at its `…/p2p-circuit/p2p/<id>` address, so others can reach
  it without port forwarding (`scripts/e2e-relay.sh`). Reservations are kept alive and re-requested if they lapse.
  DCUtR then upgrades the relayed link to a **direct** connection where the NATs allow it; validate that across two
  real NATs + a public relay with `scripts/holepunch-cross-nat.sh` (runbook: `docs/CROSS-NAT.md`).
- `systema peers | inbox | approve | reject | block | grants`: drive the running node over a private Unix socket.

### Running a public bootstrap + relay (e.g. a small VPS)

```toml
[node]
listen = ["/ip4/0.0.0.0/tcp/4001", "/ip4/0.0.0.0/udp/4001/quic-v1"]
external_addrs = ["/ip4/<public-ip>/tcp/4001", "/ip4/<public-ip>/udp/4001/quic-v1"]
dht_server = "on"
relay_server = true

[node.relay_limits]          # what you donate to others
max_circuits = 32
max_circuit_secs = 900       # agent requests can wait minutes for a GPU
max_circuit_bytes = 4194304
```

On nodes behind NAT: `bootstrap = ["/ip4/<public-ip>/tcp/4001/p2p/<relay-id>"]`, `relays = [same]`,
`dht_server = "off"`. Get the id with `systema id` on the relay.
- **Capability broker + tools.** Agents call `read_file`, `list_dir`, `write_file`, `run`, `fetch_url`; the kernel
  checks each call against the agent's manifest, executes it on the agent's behalf and audits it. Credentials,
  Systema's own state and `/proc` are hard-denied; each agent has a free workspace.
- **Rule of Two.** Sessions track untrusted content / private data / outbound channel; a call that would complete
  the lethal trifecta needs your approval (`y/N` in `systema ask`, refused on the node). An approved remote peer
  therefore can't get your files back through an agent.
- **Cockpit TUI** (`systema tui`): a Tokyo-Night terminal dashboard to monitor, steer and approve agents on a
  running node. Live agent list with state, a focus/approval panel, node + swarm vitals, a `tail -f` event log,
  and a hotbar: `a` ask · `s` steer · `c` cancel · `y`/`n`/`A` answer approvals · `j`/`k` move · `q` quit.
  Drives the node over its control socket (`watch` stream + `ask`/`steer`/`cancel`/`resolve`).
- `systema status | agents | cards | id`. Every inbound and tool decision is logged to `~/.systema/audit.jsonl`.
- Control from the shell too: `systema live <agent> <prompt>`, `steer`, `cancel-turn`, `sessions`, `approvals`,
  `resolve <id> [--always|--deny]`, `rules`, `watch`.

- **MCP servers** (stdio) behind the same broker. Declare them in `~/.systema/systema.toml`, then add
  `mcp = ["notes"]` to an agent's `[capabilities]`:

  ```toml
  [mcp.notes]
  command = "python3"
  args = ["-I", "/path/to/server.py"]
  env = { NOTES_DIR = "/home/me/notes" }   # the only env vars the server sees
  tools = ["add_note", "list_notes"]        # optional allowlist
  ask_tools = []                           # tools that always need approval
  # Trifecta legs a call adds (default all true = always ask):
  untrusted = false
  private = true
  outbound = false
  ```

Not yet: microVM sandbox for public agents, task boards, HTTP A2A gateway, reputation.
See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the security model, and [docs/ROADMAP.md](docs/ROADMAP.md)
for where Systema stands against other agentic systems and what's next.

## Install

```sh
cargo build --release
sudo install -m755 target/release/systema /usr/local/bin/systema
sudo install -m755 target/release/systema-tui /usr/local/bin/systema-tui
```

Run the node on login (optional, user service — no root):

```sh
ln -sf "$PWD/contrib/systema.service" ~/.config/systemd/user/systema.service
systemctl --user daemon-reload && systemctl --user enable --now systema.service
systema tui
```

See [`contrib/systema.service`](contrib/systema.service) for details.

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
