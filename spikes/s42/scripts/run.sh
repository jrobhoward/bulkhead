#!/bin/sh
# S42's runs, from the main checkout after
#   cargo build --release -p bulkhead-native --bin bulkhead-runner
#   (cd spikes/s42 && cargo build --release)
#
#   sh spikes/s42/scripts/run.sh checks         # through the runner, as this user
#   sh spikes/s42/scripts/run.sh root-checks    # as root: two users; on Linux a cgroup each
#   sh spikes/s42/scripts/run.sh bench [N]      # two rounds of the cost table (N scales them)
#
# root-checks copies the two programs to a directory every user may read,
# since the apps run as users 61001 and 61002 (no accounts needed), and on
# Linux makes /sys/fs/cgroup/s42-<pid> with a cgroup for the writer and one
# for the reader, removed at the end.
set -eu
ROOT=$(cd "$(dirname "$0")/../../.." && pwd)
RUNNER=${RUNNER:-$ROOT/target/release/bulkhead-runner}
S42=$ROOT/spikes/s42/target/release/s42
case "${1:-checks}" in
checks)
    "$S42" checks --runner "$RUNNER"
    ;;
root-checks)
    D=$(mktemp -d /tmp/s42-root.XXXXXX)
    chmod 0755 "$D"
    install -m 0755 "$RUNNER" "$D/bulkhead-runner"
    install -m 0755 "$S42" "$D/s42"
    CG=""
    if [ "$(uname)" = Linux ]; then
        CG=/sys/fs/cgroup/s42-$$
        mkdir "$CG"
        CG="--cgroup $CG"
    fi
    # shellcheck disable=SC2086
    "$D/s42" checks --runner "$D/bulkhead-runner" --users 61001,61002 $CG
    [ -n "$CG" ] && rmdir /sys/fs/cgroup/s42-$$ 2>/dev/null || true
    rm -rf "$D"
    ;;
bench)
    N=${2:-1}
    for round in 1 2; do
        echo "== round $round, $(date -Iseconds), load $(cut -d' ' -f1-3 /proc/loadavg)"
        "$S42" bench --runner "$RUNNER" --count "$N"
    done
    ;;
*)
    echo "usage: run.sh checks | root-checks | bench [N]" >&2
    exit 2
    ;;
esac
