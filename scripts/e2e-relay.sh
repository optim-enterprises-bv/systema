#!/usr/bin/env bash
# Relay path, as for a node behind NAT. mDNS and gossip are off everywhere.
#   R: public bootstrap + circuit relay (as a VPS would run it)
#   B: "behind NAT": listens on nothing directly, only via a reservation on R
#   A: ordinary node that knows only R
# A must find B's skill through the DHT and reach B through R's relay.
# The final step needs titan-engine on :1234 (skip it with NO_LLM=1).
set -u
cd "$(dirname "$0")/.."
B=${SYSTEMA_BIN:-$PWD/target/debug/systema}
S=$(mktemp -d)
PORT=${PORT:-47201}

for n in a b r; do
  $B --home $S/$n init --no-hub >/dev/null
  sed -i -e 's/^mdns = true/mdns = false/' -e 's/^gossip_cards = true/gossip_cards = false/' $S/$n/systema.toml
done
# R: fixed public-style address, relay server, DHT server, no public agents of its own.
sed -i -e '/^listen = \[/,/^\]/c\listen = ["/ip4/127.0.0.1/tcp/'"$PORT"'"]' \
       -e 's/^dht_server = "auto"/dht_server = "on"/' \
       -e 's/^relay_server = false/relay_server = true/' \
       -e 's|^external_addrs = \[\]|external_addrs = ["/ip4/127.0.0.1/tcp/'"$PORT"'"]|' $S/r/systema.toml
sed -i 's/^public = true/public = false/' $S/r/agents/coder.toml
RID=$($B --home $S/r id)
RADDR="/ip4/127.0.0.1/tcp/$PORT/p2p/$RID"
# B: no direct listeners at all; reachable only through R. DHT client only.
sed -i -e '/^listen = \[/,/^\]/c\listen = []' \
       -e 's/^dht_server = "auto"/dht_server = "off"/' \
       -e "s|^relays = \[\]|relays = [\"$RADDR\"]|" $S/b/systema.toml
sed -i 's/^id = "code-review"/id = "nat-review"/' $S/b/agents/coder.toml
# A: ordinary DHT node.
sed -i 's/^dht_server = "auto"/dht_server = "on"/' $S/a/systema.toml
for n in a b; do sed -i "s|^bootstrap = \[\]|bootstrap = [\"$RADDR\"]|" $S/$n/systema.toml; done

for n in r b a; do RUST_LOG=${RUST_LOG:-info,libp2p=warn} timeout 300 $B --home $S/$n node >$S/$n.log 2>&1 & sleep 1; done
trap 'kill $(jobs -p) 2>/dev/null' EXIT
timeout 20 bash -c "until grep -q 'reachable via relay' $S/b.log; do sleep 0.5; done" || { echo "B got no relay reservation"; tail -5 $S/b.log; exit 1; }
sleep 3
clean() { sed 's/\x1b\[[0-9;]*m//g' "$1"; }

PB=$($B --home $S/b id)
step() { echo; echo "### $*"; }
step "B: relay reservation";            clean $S/b.log | grep -E 'reservation granted|reachable via relay' | sed 's/^.*INFO //'
step "A: find nat-review via the DHT (expect B, reachable only via a p2p-circuit address)"
$B --home $S/a find nat-review
step "A -> B through the relay (unknown peer, expect pending)"
$B --home $S/a send $PB coder --skill nat-review "fn add(a: i32, b: i32) -> i32 { a - b }"
ID=$($B --home $S/b inbox | head -1 | cut -d' ' -f1)
step "B: approve $ID";                  $B --home $S/b approve $ID --per-hour 5 >/dev/null && echo approved
if [ -z "${NO_LLM:-}" ]; then
  step "A -> B through the relay again (expect B's coder to answer)"
  time $B --home $S/a send $PB coder --skill nat-review "Review in one sentence: fn add(a: i32, b: i32) -> i32 { a - b }"
fi
step "R: relay log";                    clean $S/r.log | grep -E 'relay:' | sed 's/^.*INFO systema_node: //' | sort | uniq -c
step "A: how it is connected to B";     clean $S/a.log | grep -E 'hole punch' | sed 's/^.*INFO systema_node: //' | cut -c1-160 | head -3; echo "(end)"
echo "logs in $S"
