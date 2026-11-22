#!/bin/sh
# run-linux.sh: as root, after setup-linux.sh, with target/release/s40
# built as a user. The probe under each fence, MQTT, then the benchmarks
# in two rounds alternating fences. Output on stdout.
set -eu
here=$(cd "$(dirname "$0")/.." && pwd)
opt=/opt/s40
install -d -m 755 $opt $opt/rules
install -m 755 "$here/target/release/s40" $opt/s40
install -m 644 "$here"/rules/*.toml $opt/rules/
sh "$here/scripts/bench-rules.sh" 127.0.0.1 $opt/rules
chmod 644 $opt/rules/*
echo "# $(uname -srm), $(date -u +%FT%TZ), load $(cut -d' ' -f1-3 /proc/loadavg)"

launch() { fence=$1; net=$2; rules=$3; shift 3; $opt/s40 launch --uid 65534 --fence "$fence" --landlock-net "$net" --rules "$opt/rules/$rules.toml" --allowed 127.0.0.1 --other 127.0.0.2 --self 127.0.0.1 --v6 -- "$@"; }

for f in "none off" "none on" "bpf off" "bpf on" "nft off"; do
  set -- $f
  echo "## probe: fence $1, landlock net $2"
  launch "$1" "$2" probe-linux probe --allowed 127.0.0.1 --other 127.0.0.2 --self 127.0.0.1 --v6
done
for f in none bpf nft; do
  echo "## mqtt: fence $f"
  launch "$f" off probe-linux mqtt --broker 127.0.0.1:1883 --refused 127.0.0.1:1884
done
for round in 1 2; do
  for f in "none off 2" "bpf off 2" "bpf off 16" "bpf off 64" "nft off 2" "nft off 16" "nft off 64" "bpf on 2"; do
    set -- $f
    echo "## bench round $round: fence $1, landlock net $2, $3 rules"
    launch "$1" "$2" "bench-$3" bench --target 127.0.0.1
  done
  echo "## bench round $round: sockets made by the launcher, passed"
  launch none off bench-2 passed --target 127.0.0.1
done
echo "# done, load $(cut -d' ' -f1-3 /proc/loadavg)"
