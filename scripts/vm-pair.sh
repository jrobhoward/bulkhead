#!/usr/bin/env bash
# `~` and `$` in strings sent over ssh expand on the VM, as meant.
# shellcheck disable=SC2016,SC2088
# A failover pair across systems (ROADMAP FB5): node 1 on a Linux VM, node 2
# on a FreeBSD VM, the witness (node 3) here, on the host running both VMs.
# The pair's second heartbeat path is a serial line between the guests:
# each VM's second serial port is a TCP listener on the host's loopback
# (docs/OPERATIONS.md, "A serial line between two VMs"), and this script
# joins the two with a relay (OpenBSD nc through a fifo), so it can cut the
# line by stopping the relay. It cuts the network between the guests with
# pf on the FreeBSD guest, as root there.
#
#   scripts/vm-pair.sh LINUX_SSH FREEBSD_SSH [FREEBSD_ROOT_SSH]
#
# LINUX_SSH and FREEBSD_SSH are ssh destinations (user@host) for the user
# that runs each node; FREEBSD_ROOT_SSH defaults to root@ the FreeBSD host.
# Each VM needs rsync, cargo (on PATH or in ~/.cargo/bin) and bash; the
# node's user in the serial device's group (dialout on Linux, dialer on
# FreeBSD); and pf available on FreeBSD (disabled, or with an anchor
# "bulkhead-vm-pair" in its ruleset). The checkout is copied to ~/bulkhead
# on each VM and built there; the run uses ~/bulkhead-vm-pair there.
#
# Environment: SERIAL_PORTS ("47111 47112": the host's loopback ports of
# the Linux and the FreeBSD VM's serial listeners), LINUX_TTY (/dev/ttyS1),
# FREEBSD_TTY (/dev/cuau1), BASE_PORT (7401: nodes 1, 2 and 3 use it and
# the next two), RUN_DIR (target/vm-pair/<time>: configs, keys, samples and
# every node's log, kept).
#
# Steps, one line of result each; it stops at the first failure:
#   1. node 1 primary runs the counter singleton, node 2 standby, both
#      paths heard;
#   2. the network between the guests cut, the serial line up: node 2 stays
#      standby for three leases;
#   3. both cut: still one runner for three leases (the witness hears
#      node 1);
#   4. node 1 killed (SIGKILL): node 2, on FreeBSD, runs the counter;
#   5. node 1 restarted as standby, then a switchover back to it.
# Throughout, both nodes are asked every 250 ms whether they run the
# counter; a sample with both running fails the run. This is coarse (two
# questions over ssh, some milliseconds apart); scripts/soak.sh is the
# exact check, from the nodes' logs.
set -euo pipefail

