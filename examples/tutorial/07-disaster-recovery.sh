#!/usr/bin/env bash
# Tutorial chapter 7: disaster recovery. Chapter 5's pair, with the witness
# at a second site keeping a copy of the pair's replicated board and its
# deployments (ROADMAP Phase 8): lose site 1, activate site 2, write there,
# bring site 1 back and fail back. See examples/tutorial/README.md.
#
#   examples/tutorial/07-disaster-recovery.sh          # pauses between steps
#   AUTO=1 examples/tutorial/07-disaster-recovery.sh   # runs straight through
#
# HTTP_PORT (default 8091) is node 1's ingress port, one more node 2's and
# two more node 3's; BASE_PORT (default 7421) the first of the three
# cluster UDP ports.
source "$(dirname "$0")/common.sh"
HTTP_PORT=${HTTP_PORT:-8091}
BASE_PORT=${BASE_PORT:-7421}
url() { echo "http://127.0.0.1:$((HTTP_PORT + $1 - 1))/board"; }
addr() { echo "127.0.0.1:$((BASE_PORT + $1 - 1))"; }

cat > policy.toml <<'TOML'
[apps.board]
grant = ["bulkhead:http-routes", "bulkhead:keyvalue", "bulkhead:logging"]
buckets = ["board"]
TOML
echo "[peers]" > peers.toml
for n in 1 2 3; do
  [ -f "node-$n.key" ] || "$BULKHEAD" cluster keygen "node-$n.key" >/dev/null
  echo "$n = [\"$("$BULKHEAD" cluster pubkey "node-$n.key")\"]" >> peers.toml
done
for n in 1 2 3; do
  {
    echo "node = $n"
    echo "data_dir = \"$DEMO_DIR/data-$n\""
    echo "admin_socket = \"$(sock "$n")\""
    echo "trusted_keys = [\"$SIGNER\"]"
    echo "policy = \"$DEMO_DIR/policy.toml\""
    if [ "$n" -eq 3 ]; then
      # The DR copy: the pair's replicated log and its deployments, pulled
      # from either pair node, the apps held and not started.
      echo "[dr]"
      echo "pair = [1, 2]"
      echo "poll_ms = 200"
    fi
    echo "[cluster]"
    echo "address = \"$(addr "$n")\""
    [ "$n" -ne 3 ] && echo "seeds = [\"$(addr 3)\"]"
    echo "key = \"$DEMO_DIR/node-$n.key\""
    echo "peers = \"$DEMO_DIR/peers.toml\""
    echo "heartbeat_interval_ms = 200"
    echo "node_timeout_ms = 1000"
    if [ "$n" -eq 3 ]; then
      echo "role = \"witness\""
      echo "[witness]"
      echo "pair = [1, 2]"
      echo "first = 1"
    else
      echo "[failover]"
      echo "partner = $((3 - n))"
      echo "arbiter = \"witness\""
      echo "witness = 3"
      echo "replication = true"
      # Node 3 keeps the DR copy (set alike on both).
      echo "dr = 3"
    fi
    echo "lease_ms = 2000"
    echo "margin_ms = 300"
    echo "[http.main]"
    echo "address = \"127.0.0.1:$((HTTP_PORT + n - 1))\""
    if [ "$n" -ne 3 ]; then
      echo "partner_url = \"http://127.0.0.1:$((HTTP_PORT + 2 - n))\""
    fi
    # On node 3 too: its copy of the pair's cluster bucket.
    echo "[buckets.board]"
    echo "durability = \"disk\""
    echo "scope = \"cluster\""
  } > "node-$n.toml"
done

start_node 3
start_node 1
start_node 2
SINGLETON='[appserver.placement]
mode = "singleton"'
deploy_board 1 "$BOARD_DIR/bulkhead.toml" "$SINGLETON"
deploy_board 2 "$BOARD_DIR/bulkhead.toml" "$SINGLETON"
role() { p "$1" --json failover | grep -q "\"role\": \"$2\""; }
serves() { [ "$(curl -s -o /dev/null -w '%{http_code}' "$(url "$1")")" = 200 ]; }
until_ok 30 serves 1
until_ok 30 role 2 standby
for _ in 1 2 3; do curl -s -X POST "$(url 1)" >/dev/null; done
holds_board() { p 3 --json dr | grep -q '"apps": 1'; }
until_ok 60 holds_board

cat <<EOF

== Chapter 7: disaster recovery
  site 1: node 1  process ${NODE_PIDS[1]}   primary: runs the board   $(url 1)
          node 2  process ${NODE_PIDS[2]}   standby                   $(url 2)
  site 2: node 3  process ${NODE_PIDS[3]}   witness and DR node       $(url 3)

Node 3 keeps a copy of the pair's board, a poll behind, and holds the
board app, deployed and not started. From another terminal:
  export PATH="$(dirname "$BULKHEAD"):\$PATH"
  bulkhead --socket $(sock 3) dr
  bulkhead --socket $(sock 3) list
  bulkhead --socket $(sock 1) failover
EOF

pause "node 3's copy"
showp 3 dr
showp 3 list

pause "lose site 1: kill -9 both nodes"
kill -9 "${NODE_PIDS[1]}" "${NODE_PIDS[2]}"
{ wait "${NODE_PIDS[1]}" "${NODE_PIDS[2]}"; } 2>/dev/null || true
echo "Both site-1 nodes are gone. Node 3 waits for you: it never activates by"
echo "itself, since only you can tell a lost site from a cut-off one."

pause "activate node 3 without confirming"
showp 3 dr activate || true
cat <<'EOF'
Refused. A site 1 that still ran and heard itself would go on running the
board beside node 3; confirm only when both site-1 nodes are down or fenced.
EOF

pause "activate node 3, confirming site 1 is down"
showp 3 dr activate --site-1-down
until_ok 30 serves 3
show curl -s "$(url 3)"
show curl -s -X POST "$(url 3)"
showp 3 dr
cat <<'EOF'
Node 3 leads from its copy at a new epoch and runs the board: the count
it had (3, unless a write came after its last poll), and new writes. A
write site 1 committed after the copy's last entry would be lost; `bulkhead
dr` showed the lag before.
EOF

pause "bring site 1 back"
start_node 1
start_node 2
reseeded() { p "$1" failover | grep -q 'caught up'; }
until_ok 60 reseeded 1
until_ok 60 reseeded 2
showp 1 failover
showp 3 dr
cat <<'EOF'
Both site-1 nodes heard from the witness that node 3 is active, marked their
copies stale, and reseeded from a full snapshot of node 3's. They run
nothing until the hand-back.
EOF

pause "fail back to node 1"
showp 3 dr failback --to 1
until_ok 30 serves 1
show curl -s "$(url 1)"
until_ok 30 role 2 standby
showp 1 failover
showp 3 dr
cat <<'EOF'
Node 1 leads again with node 3's count, node 2 is its standby, and node 3
holds the board and keeps its copy from the pair again.
EOF

if [ "$AUTO" != 1 ]; then
  echo
  echo "The nodes keep running until Ctrl-C."
  wait
fi
