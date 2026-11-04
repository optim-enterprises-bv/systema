# Systema roadmap

> See also [docs/IIi-FUSION.md](docs/IIi-FUSION.md): folding useful parts of the iii engine's model
> (generic triggers, a composable function surface, a browser console) into Systema natively.

- [x] **Browser console** (`systema-console`) — a small `axum` server over the existing
      control-socket protocol: agent catalog, swarm peers, approvals inbox, sessions, and a live
      Server-Sent-Events trace feed in a browser, alongside the TUI. No new security surface: every
      request is forwarded verbatim through the same trust gate and broker. See
      [docs/IIi-FUSION.md](docs/IIi-FUSION.md) Phase 1.

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

- [x] **Scheduling & triggers.** The node runs agent turns on their own from `schedules.toml` on a
      trigger: an interval (`every = "6h"`), a file/dir change (`on_change`), or a new board post
      (`on_board`). Cursors persist across restarts; a job arms on first sight and fires on the next
      change; overlapping runs are skipped. `systema schedule` lists; `systema schedule <name>` runs
      one now. *Next: inbound-swarm-message trigger and daily clock-time (`at = "09:00"`).*

- [x] **Shared knowledge base with retrieval** (`kb_add` / `kb_search`, `systema kb`) — agents save
      notes/documents (chunked) and retrieve them by **BM25 lexical search**, no embedding model
      required. Same untrusted-by-provenance model as the board. *Next: a vector index when
      titan-engine serves an embedding model (the store is already shaped for it).*

- [x] **Cross-swarm delegation** (`delegate_remote`) — an agent delegates a task to an agent on
      another node that advertises the skill, over the signed A2A path, via a kernel→node bridge.
      The reply is treated as untrusted outbound (fetch legs), gated by the Rule of Two and the
      remote node's own trust gate.
- [x] **Capability advertisement + capability-aware routing** — each signed agent card now carries
      the node's models, max context and a hardware string (set under `[inference]`). `peers`/`find`
      show them, and `delegate_remote` prefers the provider advertising the largest context window,
      so heavy/long work routes to the bigger node. Fields are appended + skipped-when-empty, so
      older signed cards still verify.

- [x] **Parallel fan-out** (`delegate_parallel { skill, tasks }`) — fans several independent tasks
      to swarm agents offering a skill, concurrently, round-robined across providers so they run on
      different nodes' GPUs at once; results come back merged and ordered, each recorded on the task
      board. This is where heterogeneous swarm compute pays off (not GPU-bound on one box).

**Tier 1 is complete.** What remains below is Tier 2+ and the parked sandbox.

## Deferred / blocked
- [x] **Vector (semantic) retrieval for the KB** — *code shipped.* With `[inference].embed_endpoint`
      set, `kb_add` embeds each chunk and `kb_search` ranks by cosine similarity; with it unset the
      KB falls back to BM25, so nothing extra is required. The embedding service runs separately
      (see below) — run it to turn semantic search on.

  **The embedding model doesn't belong on the GPU.** titan-engine already saturates the RTX 5080
  with generation, and an embedding model is small and encoder-only — the wrong thing to fight the
  GPU for. The right home for it is the machine's **Intel NPU**.

  **It can run on the local NPU.** This box is an Intel Core Ultra 9 285HX with a dedicated NPU
  (`/dev/accel/accel0`, `intel_vpu` driver). A small embedding model (e.g. `bge-small-en`,
  `e5-small`, `all-MiniLM-L6`, exported to ONNX/IR) runs on it through **OpenVINO's NPU plugin** (or
  ONNX Runtime with the OpenVINO execution provider), exposed as a tiny OpenAI-compatible
  `/v1/embeddings` service on its own port. Generation stays on the GPU, embeddings run on the idle
  NPU, and neither contends for the other.

  **Shipped shape (no tool changes):** `[inference].embed_endpoint` (+ `embed_model`); on `kb_add`,
  each chunk is embedded via that endpoint and the vector stored alongside the text in `kb.jsonl`;
  on `kb_search`, the query is embedded and ranked by cosine similarity (brute-force scan, fine up
  to tens of thousands of chunks — add an ANN index later), falling back to BM25 when embeddings are
  off or empty. The NPU embedding service lives in `contrib/npu-embeddings/` (OpenVINO, with a CPU
  fallback for testing). *Remaining: a `kb reindex` to embed entries added before the endpoint
  existed, and an ANN index for large stores.*
