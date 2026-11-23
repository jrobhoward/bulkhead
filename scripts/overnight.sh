#!/usr/bin/env bash
# Overnight baselines on an idle machine: each benchmark run in ROUNDS
# rounds, every run waiting for the load to settle first, on this machine
# and (with --pi) a test machine reached by ssh at the same time. Builds
# everything before the first measurement.
#
#   scripts/overnight.sh                      # this machine, 3 rounds
#   scripts/overnight.sh --pi USER@HOST       # and that machine too
#   scripts/overnight.sh --only native,sql --rounds 2
#   scripts/overnight.sh --list
#
# Options: --rounds N (3); --max-load X (1.5: the 1-minute load a run
# waits for); --wait M (30: minutes a run waits before it goes ahead,
# marked contended); --pi USER@HOST; --pi-dir DIR (bulkhead, relative to
# the remote home: its checkout, synced from this one); --pi-wrap CMD (a
# command the remote build runs under, such as a thermal or drive
# monitor); --local (this machine only: how the remote side runs).
#
# Output: target/overnight/<UTC stamp>/<machine>.md and .log, the remote
# side's copied into the same directory. A run on a machine whose load
# would not settle, or with a TLC job or a running VM (this machine's
# VMs share its cores), is marked contended. Nothing here measures a
# change: these are the baselines the next change is measured against.
set -uo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT" || exit 1

# name | what it measures (ROADMAP item) | environment | command
BENCHES=(
  "native|native instances: calls, requests from a callback and a sender, casts, calls in flight (baseline for AP1b, O1b)||target/release/examples/native_cost 4"
  "native-senders|native with 800 idle sender channels per instance (AP1b on this machine)|NATIVE_COST_SENDERS=800|target/release/examples/native_cost 4"
  "native-traced|native with every message from the host traced (O1b)|NATIVE_COST_TRACED=1|target/release/examples/native_cost 4"
  "sql|SELECT 1, an update, a transaction, 16 instances at once, through the datasource's driver (DA3b)||target/release/examples/sql_cost"
  "egress|outgoing HTTP and HTTPS, 2 B and 1 MiB bodies, 16 instances at once, through the egress driver (DA4b, PF1)||target/release/examples/egress_cost"
  "ingress|HTTP/1.1, HTTPS and HTTP/2 through the ingress driver beside the node's former ingress, S37's harness (PF2, DA2b)|DRIVER=target/release/bulkhead-ingress S37_DRIVER=da1 S37_INGRESS=1 ROUNDS=1|sh spikes/s37/scripts/run.sh"
)

rounds=3 max_load=1.5 wait_min=30 pi="" pi_dir=bulkhead pi_wrap="" local=0 only="" list=0
while [ $# -gt 0 ]; do
  case "$1" in
    --rounds) rounds=$2; shift ;;
    --max-load) max_load=$2; shift ;;
    --wait) wait_min=$2; shift ;;
    --pi) pi=$2; shift ;;
    --pi-dir) pi_dir=$2; shift ;;
    --pi-wrap) pi_wrap=$2; shift ;;
    --only) only=$2; shift ;;
    --local) local=1 ;;
    --list) list=1 ;;
    -h | --help) sed -n '2,25p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "overnight: unknown argument $1" >&2; exit 2 ;;
  esac
  shift
done
if [ "$list" = 1 ]; then
  for b in "${BENCHES[@]}"; do IFS='|' read -r name what _ <<<"$b"; printf '%-15s %s\n' "$name" "$what"; done
  exit 0
fi
wanted() { [ -z "$only" ] || [[ ",$only," == *",$1,"* ]]; }

stamp=${OVERNIGHT_STAMP:-$(date -u +%Y-%m-%dT%H%M)}
machine=$(uname -m)
out_dir=$ROOT/target/overnight/$stamp
mkdir -p "$out_dir"
out=$out_dir/$machine.md
log=$out_dir/$machine.log
load1() { cut -d' ' -f1 /proc/loadavg 2>/dev/null || sysctl -n vm.loadavg | tr -d '{}' | awk '{print $1}'; }
note() { echo "overnight[$machine]: $*" | tee -a "$log" >&2; }

# What makes this machine's numbers contended besides its load.
busy_with() {
  local why=""
  pgrep -f 'tla2tools\.ja[r]' >/dev/null 2>&1 && why="a TLC job runs"
  if command -v virsh >/dev/null 2>&1 && virsh list --state-running --name 2>/dev/null | grep -q .; then
    why="${why:+$why; }a VM runs"
  fi
  echo "$why"
}

