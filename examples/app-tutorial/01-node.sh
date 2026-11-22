#!/usr/bin/env bash
# App tutorial, chapter 1: a node, started and stopped. See
# examples/app-tutorial/README.md.
#
#   examples/app-tutorial/01-node.sh          # pauses between steps
#   AUTO=1 examples/app-tutorial/01-node.sh   # runs straight through
source "$(dirname "$0")/common.sh"

node_config
cat node.toml
echo "\$ bulkhead node --config node.toml       # in the foreground; here in the background"
start_node
showb list
# No app yet: the one line is the node's own resolver of host names, a
# driver it starts itself (marked built-in).
expect_has "apps" "$(b list)" "bulkhead-resolve  built-in  running"

pause "stop it with SIGTERM, as Ctrl-C (SIGINT) or a service manager would"
echo "\$ kill -TERM $NODE_PID"
status=0
stop_node || status=$?
echo "the node exited with status $status"
expect "exit status" "$status" 0
tail -n 3 node.log
echo

pause "what it kept"
show ls "$DEMO_DIR/data"
# `creation` is raised at every start and goes into every pid.
expect "creation" "$(cat data/creation)" 1

pause "start it again on the same data"
start_node
showb list
expect_has "apps after the restart" "$(b list)" "bulkhead-resolve  built-in  running"
show cat "$DEMO_DIR/data/creation"
expect "creation after the restart" "$(cat data/creation)" 2
