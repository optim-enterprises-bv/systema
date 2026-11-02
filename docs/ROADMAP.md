# Systema roadmap

Where Systema stands against other agentic systems (Claude Code / Agent SDK, Hermes, OpenAI
Codex/Swarm, LangGraph, CrewAI, Google ADK + Agent Engine, Letta/MemGPT, the A2A/MCP ecosystem),
and what to build next. Ordered by leverage for the goal: a **local agentic OS with a swarm**.

## Already ahead of the field
- **Swarm / networking** — libp2p + Kademlia DHT discovery + relay v2 + DCUtR hole punching +
  signed agent cards. Most "multi-agent" systems are single-process; Systema is genuinely
  distributed. A2A and MCP are supported.
- **Security model** — capability broker, lethal-trifecta taint tracking, Rule of Two, remembered
  approvals, hard-denied credential/kernel paths. Principled local safety, enforced in code.
- **Human-in-the-loop** — approvals + steer + cancel + a cockpit TUI.
- **Capable local agents** — real shell + file tools (`read/list/write/patch/search_files`),
  per-agent `memory`, an orchestrator with specialist→operator fallback, and a first cut of
  self-improvement (`systema propose`).

## Shipped recently
- [x] `patch`, `search_files`, per-agent `memory` tools; specialist→operator fallback.
- [x] Agents told their workspace + read/write roots (no more path-guessing).
- [x] `systema propose` — audit-log-driven, recency-aware improvement proposals (improver agent
      has zero host access; proposals are inert, applied by hand).
- [x] **Shared message board** (`board_read` / `board_post`) — agents coordinate through a common
      log. Shared posts are treated as untrusted-and-private input, so the trifecta gates a
      swarm agent from reading the operator's board and leaking it outward.
- [x] **Multi-agent delegation + task board** (`delegate_task` / `task_list`, `systema tasks`) — an
      agent hands a self-contained sub-task to another agent, which runs a full turn and returns
      its result; every delegation is recorded durably on the task board (running → done/failed).
      Taint flows forward (the sub-task inherits the caller's trust and its final taint merges
      back), delegation is refused from untrusted sessions (no privilege escalation), and chains
      are depth-limited and non-self. *Next on this: parallel fan-out and cross-swarm delegation.*

- [x] **Scheduling.** The node runs agent turns on an interval from `$SYSTEMA_HOME/schedules.toml`
      (`every = "6h"`), persisting last-run times across restarts and skipping a job whose previous
      run is still going. `systema schedule` lists jobs; `systema schedule <name>` runs one now.
      *Next on this: event triggers (file-watch, board-post, inbound swarm message) and daily
      clock-time (`at = "09:00"`) scheduling.*

## Tier 1 — biggest leverage (make it an OS that *does* things)
- [ ] **Parallel + cross-swarm delegation.** Delegation is sequential and local today; add parallel
      fan-out of independent sub-tasks and delegation to agents on other swarm nodes over A2A.
- [ ] **Event triggers.** Fire jobs on a file change, a new board post, or an inbound swarm message
      (not just a timer), plus daily clock-time scheduling — so the OS reacts, not just ticks.
- [ ] **Semantic memory / retrieval (RAG).** Upgrade from key/value `memory` to embeddings +
      retrieval over a shared knowledge base and ingested documents (cf. Letta/MemGPT, Mem0),
      with shared vs per-agent tiers. Needs a local embedding model behind titan-engine.

## Tier 2 — strong multipliers
- [ ] **Web search + extract** (not just raw `fetch_url`) — cf. Hermes `web_search`/`web_extract`.
- [ ] **Browser / computer-use** — drive a browser or desktop for tasks beyond the filesystem.
- [ ] **Session persistence + observability/replay** — sessions currently vanish on restart; add
      durable run history, tracing and replay (OpenTelemetry-style) on top of the audit log.
- [ ] **Evals** — a test harness for agent quality / regressions, wired to `propose` so proposals
      are testable before they are applied.

## Tier 3 — longer horizon
- [ ] **microVM sandbox (libkrun)** for public/swarm agents — the gate for giving strangers' code
      real execution. The one capability the broker currently reserves rather than grants.
- [ ] **Higher-order swarm coordination** — contract-net negotiation, reputation/trust scoring, a
      shared blackboard across peers (the local board, extended over A2A).
- [ ] **Context compaction + sub-agent isolation** — mid-session compaction and isolated sub-agent
      contexts (cf. Claude Code), vs today's budget-replace-with-summary.
- [ ] **Cloud-model fallback + vision** — overflow to a cloud model and multimodal input; today
      titan-engine serves local fast/deep tiers only.

## Principles that constrain all of the above
- The broker is the only path to the host; the manifest is the ceiling.
- Nothing an agent does can widen its own capabilities, grants or exec rules (self-improvement is
  proposals only).
- Remote/untrusted input never reaches a tool-holding agent without an explicit operator grant.
- New shared surfaces (board, task queue, memory) are untrusted-by-provenance: reading another
  party's writes taints the reader so the Rule of Two still applies.
