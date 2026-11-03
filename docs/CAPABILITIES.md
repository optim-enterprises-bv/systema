# Systema capabilities reference

Everything agents and operators can do, in one place. For *why* it's built this way (the security
model), see [ARCHITECTURE.md](ARCHITECTURE.md); for what's next, [ROADMAP.md](ROADMAP.md).

## Agent tools

Every tool call goes through the capability broker: it is checked against the agent's manifest,
executed on the agent's behalf, taint-tracked and audited. A tool only appears to an agent if its
manifest grants the capability behind it.

| Tool | Needs | What it does |
|---|---|---|
| `read_file` / `list_dir` | `fs_read` (or workspace) | Read a UTF-8 file / list a directory, within allowed trees. |
| `write_file` | `fs_write` (or workspace) | Create or overwrite a file. |
| `patch` | `fs_write` | Replace one unique snippet in a file in place. |
| `search_files` | `fs_read` | Recursive substring search of names + text contents (skips hidden files). |
| `memory` | always | The agent's own persistent key/value store (`set`/`get`/`list`/`delete`), in its workspace. |
| `board_read` / `board_post` | `board` | Read/post to the shared message board agents coordinate through. |
| `kb_search` / `kb_add` | `kb` | Search / add to the shared knowledge base (BM25, or vector when embeddings are on). |
| `delegate_task` | `delegate` | Hand a sub-task to another **local** agent and get its result. |
| `delegate_remote` | `delegate` | Delegate a task to an agent on **another swarm node** advertising a skill. |
| `delegate_parallel` | `delegate` | Fan several tasks out across the swarm concurrently; merged results. |
| `task_list` | `delegate` | Read the task board (delegated tasks + status). |
| `run` | `exec` or `shell` | Run one program (no shell), scrubbed env, workspace cwd, timeout. |
| `shell` | `shell` (non-public) | A real `bash -lc` in your home — the fast path for most local work. |
| `fetch_url` | `network` | HTTP GET a public http(s) URL (no redirects; result is untrusted). |
| `web_extract` | `network` | Fetch a public page and return it as readable text (HTML stripped; untrusted). |
| `web_search` | `network` + `search_endpoint` | Search the web via a configured SearXNG endpoint; returns title/URL/snippet. |
| `mcp__<server>__<tool>` | `mcp` | A tool on an operator-configured MCP server. |

Hard-denied regardless of manifest: the Systema home, `~/.ssh`, `~/.gnupg`, keyrings, browser
profiles, cloud creds, `/etc/shadow`, `/proc`, … Paths are canonicalized first, so `..` and
symlinks can't escape.

## Agent manifest (`agents/<name>.toml`)

```toml
name = "operator"
description = "General assistant for this machine."
model = "fast"          # fast | deep (titan-engine tiers)
priority = "normal"     # background | normal | interactive
system_prompt = "..."   # the kernel appends a working-context block (workspace + reachable paths)

[[skills]]              # advertised to the swarm when public
id = "code-review"
name = "Code review"
description = "..."

[capabilities]
fs_read  = ["~/src"]   # trees readable (private data)
fs_write = []          # trees writable (implies read); workspace is always writable
network  = false       # fetch_url
shell    = false       # real bash (never for public agents)
exec     = []           # pre-approved programs (program + arg prefix + allow/ask)
mcp      = []           # MCP servers this agent may call
board    = false        # message board
delegate = false        # delegate_task / _remote / _parallel / task_list
kb       = false        # knowledge base
public   = false        # advertise skills to the swarm (requests still hit the trust gate)
```

The manifest is the ceiling — nothing outside it is ever granted. Default agents: `operator`
(capable generalist, the orchestrator's default), `coder`, `sysadmin`, `researcher` (narrow
specialists), `improver` (no host access; drafts proposals).

## Orchestration

