#!/usr/bin/env bash
# Real cross-NAT hole-punch (DCUtR) test across three hosts.
#
# A genuine hole-punch test needs two nodes behind DIFFERENT real NATs and one
# public relay — it cannot be faked on loopback (there, peers reach each other
# directly, so there is no relayed connection to upgrade). This script drives
# the three hosts over SSH, runs a request from A to B, and checks that the
# relayed connection was upgraded to a DIRECT one by DCUtR.
#
#   RELAY : a public box (VPS) with a routable IP and an open TCP port
#   A, B  : two machines behind separate NATs (home/office/mobile hotspot)
#
# Usage:
#   RELAY=user@vps.example.com RELAY_IP=203.0.113.9 \
#   A=user@boxA B=user@boxB \
#   ./scripts/holepunch-cross-nat.sh
#
# Options (env):
#   RELAY_PORT   public TCP port the relay listens on          (default 47401)
#   BIN          path to the `systema` binary ON EACH HOST     (default: scp the local release build)
#   SKIP_DEPLOY  =1 to assume `systema` is already on each host at $BIN
#   KEEP         =1 to leave the nodes running for inspection
#
# Exit 0 only if A and B log a DCUtR "hole punch" with result: Ok(...).
set -euo pipefail

: "${RELAY:?set RELAY=user@host (the public relay box)}"
: "${RELAY_IP:?set RELAY_IP=<relay public IP>}"
: "${A:?set A=user@host (first NAT box)}"
: "${B:?set B=user@host (second NAT box)}"
RELAY_PORT=${RELAY_PORT:-47401}
REMOTE_BIN=${BIN:-/tmp/systema}
LOCAL_BIN=${SYSTEMA_BIN:-$(dirname "$0")/../target/release/systema}

# Run a (possibly multi-line) command on a host, passing it over stdin so no
# nested-quote escaping is needed. Locals in $2 are expanded here before sending.
ssh_do() { ssh -o BatchMode=yes -o ConnectTimeout=10 "$1" bash -s <<<"$2"; }

echo "## 1. deploy"
if [ "${SKIP_DEPLOY:-}" != 1 ]; then
  [ -x "$LOCAL_BIN" ] || { echo "build first: cargo build --release (or set SYSTEMA_BIN)"; exit 2; }
  for H in "$RELAY" "$A" "$B"; do
    echo "   -> $H:$REMOTE_BIN"
    scp -q "$LOCAL_BIN" "$H:$REMOTE_BIN"
  done
fi

HOME_R=/tmp/systema-relay HOME_A=/tmp/systema-a HOME_B=/tmp/systema-b

echo "## 2. init + configure"
ssh_do "$RELAY" "$REMOTE_BIN --home $HOME_R init >/dev/null
  cd $HOME_R
  sed -i -e 's/^mdns = true/mdns = false/' -e 's/^gossip_cards = true/gossip_cards = false/' systema.toml
  sed -i -e '/^listen = \[/,/^]/c\\listen = [\"/ip4/0.0.0.0/tcp/$RELAY_PORT\"]' \
         -e 's/^dht_server = \"auto\"/dht_server = \"on\"/' \
         -e 's/^relay_server = false/relay_server = true/' \
         -e 's|^external_addrs = \[\]|external_addrs = [\"/ip4/$RELAY_IP/tcp/$RELAY_PORT\"]|' systema.toml
  sed -i 's/^public = true/public = false/' agents/coder.toml"
RID=$(ssh_do "$RELAY" "$REMOTE_BIN --home $HOME_R id")
RADDR="/ip4/$RELAY_IP/tcp/$RELAY_PORT/p2p/$RID"
echo "   relay = $RADDR"