if [ "${1:-}" = "-h" ] || [ "${1:-}" = "--help" ] || [ $# -lt 2 ]; then
  sed -n '4,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'
  exit 2
fi
LINUX=$1
FREEBSD=$2
FREEBSD_ROOT=${3:-root@${FREEBSD#*@}}

ROOT=$(cd "$(dirname "$0")/.." && pwd)
read -r SERIAL_1 SERIAL_2 <<< "${SERIAL_PORTS:-47111 47112}"
LINUX_TTY=${LINUX_TTY:-/dev/ttyS1}
FREEBSD_TTY=${FREEBSD_TTY:-/dev/cuau1}
BASE_PORT=${BASE_PORT:-7401}
RUN_DIR=${RUN_DIR:-$ROOT/target/vm-pair/$(date +%Y%m%dT%H%M%S)}
BULKHEAD="$ROOT/target/release/bulkhead"
REMOTE=bulkhead-vm-pair
ANCHOR=bulkhead-vm-pair
LEASE_MS=2000
mkdir -p "$RUN_DIR"
RUN_DIR=$(cd "$RUN_DIR" && pwd)

# One ssh connection per destination, reused by every command.
CM=$(mktemp -d /tmp/vm-pair-XXXXXX)
SSH=(ssh -o BatchMode=yes -o ControlMaster=auto -o "ControlPath=$CM/%C" -o ControlPersist=120)
on() {
  local to=$1
  shift
  "${SSH[@]}" "$to" "$@"
}
host_of() { case $1 in 1) echo "$LINUX" ;; 2) echo "$FREEBSD" ;; esac; }
now_ms() { local t=${EPOCHREALTIME/[.,]/}; echo $((t / 1000)); }

# Each guest's address, and the host's on the same network, as each guest
# sees its ssh connection.
read -r HOST_IP _ LINUX_IP _ <<< "$(on "$LINUX" 'echo $SSH_CONNECTION')"
read -r _ _ FREEBSD_IP _ <<< "$(on "$FREEBSD" 'echo $SSH_CONNECTION')"
echo "linux $LINUX_IP, freebsd $FREEBSD_IP, witness $HOST_IP; run in $RUN_DIR"

# --- build -------------------------------------------------------------------

build() {
  local packages=(-p bulkhead -p bulkhead-resolve -p example-counter)
  echo "building (release) here and on both VMs..."
  cargo build --quiet --release --manifest-path "$ROOT/Cargo.toml" "${packages[@]}"
  cargo build --quiet --release --manifest-path "$ROOT/Cargo.toml" -p bulkhead-runner
  local to pids=()
  for to in "$LINUX" "$FREEBSD"; do
    rsync -a --delete -e "${SSH[*]}" --exclude target/ --exclude /.git \
      --exclude /CLAUDE.local.md \
      "$ROOT/" "$to:bulkhead/"
    on "$to" "export PATH=\$HOME/.cargo/bin:\$PATH; cd bulkhead &&
      cargo build --quiet --release ${packages[*]} &&
      cargo build --quiet --release -p bulkhead-runner" &
    pids+=($!)
  done
  wait "${pids[0]}" && wait "${pids[1]}"
}
build

# --- configuration -------------------------------------------------------------

cd "$RUN_DIR"
"$BULKHEAD" keygen signer --no-password >/dev/null
SIGNER=$(tail -n 1 signer.pub)
echo "[peers]" > peers.toml
for n in 1 2 3; do
  echo "$n = [\"$("$BULKHEAD" cluster keygen "node-$n.key")\"]" >> peers.toml
done
addr() {
  case $1 in
    1) echo "$LINUX_IP:$BASE_PORT" ;;
    2) echo "$FREEBSD_IP:$((BASE_PORT + 1))" ;;
    3) echo "$HOST_IP:$((BASE_PORT + 2))" ;;
  esac
}
sock() { echo "/tmp/bulkhead-vm-pair-$1.sock"; }
# The demo's fast timings (examples/failover/demo.sh): 200 ms heartbeats, a
# 1 s node timeout, a 2 s lease and a 300 ms margin.
for n in 1 2 3; do
  if [ "$n" -eq 3 ]; then dir=$RUN_DIR; else dir="~/$REMOTE"; fi
  {
    echo "node = $n"
    echo "data_dir = \"$dir/data-$n\""
    echo "admin_socket = \"$(sock "$n")\""
    echo "trusted_keys = [\"$SIGNER\"]"
    echo "[cluster]"
    echo "address = \"$(addr "$n")\""
    [ "$n" -ne 3 ] && echo "seeds = [\"$(addr 3)\"]"
    echo "key = \"$dir/node-$n.key\""
    echo "peers = \"$dir/peers.toml\""
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
      if [ "$n" -eq 1 ]; then tty=$LINUX_TTY; else tty=$FREEBSD_TTY; fi
      echo "serial = { device = \"$tty\" }"
    fi
    echo "lease_ms = $LEASE_MS"
    echo "margin_ms = 300"
  } > "node-$n.toml"
done
# The node's config takes no `~`: each remote node's paths are made
# absolute where it runs.
cp "$ROOT/examples/counter/bulkhead.toml" counter.toml
printf '\n[appserver.placement]\nmode = "singleton"\n' >> counter.toml
for n in 1 2; do
  to=$(host_of "$n")
  on "$to" "rm -rf $REMOTE && mkdir $REMOTE"
  scp -q -o "ControlPath=$CM/%C" "node-$n.toml" "node-$n.key" peers.toml signer.key counter.toml "$to:$REMOTE/"
  on "$to" "cd $REMOTE && sed -i.orig \"s|~/|\$HOME/|g\" node-$n.toml &&
    ~/bulkhead/target/release/bulkhead bundle counter.toml ~/bulkhead/target/release/example-counter \
      --key signer.key --out counter.rbp >/dev/null"
