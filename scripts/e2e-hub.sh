#!/usr/bin/env bash
# The slim hub (systema-hub) as the public bootstrap + relay, with two ordinary nodes.
# Loopback only, temp homes; mDNS and gossip are off everywhere.
#   H: systema-hub (DHT server + circuit relay; no agents, no kernel)
#   B: "behind NAT": no direct listeners, only a reservation on H; DHT client only
#   A: ordinary node that knows only H
# A must find B's skill through the DHT (via H) and reach B through H's relay.
# No titan-engine needed: the request lands in B's inbox as pending.
set -u
cd "$(dirname "$0")/.."
B=${SYSTEMA_BIN:-$PWD/target/debug/systema}
HUB=${SYSTEMA_HUB_BIN:-$PWD/target/debug/systema-hub}
S=$(mktemp -d)
PORT=${PORT:-47301}
fail() { echo "FAIL: $*"; echo "logs in $S"; exit 1; }

for n in a b; do
  $B --home $S/$n init --no-hub >/dev/null
  sed -i -e 's/^mdns = true/mdns = false/' -e 's/^gossip_cards = true/gossip_cards = false/' $S/$n/systema.toml
done
HID=$($HUB --home $S/h --print-peer-id) || fail "hub identity"
HADDR="/ip4/127.0.0.1/tcp/$PORT/p2p/$HID"
# B: no direct listeners at all; reachable only through H. DHT client only.
sed -i -e '/^listen = \[/,/^\]/c\listen = []' \
       -e 's/^dht_server = "auto"/dht_server = "off"/' \
       -e "s|^relays = \[\]|relays = [\"$HADDR\"]|" $S/b/systema.toml
sed -i 's/^id = "code-review"/id = "hub-review"/' $S/b/agents/coder.toml
# A: ordinary DHT node that knows only the hub.
sed -i 's/^dht_server = "auto"/dht_server = "on"/' $S/a/systema.toml
for n in a b; do sed -i "s|^bootstrap = \[\]|bootstrap = [\"$HADDR\"]|" $S/$n/systema.toml; done

RUST_LOG=${RUST_LOG:-info,libp2p=warn} timeout 300 $HUB --home $S/h --listen /ip4/127.0.0.1/tcp/$PORT --external /ip4/127.0.0.1/tcp/$PORT >$S/h.log 2>&1 &
sleep 1
for n in b a; do RUST_LOG=${RUST_LOG:-info,libp2p=warn} timeout 300 $B --home $S/$n node >$S/$n.log 2>&1 & sleep 1; done
trap 'kill $(jobs -p) 2>/dev/null' EXIT
timeout 20 bash -c "until grep -q 'reachable via relay' $S/b.log; do sleep 0.5; done" || { tail -5 $S/b.log; fail "B got no relay reservation on the hub"; }
sleep 3
clean() { sed 's/\x1b\[[0-9;]*m//g' "$1"; }
step() { echo; echo "### $*"; }

PB=$($B --home $S/b id)
step "B: relay reservation"; clean $S/b.log | grep -E 'reservation granted|reachable via relay' | sed 's/^.*INFO //'
step "A: find hub-review via the DHT (expect B, reachable only via a p2p-circuit address)"
OUT=$($B --home $S/a find hub-review); echo "$OUT"
echo "$OUT" | grep -q "$PB" || fail "A did not find B through the hub"
echo "$OUT" | grep -q 'p2p-circuit' || fail "B's card has no relayed address"
step "A -> B through the relay (unknown peer, expect pending)"
SEND=$($B --home $S/a send $PB coder --skill hub-review "fn f() {}"); echo "$SEND"
echo "$SEND" | grep -q '^pending' || fail "request did not reach B's inbox"
step "B: inbox"; $B --home $S/b inbox
step "H: hub log"; clean $S/h.log | grep -E 'relay:|listening|acting' | sed 's/^.*INFO systema_hub: //' | sort | uniq -c
echo; echo "PASS"; echo "logs in $S"
