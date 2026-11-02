#!/usr/bin/env bash
# Internet-style discovery with no LAN help: mDNS and card gossip are OFF.
# C is a bootstrap node (as a VPS would be); A and B only know C's address.
# B offers a unique skill; A must find it via the Kademlia DHT, fetch B's signed
# card directly, then reach B with a request. No titan-engine needed.
set -u
cd "$(dirname "$0")/.."
B=${SYSTEMA_BIN:-$PWD/target/debug/systema}
S=$(mktemp -d)
PORT=${PORT:-47001}

for n in a b c; do $B --home $S/$n init >/dev/null; done
for n in a b c; do
  sed -i -e 's/^mdns = true/mdns = false/' -e 's/^gossip_cards = true/gossip_cards = false/' \
         -e 's/^dht_server = "auto"/dht_server = "on"/' $S/$n/systema.toml
done
# C: fixed address, nothing public of its own.
sed -i "s|\"/ip4/0.0.0.0/tcp/0\",|\"/ip4/127.0.0.1/tcp/$PORT\",|; s|\"/ip4/0.0.0.0/udp/0/quic-v1\",||" $S/c/systema.toml
sed -i 's/^public = true/public = false/' $S/c/agents/coder.toml
# B: coder offers a unique skill.
sed -i 's/^id = "code-review"/id = "rust-review"/' $S/b/agents/coder.toml
CID=$($B --home $S/c id)
BOOT="/ip4/127.0.0.1/tcp/$PORT/p2p/$CID"
for n in a b; do sed -i "s|^bootstrap = \[\]|bootstrap = [\"$BOOT\"]|" $S/$n/systema.toml; done

for n in c b a; do timeout 120 $B --home $S/$n node >$S/$n.log 2>&1 & sleep 1; done
trap 'kill $(jobs -p) 2>/dev/null' EXIT
sleep 4

step() { echo; echo "### $*"; }
step "A: peers via gossip (expect none: gossip and mDNS are off)"; $B --home $S/a peers; echo "(end)"
step "A: find rust-review via the DHT"; $B --home $S/a find rust-review
step "A: find a skill nobody offers";   $B --home $S/a find quantum-baking
step "A -> B via the discovered card (expect pending: A is unknown to B)"
$B --home $S/a send "$($B --home $S/b id)" coder --skill rust-review "fn f() {}"
step "B: inbox"; $B --home $S/b inbox
