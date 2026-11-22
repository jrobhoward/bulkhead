#!/usr/bin/env bash
# App tutorial, chapter 8: the chat with a disaster-recovery site. Site 1
# (the pair) is lost; the DR node is activated and carries on from its
# copy; then site 1 comes back and takes over again. See
# examples/app-tutorial/README.md.
#
# PORT_1, PORT_2, PORT_3 (default 7521 to 7523): the nodes' cluster ports.
source "$(dirname "$0")/common.sh"
source "$TUT/cluster.sh"
PORT_1=${PORT_1:-7521} PORT_2=${PORT_2:-7522} PORT_3=${PORT_3:-7523}

ROOM=$(build_app tut-room-5)
HISTORY=$(build_app tut-history-5)
cat > policy.toml <<'TOML'
[apps.room]
grant = ["bulkhead:logging", "bulkhead:metrics", "bulkhead:groups", "bulkhead:keyvalue"]
buckets = ["posts"]

[apps.history]
grant = ["bulkhead:logging", "bulkhead:groups", "bulkhead:keyvalue"]
buckets = ["posts"]
TOML
# Node 3, at site 2, is the witness and the DR node.
cluster_configs dr
start_node_n 3
start_node_n 1
start_node_n 2
until_ok 30 role 1 primary
until_ok 30 role 2 standby
# Deployed to the pair only: the DR node takes the primary's deployments
# and holds them, not started.
for n in 1 2; do
  deploy_n "$n" "$TUT/07-pair/history.toml" "$HISTORY"
  deploy_n "$n" "$TUT/07-pair/room.toml" "$ROOM"
done
until_ok 30 chatting 1
holds_apps() { bn 3 --json dr | grep -q '"apps": 2'; }
until_ok 60 holds_apps

pause "chat at site 1; node 3 keeps a copy, a poll behind"
showbn 1 call room "post hello"
bn 1 call room "post again" >/dev/null
until_ok 20 seen_n 1 2
commit_of() { grep -o '"commit": [0-9]*' | head -n 1 | grep -o '[0-9]*$'; }
dr_caught_up() {
  local primary copy
  primary=$(bn 1 --json failover | commit_of)
  copy=$(bn 3 --json dr | commit_of)
  [ -n "$primary" ] && [ -n "$copy" ] && [ "$copy" -ge "$primary" ]
}
until_ok 30 dr_caught_up
showbn 3 dr

pause "lose site 1: kill -9 both nodes"
kill_node 1 2
echo "Node 3 never activates by itself: only an operator can tell a lost site"
echo "from a cut-off one."
showbn 3 dr activate --site-1-down
until_ok 60 chatting 3
showbn 3 call room recent
expect "recent on node 3" "$(bn 3 call room recent)" $'hello\nagain'
showbn 3 logs room -n 1
showbn 3 call room seen
expect "posts seen again on node 3" "$(bn 3 call room seen)" 0
bn 3 call room "post from site 2" >/dev/null
until_ok 20 seen_n 3 1

pause "site 1 comes back, reseeds from node 3, and takes over again"
start_node_n 1
start_node_n 2
showbn 3 dr failback --to 1
until_ok 60 role 1 primary
until_ok 30 chatting 1
showbn 1 call room recent
expect "recent on node 1" "$(bn 1 call room recent)" $'hello\nagain\nfrom site 2'
showbn 1 call room seen
expect "posts seen again on node 1" "$(bn 1 call room seen)" 0
