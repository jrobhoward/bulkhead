# Shared by the app tutorial's chapters 7 and 8; sourced after common.sh.
#
# Three nodes on loopback: an HA pair (1 and 2) and a witness (3), which
# with `cluster_configs dr` is also the disaster-recovery node. Cluster
# ports from PORT_1, PORT_2 and PORT_3. Defines:
#   sockn N          node N's admin socket
#   bn N ARGS...     the bulkhead CLI against node N
#   showbn N ARGS... print it, then run it
#   cluster_configs [dr]   write node-1.toml to node-3.toml and the keys
#   start_node_n N   start node N in the background
#   kill_node N...   kill -9 nodes, as a power cut would
#   role N ROLE      whether node N's failover role is ROLE
#   deploy_n N MANIFEST PROGRAM   bundle, sign and deploy to node N
#   chatting N       whether node N's room answers and reaches history
#   seen_n N COUNT   whether node N's room has seen COUNT posts
#   now_ms           milliseconds since the epoch

sockn() { echo "/tmp/bulkhead-apptut-$$-$1.sock"; }
bn() {
  local n=$1
  shift
  "$BULKHEAD" --socket "$(sockn "$n")" "$@"
}
showbn() {
  local n=$1
  shift
  echo "\$ bulkhead $*        # node $n"
  bn "$n" "$@"
  echo
}

cluster_configs() {
  local dr=${1:-}
  echo "[peers]" > peers.toml
  for n in 1 2 3; do
    [ -f "node-$n.key" ] || "$BULKHEAD" cluster keygen "node-$n.key" >/dev/null
    echo "$n = [\"$("$BULKHEAD" cluster pubkey "node-$n.key")\"]" >> peers.toml
  done
  local port
  for n in 1 2 3; do
    port="PORT_$n"
    {
      echo "node = $n"
      echo "data_dir = \"$DEMO_DIR/data-$n\""
      echo "admin_socket = \"$(sockn "$n")\""
      echo "trusted_keys = [\"$SIGNER\"]"
      echo "policy = \"$DEMO_DIR/policy.toml\""
      if [ "$n" -eq 3 ] && [ "$dr" = dr ]; then
        echo "[dr]"
        echo "pair = [1, 2]"
        echo "poll_ms = 200"
      fi
      echo "[cluster]"
      echo "address = \"127.0.0.1:${!port}\""
      [ "$n" -ne 3 ] && echo "seeds = [\"127.0.0.1:$PORT_3\"]"
      echo "key = \"$DEMO_DIR/node-$n.key\""
      echo "peers = \"$DEMO_DIR/peers.toml\""
      # Short timings, so a takeover takes seconds (the defaults: 9 to 10 s).
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
        [ "$dr" = dr ] && echo "dr = 3"
      fi
      echo "lease_ms = 2000"
      echo "margin_ms = 300"
      # The posts, kept whole on both nodes of the pair (and on the DR
      # node) through the replicated log; a plain witness keeps none.
      if [ "$n" -ne 3 ] || [ "$dr" = dr ]; then
        echo "[buckets.posts]"
        echo "durability = \"disk\""
        echo "scope = \"cluster\""
      fi
    } > "node-$n.toml"
  done
}

declare -A NODE_PIDS
start_node_n() {
  "$BULKHEAD" node --config "$DEMO_DIR/node-$1.toml" >> "$DEMO_DIR/node-$1.log" 2>&1 &
  NODE_PIDS[$1]=$!
  for _ in $(seq 600); do [ -S "$(sockn "$1")" ] && return; sleep 0.05; done
  echo "node $1 did not start; see $DEMO_DIR/node-$1.log" >&2
  exit 1
}

kill_node() {
  for n in "$@"; do
    kill -9 "${NODE_PIDS[$n]}" 2>/dev/null || true
    { wait "${NODE_PIDS[$n]}"; } 2>/dev/null || true
    unset "NODE_PIDS[$n]"
    rm -f "$(sockn "$n")"
  done
}

cleanup() {
  for n in "${!NODE_PIDS[@]}"; do
    kill -TERM "${NODE_PIDS[$n]}" 2>/dev/null || true
  done
  wait 2>/dev/null || true
  rm -f /tmp/bulkhead-apptut-$$-*.sock
  echo
  echo "nodes stopped; their configs, data and logs are in $DEMO_DIR"
}

role() { bn "$1" --json failover 2>/dev/null | grep -q "\"role\": \"$2\""; }

deploy_n() {
  local n=$1 manifest=$2 program=$3 name
  name=$(basename "$manifest" .toml)
  "$BULKHEAD" bundle "$manifest" "$program" --key signer.key --out "$name.rbp" >/dev/null
  bn "$n" deploy "$name.rbp" >/dev/null
}

# A process in `ps` can come a moment before its app counts as running:
# wait for answers instead.
chatting() {
  local out
  out=$(bn "$1" call room recent 2>/dev/null) || return 1
  case "$out" in history:*) return 1 ;; esac
}

seen_n() { [ "$(bn "$1" call room seen 2>/dev/null)" = "$2" ]; }

# bash 5's clock: GNU date's %N is not portable (FreeBSD's date has none).
now_ms() { local t=${EPOCHREALTIME/[.,]/}; echo $((t / 1000)); }
