#!/usr/bin/env bash
# App tutorial, chapter 4: two apps talking, by publish, call and cast,
# under messaging ACLs. See examples/app-tutorial/README.md.
source "$(dirname "$0")/common.sh"

ROOM=$(build_app tut-room-4)
HISTORY=$(build_app tut-history-4)
cat > policy.toml <<'TOML'
[apps.room]
grant = ["bulkhead:logging", "bulkhead:metrics", "bulkhead:groups"]

[apps.history]
grant = ["bulkhead:logging", "bulkhead:groups"]

[apps.lurker]
grant = ["bulkhead:logging", "bulkhead:metrics", "bulkhead:groups"]
TOML
node_config
start_node
deploy "$TUT/04-history/bulkhead.toml" "$HISTORY"
deploy "$TUT/04-room/bulkhead.toml" "$ROOM"
echo

pause "a post is published to the group chat, which history joined"
showb call room "post hello"
expect "second post" "$(b call room 'post again')" "#2 to 1"
showb call room recent
expect "recent" "$(b call room recent)" $'hello\nagain'

pause "the lurker: the room's program under another name, which history does not accept"
deploy "$TUT/04-room/lurker.toml" "$ROOM"
showb call lurker recent
expect_has "lurker's call" "$(b call lurker recent)" "access denied"
showb call lurker "post spam"
showb call room recent
expect "recent without the lurker's post" "$(b call room recent)" $'hello\nagain'

pause "a cast: sent, with no answer"
showb call room clear
cleared() { [ -z "$(b call room recent)" ]; }
until_ok 20 cleared
showb call room recent

pause "history keeps the posts in memory: a restart of its instance loses them"
b call room "post kept?" >/dev/null
pid=$(pid_of history)
showb kill "$pid"
until_ok 30 running history "$pid"
showb call room recent
expect "recent after history restarted" "$(b call room recent)" ""
