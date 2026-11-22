#!/bin/sh
# run-freebsd.sh: as root, after setup-freebsd.sh, with target/release/s40
# built as a user. Three phases: no packet filter loaded, pf (a jail with
# its own address, rules in an anchor), ipfw (a jail on the host's
# addresses, rules matching its id). Loads pf and ipfw and leaves them
# loaded: restore the VM's snapshot afterwards. Output on stdout.
set -eu
here=$(cd "$(dirname "$0")/.." && pwd)
opt=/opt/s40
install -d -m 755 $opt $opt/rules $opt/rules/lo1
install -m 755 "$here/target/release/s40" $opt/s40
install -m 644 "$here"/rules/*.toml $opt/rules/
sh "$here/scripts/bench-rules.sh" 127.0.0.1 $opt/rules
sh "$here/scripts/bench-rules.sh" 10.40.0.1 $opt/rules/lo1
chmod 644 $opt/rules/*.toml $opt/rules/lo1/*.toml
echo "# $(uname -srm), $(date -u +%FT%TZ), load $(sysctl -n vm.loadavg)"

# launch FENCE RULES ALLOWED OTHER SELF ROLE...
launch() { fence=$1; rules=$2; a=$3; o=$4; s=$5; shift 5; $opt/s40 launch --uid 65534 --fence "$fence" --rules "$opt/rules/$rules.toml" --allowed "$a" --other "$o" --self "$s" -- "$@"; }
LO="127.0.0.1 127.0.0.2 127.0.0.1"
LO1="10.40.0.1 10.40.0.3 10.40.0.2"

echo "## phase: no packet filter loaded"
echo "## probe: fence none"
launch none probe-linux $LO probe --allowed 127.0.0.1 --other 127.0.0.2 --self 127.0.0.1
echo "## mqtt: fence none"
launch none probe-linux $LO mqtt --broker 127.0.0.1:1883 --refused 127.0.0.1:1884
for round in 1 2; do
  echo "## bench round $round: fence none, 2 rules, lo0"
  launch none bench-2 $LO bench --target 127.0.0.1
  echo "## bench round $round: fence none, 2 rules, lo1"
  launch none lo1/bench-2 10.40.0.1 10.40.0.3 10.40.0.1 bench --target 10.40.0.1
  echo "## bench round $round: sockets made by the launcher, passed"
  launch none bench-2 $LO passed --target 127.0.0.1
done

echo "## phase: pf"
kldload -n pf
pfctl -e 2>/dev/null || true
printf 'pass all\nanchor "s40/*"\n' | pfctl -f -
echo "## probe: fence pf"
launch pf probe-pf $LO1 probe --allowed 10.40.0.1 --other 10.40.0.3 --self 10.40.0.2
echo "## mqtt: fence pf"
launch pf probe-pf $LO1 mqtt --broker 10.40.0.1:1883 --refused 10.40.0.1:1884
for round in 1 2; do
  for n in 2 16 64; do
    echo "## bench round $round: fence pf, $n rules"
    launch pf "lo1/bench-$n" $LO1 bench --target 10.40.0.1
  done
done
pfctl -d

echo "## phase: ipfw"
kenv net.inet.ip.fw.default_to_accept=1 >/dev/null
kldload -n ipfw
echo "## probe: fence ipfw"
launch ipfw probe-linux $LO probe --allowed 127.0.0.1 --other 127.0.0.2 --self 127.0.0.1
echo "## mqtt: fence ipfw"
launch ipfw probe-linux $LO mqtt --broker 127.0.0.1:1883 --refused 127.0.0.1:1884
for round in 1 2; do
  echo "## bench round $round: fence none, ipfw loaded"
  launch none bench-2 $LO bench --target 127.0.0.1
  for n in 2 16 64; do
    echo "## bench round $round: fence ipfw, $n rules"
    launch ipfw "bench-$n" $LO bench --target 127.0.0.1
  done
done
echo "# done, load $(sysctl -n vm.loadavg)"