- [ ] **Local parallel delegation** — low value on a single GPU (generation serializes); the swarm
      path above is the one that matters.
- [ ] **More triggers.** Add an inbound-swarm-message trigger and daily clock-time (`at = "09:00"`)
      to the file-change and board-post triggers already shipped.

## Tier 2 — strong multipliers
- [x] **Web search + extract** — `web_extract` returns a page as readable text (HTML stripped), and
      `web_search` queries a configured SearXNG endpoint (`search_endpoint`); results are untrusted
      web content. See `contrib/searxng/`.
- [ ] **Browser / computer-use** — drive a browser or desktop for tasks beyond the filesystem.
- [x] **Session persistence** — sessions are saved to `sessions/<key>.json` after every turn and
      restored on the next turn, so context and the prefix cache survive a node restart.
      `systema session [key]` lists/prints transcripts.
- [x] **Observability / replay** — every turn is recorded to `runs.jsonl` (prompt, each tool call +
      outcome, final answer, timing, trust). `systema history` lists recent turns; `systema replay
      <id>` shows one turn's full trace. *Next: OpenTelemetry export for external tracing.*
- [x] **Evals** — `systema eval` runs an `evals.toml` suite (one turn per eval, case-insensitive
      `expect`/`expect_absent` substring assertions), so agent changes can be regression-tested.
      An optional `judge = "<criterion>"` is graded PASS/FAIL by the fast model, for checks
      substrings can't express. See `contrib/evals.example.toml`. *Next: wiring evals into `propose`
      so a proposed change is tested before it's applied.*

## Tier 3 — longer horizon
- [x] **microVM sandbox (libkrun)** — a public (swarm-facing) agent's `shell` now runs inside a
      libkrun microVM (`podman --runtime krun`): its own guest kernel, no host filesystem (only the
      agent's workspace at `/work`), no network. Off by default; enable `[sandbox]` and the runtime
      must be present (else a public shell stays denied — enabling only ever *adds* isolation-safe
      capability). See `contrib/microvm/`. *Next: a filtered-network opt-in, and running public
      `exec` in the VM too.*
- [x] **Higher-order swarm coordination.** *Reputation scoring:* each node tracks how well every
      peer serves its requests (`reputation.json`, `systema reputation`). *Blackboard over A2A:* a
      local board post gossips to the swarm (signed, topic `systema/v1/board`); peers add it to
      their board marked as a remote/untrusted author, so agents on different nodes share one board.
      *Contract-net-style award:* cross-swarm delegation now prefers a currently-connected peer
      (liveness), then reputation, then capability. *Remaining: a live bid round-trip (providers
      quote load before the award).*
- [x] **Context compaction** — past the token budget a session is now **summarized and re-seeded**
      with that brief (fast model, background priority) instead of being dropped, so long
      conversations keep their thread. Sub-agent isolation already holds: delegated turns run in
      their own `task:<id>` sessions. *Next: incremental compaction rather than a single summary.*
- [x] **Cloud-model fallback** — when the local engine is unreachable, requests fall back to a
      configured OpenAI-compatible `cloud_endpoint`/`cloud_model` (key from `$SYSTEMA_CLOUD_API_KEY`),
      so the OS keeps working if titan-engine is down. *Remaining: vision/multimodal input, and
      capability-based routing to the cloud for specific heavy tasks (not just on failure).*

## Principles that constrain all of the above
- The broker is the only path to the host; the manifest is the ceiling.
- Nothing an agent does can widen its own capabilities, grants or exec rules (self-improvement is
  proposals only).
- Remote/untrusted input never reaches a tool-holding agent without an explicit operator grant.
- New shared surfaces (board, task queue, memory) are untrusted-by-provenance: reading another
  party's writes taints the reader so the Rule of Two still applies.
