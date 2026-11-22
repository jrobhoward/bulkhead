#!/usr/bin/env bash
# App tutorial, chapter 6: testing the app. The logic's unit tests, which
# need no node; the tests that run the app on one are in
# crates/bulkhead/tests/app_tutorial.rs. See examples/app-tutorial/README.md.
source "$(dirname "$0")/common.sh"

pause "the logic's unit tests: plain functions, no node"
echo "\$ cargo test -p tut-room-5 -p tut-history-5"
# Without --quiet, so each test is named.
flags=()
for f in "${CARGO_FLAGS[@]}"; do [ "$f" = --quiet ] || flags+=("$f"); done
out=$(cargo test "${flags[@]}" -p tut-room-5 -p tut-history-5 2>&1)
echo "$out" | grep -E "^test |test result"
expect_has "room's tests" "$out" "describe____a_new_post____its_key_and_text ... ok"
expect_has "history's tests" "$out" "key____consecutive_numbers____sort_in_order ... ok"
if echo "$out" | grep -q "FAILED"; then
  echo "a test failed" >&2
  exit 1
fi

echo
echo "The app on a node, and these chapters' scripts, from the repository root:"
echo "\$ cargo test -p bulkhead --test app_tutorial"