# Wait for the load to fall below max_load, up to wait_min minutes; the
# load it ran at, and why it is contended if it is.
wait_idle() {
  local deadline=$(( $(date +%s) + wait_min * 60 )) l
  while :; do
    l=$(load1)
    if awk -v l="$l" -v m="$max_load" 'BEGIN { exit !(l < m) }'; then
      break
    fi
    if [ "$(date +%s)" -ge "$deadline" ]; then
      note "load $l still at or above $max_load after $wait_min min: running anyway, marked contended"
      break
    fi
    sleep 30
  done
  load1
}

build() {
  note "building (release)"
  cargo build --release -q -p bulkhead-runner -p bulkhead-fixture-native -p bulkhead-fixture-sdk \
    -p bulkhead-egress -p bulkhead-sql -p bulkhead-ingress 2>&1 | tee -a "$log" | tail -3 || return 1
  cargo build --release -q -p bulkhead-host --examples 2>&1 | tee -a "$log" | tail -3 || return 1
  (cd spikes/s37 && cargo build --release -q -p s37-bench -p s37-app) 2>&1 | tee -a "$log" | tail -3 || return 1
}

remote() {
  note "syncing to $pi:$pi_dir and starting there"
  rsync -rlpc --exclude target --exclude .git --exclude fuzz/corpus ./ "$pi:$pi_dir/" >>"$log" 2>&1 || {
    note "sync to $pi failed"; return 1; }
  local args="--local --rounds $rounds --max-load $max_load --wait $wait_min"
  [ -n "$only" ] && args="$args --only $only"
  # shellcheck disable=SC2029 # expanded here, on purpose.
  ssh "$pi" "cd $pi_dir && export PATH=\$HOME/.cargo/bin:\$PATH OVERNIGHT_STAMP=$stamp OVERNIGHT_COMMIT='$commit' && \
    $pi_wrap bash scripts/overnight.sh $args" >"$out_dir/remote.out" 2>&1
  local status=$?
  rsync -rlpc "$pi:$pi_dir/target/overnight/$stamp/" "$out_dir/" >>"$log" 2>&1 \
    || note "copying the results back from $pi failed"
  note "the remote side ended (status $status)"
}

# The remote side has no .git: the local side passes its commit across.
commit=${OVERNIGHT_COMMIT:-$(git rev-parse --short HEAD 2>/dev/null)$(git diff --quiet HEAD -- crates spikes 2>/dev/null || echo ' with local changes')}
cpu=$(grep -m1 'model name' /proc/cpuinfo 2>/dev/null | cut -d: -f2- | sed 's/^ *//')
[ -z "$cpu" ] && cpu=$(tr -d '\0' </proc/device-tree/model 2>/dev/null || sysctl -n hw.model 2>/dev/null)
{
  echo "## $(date -u +%Y-%m-%d): overnight baselines, $machine (scripts/overnight.sh)"
  echo
  echo "$cpu, $(nproc) threads, $(uname -sr); $(rustc --version | cut -d' ' -f1-2); commit $commit; $rounds rounds; a run waits for a 1-minute load under $max_load (up to $wait_min min); load $(load1) at the start."
} >"$out"

pi_job=""
if [ "$local" = 0 ] && [ -n "$pi" ]; then
  remote &
  pi_job=$!
fi
build || { note "the build failed: see $log"; exit 1; }

for r in $(seq "$rounds"); do
  for b in "${BENCHES[@]}"; do
    IFS='|' read -r name what env cmd <<<"$b"
    wanted "$name" || continue
    load=$(wait_idle)
    why=$(busy_with)
    awk -v l="$load" -v m="$max_load" 'BEGIN { exit !(l >= m) }' && why="${why:+$why; }load $load"
    note "round $r: $name (load $load${why:+; contended: $why})"
    {
      echo
      echo "### round $r, $name: $what"
      echo
      echo "Load $load at the start${why:+. Contended: $why}."
    } >>"$out"
    timer=()
    [ -x /usr/bin/time ] && timer=(/usr/bin/time -f '%e s wall, %U s user, %S s system, %M KiB peak RSS')
    echo "\$ $env $cmd" >>"$log"
    # shellcheck disable=SC2086 # $env is words on purpose.
    if result=$(env $env "${timer[@]}" bash -c "$cmd" 2>&1 | tee -a "$log"); then
      { echo; echo '```text'; echo "$result" | grep -v '^\s*$'; echo '```'; } >>"$out"
    else
      note "$name failed in round $r (see $log)"
      { echo; echo "Failed: \`$env $cmd\` (the log has its output)."; } >>"$out"
    fi
  done
done
note "this machine is done: $out"
if [ -n "$pi_job" ]; then
  note "waiting for the remote side"
  wait "$pi_job"
fi
note "results in $out_dir"
