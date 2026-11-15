#!/bin/sh
# Run examples/admin_cost.rs against a node built from BULKHEAD_BIN (with its
# runner, resolver and, since ROADMAP DA7, admin and metrics drivers beside
# it): a fresh data directory, an admin key, an operator, [admin] and
# [metrics] on loopback. On an idle machine.
#
#   sh crates/bulkhead/examples/admin_cost.sh LABEL BULKHEAD_BIN HARNESS [N]
set -eu
LABEL=$1 BIN=$2 HARNESS=$3 N=${4:-1000}
D=$(mktemp -d)
$BIN cluster keygen $D/admin.key >/dev/null
$BIN cluster keygen $D/operator.key >/dev/null
ADMIN_KEY=$($BIN cluster pubkey $D/admin.key)
printf '[operators]\nbench = ["%s"]\n' "$($BIN cluster pubkey $D/operator.key)" > $D/operators.toml
cat > $D/node.toml <<TOML
data_dir = "$D/data"
[admin]
address = "127.0.0.1:17443"
key = "$D/admin.key"
operators = "$D/operators.toml"
[metrics]
address = "127.0.0.1:19101"
TOML
$BIN node --config $D/node.toml >$D/node.log 2>&1 &
PID=$!
for i in $(seq 200); do [ -S $D/data/admin.sock ] && break; sleep 0.05; done
sleep 1
echo "## $LABEL"
$HARNESS 127.0.0.1:17443 "$ADMIN_KEY" $D/operator.key 19101 $PID $N
kill $PID; wait $PID 2>/dev/null || true
rm -rf $D
