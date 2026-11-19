#!/usr/bin/env bash
# Runs configs one after another, exhaustively, with checkpoints, into
# ~/tla-runs/s38-<date> (S38_RUNS overrides). DrF_* configs check
# DrFailover.tla, DrR_* DrReplication.tla. Usage: run.sh CONFIG...
# S38_SIMULATE=<minutes> runs random simulation instead, for that long.
set -euo pipefail
cd "$(dirname "$0")"
out=${S38_RUNS:-$HOME/tla-runs/s38-$(date +%F)}
mkdir -p "$out"
for name in "$@"; do
    case $name in
        DrF_*) spec=DrFailover.tla ;;
        DrR_*) spec=DrReplication.tla ;;
        *) echo "unknown config $name" >&2; exit 2 ;;
    esac
    mode=(-checkpoint 30 -metadir "$out/$name")
    if [ -n "${S38_SIMULATE:-}" ]; then
        mode=(-simulate -depth 400 -metadir "$out/$name-sim")
    fi
    if [ -n "${S38_SIMULATE:-}" ]; then
        timeout --signal=INT "$((S38_SIMULATE * 60))" \
            java -XX:+UseParallelGC -Xmx"${S38_HEAP:-40g}" -jar "$HOME/tla/tla2tools.jar" \
            -workers "${S38_WORKERS:-12}" -deadlock -noGenerateSpecTE "${mode[@]}" \
            -config "$name.cfg" "$spec" > "$out/$name-sim.log" 2>&1 || true
        log=$out/$name-sim.log
    else
        java -XX:+UseParallelGC -Xmx"${S38_HEAP:-40g}" -jar "$HOME/tla/tla2tools.jar" \
            -workers "${S38_WORKERS:-12}" -deadlock -noGenerateSpecTE "${mode[@]}" \
            -config "$name.cfg" "$spec" > "$out/$name.log" 2>&1 || true
        log=$out/$name.log
    fi
    result=$(grep -E 'is violated|No error has been found|Error:' "$log" | head -1 || true)
    states=$(grep -E 'distinct states found|states generated' "$log" | tail -1 || true)
    echo "$name: $result | $states" | tee -a "$out/summary.txt"
done
