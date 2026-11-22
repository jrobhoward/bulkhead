#!/usr/bin/env bash
# App tutorial, chapter 3: the room logs what it does and counts its
# posts; reading both. See examples/app-tutorial/README.md.
source "$(dirname "$0")/common.sh"

ROOM=$(build_app tut-room-3)
# The operator grants what the manifest asks for.
cat > policy.toml <<'TOML'
[apps.room]
grant = ["bulkhead:logging", "bulkhead:metrics"]
TOML
node_config
start_node
deploy "$TUT/03-room/bulkhead.toml" "$ROOM"
b call room "post hello" >/dev/null
b call room "post again" >/dev/null
b call room "shout" >/dev/null

pause "the room's log records"
showb logs room
logs=$(b logs room)
expect_has "logs" "$logs" "room open"
expect_has "logs" "$logs" "post #2: again"
expect_has "logs" "$logs" "not understood"

pause "debug records appear once the level is lowered"
showb log-level room debug
# The new level reaches the instance as a host event, soon after.
debug_logged() {
  b call room recent >/dev/null
  b logs room -n 1 | grep -q "recent: 2 posts"
}
until_ok 20 debug_logged
showb logs room -n 1
expect_has "debug record" "$(b logs room -n 1)" "recent: 2 posts"
b log-level room reset >/dev/null

pause "the counter, among the node's metrics"
echo "\$ bulkhead metrics | grep room_"
b metrics | grep room_
expect_has "metric" "$(b metrics)" 'room_posts_total{app="room"} 2'

pause "the app in detail"
showb status room