done

# --- nodes, the relay and the cut ----------------------------------------------

declare -A PIDS
p() {
  local n=$1
  shift
  if [ "$n" -eq 3 ]; then
    "$BULKHEAD" --socket "$(sock 3)" "$@"
  else
    on "$(host_of "$n")" "~/bulkhead/target/release/bulkhead --socket $(sock "$n") $*"
  fi
}
start() {
  local n=$1
  if [ "$n" -eq 3 ]; then
    "$BULKHEAD" node --config node-3.toml >> node-3.log 2>&1 &
    PIDS[3]=$!
  else
    PIDS[$n]=$(on "$(host_of "$n")" "cd $REMOTE && rm -f $(sock "$n") &&
      { nohup ~/bulkhead/target/release/bulkhead node --config node-$n.toml \
        >> node-$n.log 2>&1 </dev/null & echo \$!; }")
  fi
  until_ok 10 p "$n" ps
}
kill_node() {
  local n=$1 signal=${2:-TERM}
  if [ "$n" -eq 3 ]; then
    kill "-$signal" "${PIDS[3]}" 2>/dev/null || true
  else
    on "$(host_of "$n")" "kill -$signal ${PIDS[$n]}" 2>/dev/null || true
  fi
  unset "PIDS[$n]"
}

FIFO="$CM/relay"
mkfifo "$FIFO"
RELAY=
relay_up() {
  # setsid: the relay's two nc processes are one process group to stop.
  setsid bash -c "nc 127.0.0.1 $SERIAL_1 < '$FIFO' | nc 127.0.0.1 $SERIAL_2 > '$FIFO'" &
  RELAY=$!
}
relay_down() {
  if [ -n "$RELAY" ]; then kill -- "-$RELAY" 2>/dev/null || true; fi
  RELAY=
}

PF_ENABLED_HERE=0
pf_ready() {
  local info
  info=$(on "$FREEBSD_ROOT" "kldload -n pf && pfctl -s info 2>/dev/null | head -1; pfctl -s Anchors 2>/dev/null")
  if grep -q "Status: Enabled" <<< "$info"; then
    grep -q "$ANCHOR" <<< "$info" || {
      echo "pf is enabled on the FreeBSD VM without an anchor \"$ANCHOR\"; add one, or disable pf" >&2
      exit 1
    }
  else
    # Stateless, so a block in the anchor stops flows already passing.
    on "$FREEBSD_ROOT" "printf 'anchor \"$ANCHOR\"\npass all no state\n' | pfctl -q -f - && pfctl -q -e"
    PF_ENABLED_HERE=1
  fi
}
net_cut() {
  on "$FREEBSD_ROOT" "printf 'block drop quick from $LINUX_IP to any\nblock drop quick from any to $LINUX_IP\n' |
    pfctl -q -a $ANCHOR -f -"
}
net_up() { on "$FREEBSD_ROOT" "pfctl -q -a $ANCHOR -F rules" 2>/dev/null || true; }

SAMPLER=
cleanup() {
  set +e
  [ -n "$SAMPLER" ] && kill "$SAMPLER" 2>/dev/null
  for n in "${!PIDS[@]}"; do kill_node "$n"; done
  relay_down
  net_up
  if [ "$PF_ENABLED_HERE" = 1 ]; then
    on "$FREEBSD_ROOT" "pfctl -q -d; pfctl -q -F rules" 2>/dev/null
  fi
  wait 2>/dev/null
  for n in 1 2; do
    scp -q -o "ControlPath=$CM/%C" "$(host_of "$n"):$REMOTE/node-$n.log" "$RUN_DIR/" 2>/dev/null
  done
  "${SSH[@]}" -O exit "$LINUX" 2>/dev/null
  "${SSH[@]}" -O exit "$FREEBSD" 2>/dev/null
  "${SSH[@]}" -O exit "$FREEBSD_ROOT" 2>/dev/null
  rm -rf "$CM"
  echo "logs, configs and samples are in $RUN_DIR"
}
trap cleanup EXIT

until_ok() {
  local deadline=$(($(date +%s) + $1))
  shift
  until "$@" >/dev/null 2>&1; do
    [ "$(date +%s)" -ge "$deadline" ] && return 1
    sleep 0.1
  done
}
running() { p "$1" --json status counter 2>/dev/null | grep -q '"status": "running"'; }
failover() { p "$1" --json failover 2>/dev/null | jq -c "$2"; }
role() { [ "$(failover "$1" .role)" = "\"$2\"" ]; }
# Whether node N hears its partner on path P (link or serial): true/false.
heard() { [ "$(failover "$1" ".paths.$2")" = "$3" ]; }

result() {
  local ok=$1 step=$2 what=$3
  if [ "$ok" = 0 ]; then
    printf 'ok    %-28s %s\n' "$step" "$what"
  else
    printf 'FAIL  %-28s %s\n' "$step" "$what"
    exit 1
  fi
}
check() {
  local step=$1 what=$2
  shift 2
  if "$@"; then result 0 "$step" "$what"; else result 1 "$step" "$what"; fi
}

# Both nodes asked every 250 ms; a line per sample: ms, node 1, node 2.
sampler() {
  while :; do
    local t r1=0 r2=0
    t=$(now_ms)
    running 1 && r1=1
    running 2 && r2=1
    echo "$t $r1 $r2"
    sleep 0.25
  done > samples.txt
}
doubles() { awk '$2 == 1 && $3 == 1' samples.txt | wc -l; }

# --- the run ---------------------------------------------------------------------

pf_ready
relay_up
start 3
start 1
start 2
p 1 deploy "~/$REMOTE/counter.rbp" >/dev/null
p 2 deploy "~/$REMOTE/counter.rbp" >/dev/null
sampler &
SAMPLER=$!

steady() {
  until_ok 30 running 1 && until_ok 30 role 2 standby &&
    until_ok 15 heard 1 link true && until_ok 15 heard 1 serial true &&
    until_ok 15 heard 2 link true && until_ok 15 heard 2 serial true
}
check "1 steady" "node 1 (Linux) runs the counter, node 2 (FreeBSD) standby, link and serial heard" steady
for _ in 1 2 3; do p 1 call counter inc >/dev/null; done
check "1 steady" "the counter counts on node 1: $(p 1 call counter get)" true

hold() {
  # For three leases node 1 runs and node 2 stays standby.
  local until=$(($(now_ms) + 3 * LEASE_MS))
  while [ "$(now_ms)" -lt "$until" ]; do
    running 1 && role 2 standby || return 1
    sleep 0.2
  done
}
net_cut
check "2 network cut, serial up" "node 2 hears node 1 on serial only" \
  until_ok 10 heard 2 link false
check "2 network cut, serial up" "node 2 still hears serial" heard 2 serial true
check "2 network cut, serial up" "node 1 runs, node 2 standby for $((3 * LEASE_MS)) ms" hold
net_up
check "2 network cut, serial up" "link heard again" until_ok 15 heard 2 link true

net_cut
relay_down
silent() { heard 2 link false && heard 2 serial false; }
check "3 network and serial cut" "node 2 hears neither path" until_ok 10 silent
check "3 network and serial cut" "node 1 runs, node 2 standby for $((3 * LEASE_MS)) ms (the witness hears node 1)" hold
net_up
relay_up
check "3 network and serial cut" "both paths heard again" steady

t0=$(now_ms)
kill_node 1 KILL
check "4 node 1 killed" "node 2 (FreeBSD) runs the counter" until_ok 30 running 2
check "4 node 1 killed" "after $(($(now_ms) - t0)) ms: $(p 2 call counter get) (a singleton starts from init)" true

start 1
check "5 back" "node 1 rejoins as standby" until_ok 30 role 1 standby
t0=$(now_ms)
p 2 failover switchover >/dev/null
check "5 back" "a switchover moves the counter to node 1 (Linux)" until_ok 30 running 1
check "5 back" "after $(($(now_ms) - t0)) ms; node 2 standby" until_ok 15 role 2 standby

kill "$SAMPLER"
SAMPLER=
n=$(wc -l < samples.txt)
d=$(doubles)
if [ "$d" -eq 0 ]; then
  result 0 "sampled" "$n samples, none with both nodes running the counter"
else
  result 1 "sampled" "$d of $n samples with both running (samples.txt)"
fi
