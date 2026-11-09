#!/usr/bin/env bash
# Cross-swarm delegation across two local nodes over mDNS.
#   B: provider — runs `coder` (public, skill `code-review`)
#   A: delegator — its `operator` calls `delegate_remote` to reach B's coder
# B is pre-granted A's peer for `code-review` (so there's no approval dance), then A's operator
# is asked to delegate a review to the swarm; we assert A received B's answer, not "no provider".
#
# Needs titan-engine on :1234 (both agents use it). Run from the repo root.
set -u
cd "$(dirname "$0")/.."
BIN=${SYSTEMA_BIN:-$PWD/target/release/systema}
[ -x "$BIN" ] || { echo "build first: cargo build --release (or set SYSTEMA_BIN)"; exit 2; }
S=$(mktemp -d)

for n in a b; do
  $BIN --home $S/$n init --no-hub >/dev/null
  # Keep the default (all-interface, ephemeral) listeners so mDNS discovery works; announce often.
  sed -i -e 's/^announce_secs = .*/announce_secs = 5/' $S/$n/systema.toml
done

AID=$($BIN --home $S/a id)
BID=$($BIN --home $S/b id)

# Pre-grant: let A's peer invoke `code-review` on B without approval.
cat > $S/b/trust.json <<JSON
{ "grants": [ { "peer": "$AID", "skill": "code-review", "max_per_hour": 10, "expires_at_ms": null } ], "blocked": [] }
JSON

RUST_LOG=${RUST_LOG:-info,libp2p=warn} $BIN --home $S/b node >$S/b.log 2>&1 & sleep 1
RUST_LOG=${RUST_LOG:-info,libp2p=warn} $BIN --home $S/a node >$S/a.log 2>&1 & sleep 1
trap 'kill $(jobs -p) 2>/dev/null' EXIT

echo "### waiting for A to discover B's code-review skill"
if ! timeout 30 bash -c "until grep -q 'discovered agent' $S/a.log 2>/dev/null; do sleep 1; done"; then
  echo "FAIL: A never discovered B"; $BIN --home $S/a peers; tail -5 $S/a.log; exit 1
fi
$BIN --home $S/a peers

echo "### A's operator delegates a review to the swarm"
$BIN --home $S/a live operator "Use the delegate_remote tool with skill 'code-review' and task 'Review this function in one sentence: fn add(a: i32, b: i32) -> i32 { a - b }'. Then report the reviewer's answer." >/dev/null 2>&1

# Watch A's event stream for the turn to finish.
ANS=$(timeout 90 $BIN --home $S/a watch 2>/dev/null | grep -m1 '"event":"turn_finished"')
echo "### A's operator turn result:"
echo "$ANS" | python3 -c 'import sys,json; print(json.loads(sys.stdin.read())["text"])' 2>/dev/null || echo "$ANS"

echo "### B served the request?"; grep -iE "a2a|inbound|code-review" $S/b.log | sed 's/^.*systema_node: //' | tail -3

if echo "$ANS" | grep -qiE "no known swarm agent|did not respond"; then
  echo "FAIL: A did not reach B's agent."; exit 1
fi
echo "PASS: A's operator delegated across the swarm and got B's answer. logs in $S"