- `systema do "<request>"` (or the cockpit's **ask OS**) routes to the best agent. It defaults to
  `operator` and picks a specialist only on a clear domain match.
- If a specialist hits its capability ceiling mid-turn, the turn **hands off to `operator`**
  automatically (local sessions only). So a local request still completes.

## Scheduling & triggers (`$SYSTEMA_HOME/schedules.toml`)

The node fires agent turns on their own. Each job has exactly one trigger:

```toml
[[job]]
name = "log-sweep"
agent = "operator"      # optional, defaults to operator
prompt = "Summarize errors in the last 6h of the journal; post to the board if needed."
every = "6h"            # interval (s/m/h/d)
# on_change = "~/path"  # OR fire when a file/dir's mtime changes
# on_board  = "ops"     # OR fire on a new board post to a topic (empty/* = any)
enabled = true
```

A new job arms on first sight and fires on the next change; overlapping runs are skipped; cursors
persist across restarts. `systema schedule` lists; `systema schedule <name>` runs one now.

## The swarm

- **Discovery:** mDNS on the LAN + a Kademlia DHT across the internet; agents are found by skill.
- **Reach:** QUIC/TCP, circuit relays for nodes behind NAT, DCUtR hole-punching to upgrade a
  relayed link to a direct one.
- **A2A:** signed agent cards + signed request/response. An inbound request from an unknown peer
  goes to the approvals inbox; grants are one peer × one skill, rate-limited, optionally expiring.
- **Capability advertisement:** cards carry the node's `models`, `max_context` and `hardware`
  (set under `[inference]`); `delegate_remote` routes to the most capable provider, and
  `delegate_parallel` round-robins across providers to use many nodes' GPUs at once.

## Self-improvement

`systema propose` digests the audit log and has the capability-less `improver` agent draft
improvement proposals to `~/.systema/proposals/<ts>.md`. It can change nothing — proposals are
inert text the operator applies by hand.

## CLI

| Command | Purpose |
|---|---|
| `systema init` / `id` / `status` | Set up the home / print the peer id / node vitals. |
| `systema agents` / `cards` | List loaded agents / print this node's signed public cards. |
| `systema ask <agent> <prompt>` | One turn with an agent (in-process). |
| `systema do <prompt>` | Ask the OS — route to the best agent (with operator fallback). |
| `systema propose` | Draft improvement proposals from the audit log. |
| `systema tasks` | Show the task board. |
| `systema session [key]` | List persisted sessions, or print one's transcript. |
| `systema eval [name]` | Run the eval suite (`evals.toml`): substring assertions + optional LLM-judge criterion. |
| `systema history [agent]` | Recent turns — prompt, tool count, status, timing. |
| `systema replay <id>` | One turn's full trace: prompt → each tool call/result → answer. |
| `systema schedule [name]` | List scheduled jobs, or run one now. |
| `systema kb <query>` / `kb-add --title T <text>` | Search / seed the knowledge base. |
| `systema node` | Run the swarm node until ctrl-c. |
| `systema peers` / `find <skill>` | Discovered peers / DHT lookup by skill (show capabilities). |
| `systema send <peer> <agent> --skill <s> <text>` | Direct A2A request to a peer. |
| `systema inbox` / `approve` / `reject` / `block` / `grants` | Manage inbound requests and grants. |
| `systema live <agent> <prompt>` / `steer` / `cancel-turn` | Drive turns through a running node. |
| `systema sessions` / `approvals` / `resolve` / `rules` | Inspect/answer running turns and tool approvals. |
| `systema watch` | Stream kernel events as JSON lines. |
| `systema tui` | The cockpit: monitor, steer and approve agents. |

## Configuration (`$SYSTEMA_HOME/systema.toml`)

- **`[inference]`** — `endpoint`, `fast_model`, `deep_model`, `max_concurrent`, `timeout_secs`,
  `temperature`, `top_p`; advertised capabilities `max_context`, `hardware`; embeddings
  `embed_endpoint`, `embed_model` (see `contrib/npu-embeddings/`).
- **`[node]`** — `listen`, `bootstrap`, `mdns`, `announce_secs`, `gossip_cards`, `dht_server`,
  `relays`, `external_addrs`, `relay_server`, `[node.relay_limits]`, `[node.trust]`.
- **MCP servers** — `[mcp.<name>]` with `command`, `args`, `env`, allowed `tools`, `ask_tools`,
  and the trifecta legs it touches (`untrusted`/`private`/`outbound`, default all three).
- **`schedules.toml`** — scheduled/triggered jobs (above). See `contrib/schedules.example.toml`.

## State files (`$SYSTEMA_HOME/`, default `~/.systema/`)

`identity.key`, `systema.toml`, `agents/`, `workspaces/<agent>/`, `sessions/<key>.json`,
`control.sock`, `audit.jsonl`, `trust.json`, `inbox.json`, `approvals.json`, `board.jsonl`,
`tasks.jsonl`, `kb.jsonl`, `runs.jsonl`, `schedules.toml` + `schedule-state.json`, `evals.toml`,
`proposals/`.

Observability: `audit.jsonl` is one line per tool call; `runs.jsonl` is one record per *turn*
(prompt, every tool call + outcome, final answer, timing, trust) — the per-turn view behind
`systema history` and `systema replay`.

Sessions persist to `sessions/<key>.json` and are restored on the next turn (as long as the agent's
system prompt is unchanged), so conversation context — and titan-engine's prefix cache — survive a
node restart. A session is still replaced, not rewritten, once it outgrows its token budget.
