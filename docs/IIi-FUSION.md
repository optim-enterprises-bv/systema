# Folding iii into Systema

How Systema absorbs the parts of the [iii](https://github.com/iii-hq/iii) engine's model that are
genuinely useful here -- a generic trigger abstraction, a composable function surface, and a live
browser console -- **without** taking a runtime dependency on iii and without touching its
Elastic-License-2.0 `engine/`. Only the *pattern* crosses over; the code is native Rust in
Systema's own crates, Apache-2.0 throughout.

## Why not the other direction

The alternative -- wrapping `systema-kernel` as a worker *inside* a running iii engine -- was
considered and rejected for this effort: it would make Systema a plugin of someone else's runtime
rather than enhancing Systema itself. This plan goes the other way: iii's useful ideas move *into*
Systema's own kernel/node/console, as Systema code, so Systema stays a single self-contained OS
you can run with nothing else installed.

## What iii has that Systema didn't

| iii primitive | What it gives you | Systema's nearest existing piece |
|---|---|---|
| Trigger (generic 'run this when X') -- cron, HTTP, queue, state-change, comment, anything | New entry points without new code per integration | `schedule.rs`: exactly three fixed kinds (`every`, `on_change`, `on_board`) |
| Function (any worker, any language, one stable id) | A uniform call surface for humans, agents and other processes | Tools are compiled into `systema-kernel::tools`; MCP (`mcp.rs`) is the only pluggable surface |
| Console (chat, trace explorer, worker catalog, injectable pages) | A browser-reachable live view and control panel | `systema-tui`: a terminal cockpit only, nothing over HTTP |
| Compose (`compose::add` at runtime) | Extend a running system without a restart | None -- agents/capabilities are fixed at `systema node` startup |
| RBAC proxy / `pre-trigger` hook | An outer policy layer independent of what's being called | `capability.rs`'s broker is the *only* layer, baked into the kernel |

Systema's own `docs/ROADMAP.md` already named two of these as open items before this doc existed:
an **HTTP A2A gateway** (tier not yet shipped) and **more triggers** (inbound-swarm-message, daily
clock-time). This plan gives both a concrete shape, plus the UI the roadmap never scoped.

## What Systema keeps that iii doesn't have

None of this weakens Systema's actual differentiators, which this plan treats as fixed:

- The **capability broker** and session-level **Rule of Two** taint tracking (`capability.rs`) stay
  the inner, non-bypassable layer for every tool call. A new trigger or a new HTTP entry point is
  just one more way to *start* a turn -- it still runs through the same broker as `systema do`.
- The **libp2p swarm** (`systema-node`) -- discovery, signed cards, cross-node delegation -- is
  Systema's, and stays untouched. iii has no equivalent.
- `systema-kernel` stays a library any binary in the workspace can embed (CLI, TUI, console, and
  the node). No process boundary is introduced between "kernel" and "the rest", unlike iii where
  every capability is a separate OS process talking over a wire protocol.

## Phase 1 -- shipped in this change: the console

`crates/systema-console` (Apache-2.0, new workspace member): a small `axum` HTTP server that
speaks the **existing** `systema_node::control::Control` protocol -- the same JSON-over-unix-socket
protocol the CLI and `systema-tui` already use against `$SYSTEMA_HOME/control.sock`. It adds no new
security surface: every HTTP request is forwarded verbatim to the control socket, so the console
can only ever do what the CLI could already do, through the same trust gate and broker.

What it serves:

- `GET /` -- a single embedded dashboard page (vanilla HTML/JS, no build step): agent catalog,
  swarm peers, approvals inbox with approve/deny buttons, sessions, and a live trace feed.
- `GET /api/{status,agents,peers,sessions,approvals,inbox,grants,rules}` -- read-only passthroughs
  to the matching `Control` variant.
- `POST /api/{ask,steer,cancel,resolve,approve,reject,block,send,find,forget}` -- the write-side
  `Control` variants, as JSON bodies.
- `POST /api/webhook/<name>` -- fires the `on_http = "<name>"` scheduled job(s) (Phase 2, below).
- `GET /api/events` -- Server-Sent-Events, bridging the control socket's `Control::Watch` line
  stream (the same feed the TUI renders) straight to the browser. This is the trace-explorer
  analogue of iii's console, built on Systema's own event bus (`events.rs`), not iii's.

Run it alongside a node:

```sh
systema node &
systema-console --home ~/.systema --port 8787
# open http://127.0.0.1:8787
```

## Phase 2 -- generalize triggers

**Shipped in this change:**

- `Trigger::At(u8, u8)` -- `at = "09:00"` in `schedules.toml`: a daily wall-clock-time (UTC) fire,
  checked on the same tick loop as `every`, with the same "arm on first sight, don't fire" rule.
  Cursor is the epoch-ms of the scheduled instant last fired for, so a restart mid-day neither
  double-fires nor skips the next day. Covered by `daily_at_fires_once_then_again_next_day` and
  `daily_at_armed_after_its_time_does_not_fire_same_day` in `schedule.rs`.
- `Trigger::Http(String)` -- `on_http = "<name>"`: fired on demand, not polled. `systema-console`
  exposes `POST /api/webhook/<name>`, which calls a new `Control::FireWebhook { name }` on the
  node's control socket; the node runs `schedule::http_jobs(home, name)` and fires each matching
  enabled job exactly like a due scheduled job (same `spawn_operator_turn`, same "skip if already
  running / unknown agent" guards). This is the roadmap's "HTTP A2A gateway" line item, scoped to
  triggering -- a general inbound-request gateway is still open (see Phase 2 remaining, below).

**Remaining:**

- `Trigger::Inbound(Option<String>)` -- fire on an inbound swarm A2A request (optionally scoped to
  a skill), reusing the same "arm on first sight, fire on next change" cursor model as
  `OnChange`/`OnBoard`. Needs a cursor source inside the node's A2A request handler (a
  last-inbound-timestamp per skill, mirroring `board::latest_ts`) before `schedule::check` can
  observe it -- deferred because it touches the inbound A2A path directly, where mistakes are
  costlier than in the self-contained `schedule.rs` module the two shipped triggers above live in.

None of this touches the capability broker: a triggered turn still goes through the exact same
`check_capability` / Rule-of-Two path as an operator-typed prompt.

## Phase 3 -- an outer policy hook (not yet done)

iii's `pre-trigger` hook (deny/hold/rewrite before a call reaches its target) is a useful *second*
layer, not a replacement for the broker. Add a `Vec<Box<dyn PolicyHook>>` the kernel consults
before the broker on every tool request -- e.g. a hook that rate-limits by wall-clock regardless of
manifest, or one that logs to an external SIEM. The broker remains the ceiling; hooks can only ever
veto or slow down a call, never approve something the manifest forbids.

## Phase 4 -- pluggable functions (longer horizon)

iii's biggest structural difference is that *any* process in *any* language can register a
function at runtime. Systema's tools are compiled in. A minimal, optional step toward this: let
`mcp.rs`'s existing "external tool server" pattern (already how MCP servers are wired in) become
the general extension point -- a new capability class, `capabilities.workers`, naming local
processes that speak a tiny length-prefixed JSON protocol and register tool names the broker then
gates exactly like any built-in tool. This is deliberately the last phase: it is the one that most
resembles handing out exec rights, and the broker's "nothing an agent does can widen its own
capabilities" principle (`ROADMAP.md`) applies to it in full.

## License note

Everything above is Systema code (Apache-2.0) implementing an architecture *pattern* observed in
iii's Elastic-License-2.0 engine. No file from `iii-hq/iii` is copied, vendored, or linked. This
keeps Systema's own license clean and avoids any ELv2 obligation.
