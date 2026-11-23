#!/bin/sh
# S37's measurements, from the main checkout, after
#   cargo build --release -p bulkhead-runner
#   (cd spikes/s37 && cargo build --release -p s37-bench -p s37-app)
# (and `-p s37-driver` or `-p s37-driver-da1` for those drivers)
# On an idle machine only. Each row alternates the node's ingress and the
# driver; ROUNDS rounds; SECS seconds a run after a second's warm-up.
#   sh spikes/s37/scripts/run.sh > report.txt
set -eu
ROUNDS=${ROUNDS:-2}
SECS=${SECS:-10}
POOL=${POOL:-4}
DRIVERS=${DRIVERS:-4}
WORKERS=${WORKERS:-4}
# DRIVER=s37-driver-da1 S37_DRIVER=da1: DA1's driver (one process, DRIVERS
# worker threads, its own accepts, calls by name in flight) in place of
# S37's connection handler.
# DRIVER=target/release/bulkhead-ingress S37_DRIVER=da1 S37_INGRESS=1: the
# HTTP ingress driver itself (ROADMAP DA2), given a path.
DRIVER=${DRIVER:-s37-driver}
S=spikes/s37/target/release
case $DRIVER in */*) DRIVER_PATH=$DRIVER ;; *) DRIVER_PATH=$S/$DRIVER ;; esac
LOG=$(mktemp)
S37_WORKERS=$WORKERS $S/s37-bench node target/release/bulkhead-runner $S/s37-app $DRIVER_PATH $POOL $DRIVERS > "$LOG" 2>&1 &
NODE=$!
trap 'kill $NODE 2>/dev/null; rm -f "$LOG"' EXIT
for _ in $(seq 300); do grep -q '^ready' "$LOG" && break; sleep 0.1; done
L=$(grep '^ready' "$LOG")
get() { echo "$L" | tr ' ' '\n' | grep "^$1=" | cut -d= -f2; }
echo "# S37 run: $(uname -srm), $(nproc) CPUs, pool $POOL, drivers $DRIVERS ($DRIVER), $WORKERS node and client workers"
echo "# start load: $(cut -d' ' -f1-3 /proc/loadavg)"
row() { # path-pair label conns body tls
    for side in ingress driver; do
        if [ "$4" = tls ]; then a=$(get $side-https); else a=$(get $side-http); fi
        printf '%s %s ' "$1" "$side"
        $S/s37-bench load "$a" "$2" "$SECS" "$3" "$4" $NODE "$WORKERS"
    done
}
# HTTP/2 (ROADMAP DA2b): N streams on one connection, through the ingress
# driver alone (the node's former ingress speaks HTTP/1.1 only), beside
# the TLS rows' N HTTP/1.1 connections.
h2row() { # label streams body
    printf '%s driver ' "$1"
    $S/s37-bench load "$(get driver-https)" "$2" "$SECS" "$3" h2 $NODE "$WORKERS"
}
for r in $(seq "$ROUNDS"); do
    echo "## round $r (load $(cut -d' ' -f1 /proc/loadavg))"
    for c in 1 32 256; do row "GET" $c 0 plain; done
    for c in 1 32 256; do row "GET" $c 0 tls; done
    for b in 1024 65536 1048576; do for c in 1 32; do row "POST" $c $b plain; done; done
    if [ "${S37_INGRESS:-0}" = 1 ]; then
        for c in 1 32 256; do h2row "GET-h2" $c 0; done
        for b in 65536 1048576; do for c in 1 32; do h2row "POST-h2" $c $b; done; done
    fi
done