# B: behind NAT, relays through R, offers a renamed skill so A can find it.
ssh_do "$B" "$REMOTE_BIN --home $HOME_B init >/dev/null
  cd $HOME_B
  sed -i -e 's/^mdns = true/mdns = false/' -e 's/^gossip_cards = true/gossip_cards = false/' \
         -e 's/^dht_server = \"auto\"/dht_server = \"off\"/' \
         -e 's|^relays = \[\]|relays = [\"$RADDR\"]|' \
         -e 's|^bootstrap = \[\]|bootstrap = [\"$RADDR\"]|' systema.toml
  sed -i 's/^id = \"code-review\"/id = \"nat-review\"/' agents/coder.toml"

# A: behind a different NAT, ordinary DHT node, knows only R.
ssh_do "$A" "$REMOTE_BIN --home $HOME_A init >/dev/null
  cd $HOME_A
  sed -i -e 's/^mdns = true/mdns = false/' -e 's/^gossip_cards = true/gossip_cards = false/' \
         -e 's/^dht_server = \"auto\"/dht_server = \"on\"/' \
         -e 's|^relays = \[\]|relays = [\"$RADDR\"]|' \
         -e 's|^bootstrap = \[\]|bootstrap = [\"$RADDR\"]|' systema.toml"

echo "## 3. start nodes"
LOG='RUST_LOG=info,libp2p=warn,systema_node=debug'
ssh_do "$RELAY" "cd $HOME_R; nohup env $LOG $REMOTE_BIN --home $HOME_R node >node.log 2>&1 & echo started"; sleep 2
ssh_do "$B"     "cd $HOME_B; nohup env $LOG $REMOTE_BIN --home $HOME_B node >node.log 2>&1 & echo started"; sleep 2
ssh_do "$A"     "cd $HOME_A; nohup env $LOG $REMOTE_BIN --home $HOME_A node >node.log 2>&1 & echo started"

cleanup() {
  [ "${KEEP:-}" = 1 ] && { echo "KEEP=1: nodes left running"; return; }
  for H in "$RELAY" "$A" "$B"; do ssh_do "$H" "pkill -f '$REMOTE_BIN --home' || true" >/dev/null 2>&1 || true; done
}
trap cleanup EXIT

echo "## 4. wait for B's relay reservation"
for _ in $(seq 1 40); do
  ssh_do "$B" "grep -q 'reachable via relay' $HOME_B/node.log" && { echo "   ok"; break; }
  sleep 1
done
ssh_do "$B" "grep -q 'reachable via relay' $HOME_B/node.log" || { echo "FAIL: B never reserved on the relay"; ssh_do "$B" "tail -8 $HOME_B/node.log"; exit 1; }

echo "## 5. A finds B via the DHT and sends through the relay (opens the circuit DCUtR will upgrade)"
PB=$(ssh_do "$B" "$REMOTE_BIN --home $HOME_B id")
ssh_do "$A" "$REMOTE_BIN --home $HOME_A find nat-review || true"
ssh_do "$A" "$REMOTE_BIN --home $HOME_A send $PB coder --skill nat-review 'ping' || true"

echo "## 6. wait for the DCUtR upgrade"
ok=""
for _ in $(seq 1 40); do
  if ssh_do "$A" "grep -qE 'hole punch.*result: Ok' $HOME_A/node.log" \
  || ssh_do "$B" "grep -qE 'hole punch.*result: Ok' $HOME_B/node.log"; then ok=1; break; fi
  sleep 1
done

echo "### DCUtR events (A)"; ssh_do "$A" "grep -i 'hole punch' $HOME_A/node.log | tail -3" || true
echo "### DCUtR events (B)"; ssh_do "$B" "grep -i 'hole punch' $HOME_B/node.log | tail -3" || true
echo "### relay circuits served (R)"; ssh_do "$RELAY" "grep -i 'relay:' $HOME_R/node.log | tail -5" || true

if [ -n "$ok" ]; then
  echo "PASS: A and B punched through their NATs — the relayed connection became direct."
  exit 0
fi
echo "FAIL: no direct-connection upgrade. The data path still works via the relay, but DCUtR did not"
echo "      punch through. This is expected behind symmetric NAT; try a different network for A or B."
exit 1
