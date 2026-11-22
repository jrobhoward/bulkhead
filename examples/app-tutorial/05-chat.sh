#!/usr/bin/env bash
# App tutorial, chapter 5: posts in a bucket that outlives instances and
# the node; the room watches it and resumes its watch after a restart. See
# examples/app-tutorial/README.md.
source "$(dirname "$0")/common.sh"

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
# The shared bucket, on disk.
node_config $'[buckets.posts]\ndurability = "disk"'
start_node
deploy "$TUT/05-history/bulkhead.toml" "$HISTORY"
deploy "$TUT/05-room/bulkhead.toml" "$ROOM"
echo

pause "history writes each post to the bucket; the room's watch sees it"
showb call room "post hello"
b call room "post again" >/dev/null
seen() { [ "$(b call room seen)" = "$1" ]; }
until_ok 20 seen 2
showb call room recent
expect "recent" "$(b call room recent)" $'hello\nagain'
showb logs room
expect_has "room's log" "$(b logs room)" "new post p000002: again"

pause "history's instance restarts; the posts are in the bucket, not in it"
pid=$(pid_of history)
showb kill "$pid"
until_ok 30 running history "$pid"
showb call room recent
expect "recent after history restarted" "$(b call room recent)" $'hello\nagain'

pause "stop the room; a post arrives while it is down (cast to history directly)"
showb stop room
showb cast history "said while you were away"
start_and_wait() { b start room >/dev/null && until_ok 30 running room; }
echo "\$ bulkhead start room"
start_and_wait
# The new instance resumed after the revision the last one saved: it sees
# the one post it missed, not the two before.
until_ok 20 seen 1
showb call room seen
showb logs room -n 2
expect_has "room's log" "$(b logs room -n 2)" "new post p000003: while you were away"

pause "restart the node: the bucket is on disk"
stop_node
start_node
until_ok 30 running history
showb call room recent
expect "recent after the node restarted" "$(b call room recent)" $'hello\nagain\nwhile you were away'
