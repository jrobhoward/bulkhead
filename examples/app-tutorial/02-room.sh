#!/usr/bin/env bash
# App tutorial, chapter 2: a first app, the chat room, built, bundled,
# deployed and called. See examples/app-tutorial/README.md.
source "$(dirname "$0")/common.sh"

ROOM=$(build_app tut-room-2)
node_config
start_node

echo "\$ bulkhead bundle 02-room/bulkhead.toml target/debug/tut-room-2 --key signer.key"
echo "\$ bulkhead deploy 02-room-bulkhead.rbp"
deploy "$TUT/02-room/bulkhead.toml" "$ROOM"
echo

showb call room "post hello"
expect "first post" "$(b call room 'post again')" "#2"
showb call room recent
expect "recent" "$(b call room recent)" $'hello\nagain'

pause "kill the instance; its supervisor starts a new one, with empty memory"
pid=$(pid_of room)
showb kill "$pid"
until_ok 30 running room "$pid"
showb ps
showb call room recent
expect "recent after the restart" "$(b call room recent)" ""

pause "restart the node: the app is still deployed, and starts again"
stop_node
start_node
until_ok 30 running room
showb list
expect_has "apps" "$(b list)" "room              0.1.0     running"
