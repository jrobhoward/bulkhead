#!/usr/bin/env bash
# App tutorial, chapter 7: the chat on an HA pair. The primary dies, the
# standby carries on with the posts and the room's place in them; then the
# old primary rejoins and takes over again. See
# examples/app-tutorial/README.md.
#
# PORT_1, PORT_2, PORT_3 (default 7501 to 7503): the nodes' cluster ports.
source "$(dirname "$0")/common.sh"
source "$TUT/cluster.sh"
PORT_1=${PORT_1:-7501} PORT_2=${PORT_2:-7502} PORT_3=${PORT_3:-7503}

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
cluster_configs
start_node_n 3
start_node_n 1
start_node_n 2
until_ok 30 role 1 primary
until_ok 30 role 2 standby
# Deployed to both nodes of the pair; as singletons they run on the
# primary only.
for n in 1 2; do
  deploy_n "$n" "$TUT/07-pair/history.toml" "$HISTORY"
  deploy_n "$n" "$TUT/07-pair/room.toml" "$ROOM"
done
until_ok 30 chatting 1
showbn 2 list
expect_has "node 2's apps" "$(bn 2 list)" "room              0.5.0     standby"

pause "chat on node 1, the primary"
showbn 1 call room "post hello"
bn 1 call room "post again" >/dev/null
until_ok 20 seen_n 1 2
showbn 1 call room recent
showbn 1 failover

pause "kill -9 node 1, as a power cut would"
kill_node 1
start=$(now_ms)
until_ok 60 role 2 primary
until_ok 30 chatting 2
echo "node 2 runs the apps $(($(now_ms) - start)) ms later"
showbn 2 call room recent
expect "recent on node 2" "$(bn 2 call room recent)" $'hello\nagain'
showbn 2 logs room -n 1
expect_has "node 2's room" "$(bn 2 logs room -n 1)" "watching posts after revision"
# Its room resumed after the revision node 1's room saved: nothing again.
showbn 2 call room seen
expect "posts seen again on node 2" "$(bn 2 call room seen)" 0
bn 2 call room "post from node 2" >/dev/null
until_ok 20 seen_n 2 1
showbn 2 logs room -n 1

pause "node 1 comes back, catches up as the standby, and takes over again"
start_node_n 1
until_ok 60 role 1 standby
caught_up() { [[ "$(bn 2 failover)" != *"committing alone"* ]]; }
until_ok 60 caught_up
# Refused (and nothing changed) until node 2 hears node 1 as a standby
# that confirms; asked again until it is accepted.
switch_back() { bn 2 failover switchover > switchover.out 2>&1; }
echo "\$ bulkhead failover switchover        # node 2"
until_ok 60 switch_back
cat switchover.out
echo
until_ok 60 role 1 primary
until_ok 30 chatting 1
showbn 1 call room recent
expect "recent on node 1" "$(bn 1 call room recent)" $'hello\nagain\nfrom node 2'
showbn 1 call room seen
expect "posts seen again on node 1" "$(bn 1 call room seen)" 0
bn 1 call room "post back home" >/dev/null
until_ok 20 seen_n 1 1
showbn 1 logs room -n 1
