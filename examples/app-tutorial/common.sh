# Shared by the app tutorial's chapters; sourced, not run.
#
# Sets ROOT, TUT, DEMO_DIR, BIN and BULKHEAD; builds bulkhead, the runner
# and the resolver beside it (debug builds; PROFILE=release for release
# ones, CARGO_TARGET_DIR for another target directory); makes a signing
# key and starts no node. Defines:
#   sock             the node's admin socket
#   b ARGS...        the bulkhead CLI against the node
#   build_app PKG    build one of the tutorial's crates; print its program
#   node_config [EXTRA]   write node.toml (EXTRA: TOML appended)
#   start_node       start `bulkhead node` on node.toml, in the background
#   stop_node        send it SIGTERM and wait for it to exit
#   deploy MANIFEST PROGRAM   bundle, sign and deploy
#   pid_of APP       the pid of APP's first instance
#   running APP [OLD]         whether APP has an instance (other than OLD)
#   until_ok SECS CMD...      retry CMD until it succeeds
#   show CMD...      print a command, then run it
#   showb ARGS...    the same for `bulkhead ARGS`
#   expect WHAT GOT WANT      stop the chapter if GOT is not WANT
#   expect_has WHAT GOT PART  stop the chapter if GOT lacks PART
#   pause TEXT       wait for Enter (skipped with AUTO=1)
set -euo pipefail
shopt -s inherit_errexit

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
TUT="$ROOT/examples/app-tutorial"
DEMO_DIR=${DEMO_DIR:-$(mktemp -d "${TMPDIR:-/tmp}/bulkhead-app-tutorial-XXXXXX")}
AUTO=${AUTO:-0}
PROFILE=${PROFILE:-debug}
TARGET=${CARGO_TARGET_DIR:-$ROOT/target}
BIN="$TARGET/$PROFILE"
BULKHEAD="$BIN/bulkhead"
CARGO_FLAGS=(--quiet --manifest-path "$ROOT/Cargo.toml" --target-dir "$TARGET")
[ "$PROFILE" = release ] && CARGO_FLAGS+=(--release)

echo "building bulkhead ($PROFILE; the first build takes a few minutes)..."
cargo build "${CARGO_FLAGS[@]}" -p bulkhead
# The node runs each app through the bulkhead-runner beside it, and starts
# its resolver of host names (bulkhead-resolve) from there too.
cargo build "${CARGO_FLAGS[@]}" -p bulkhead-runner
cargo build "${CARGO_FLAGS[@]}" -p bulkhead-resolve

# Called as PROGRAM=$(build_app PKG): a command substitution, where
# `set -e` does not stop the function, so a failed build returns here.
build_app() {
  cargo build "${CARGO_FLAGS[@]}" -p "$1" || return 1
  echo "$BIN/$1"
}

cd "$DEMO_DIR"
[ -f signer.key ] || "$BULKHEAD" keygen signer --no-password >/dev/null
SIGNER=$(tail -n 1 signer.pub)

# Unix socket paths are limited to about 107 bytes; keep this one short.
sock() { echo "/tmp/bulkhead-apptut-$$.sock"; }
b() { "$BULKHEAD" --socket "$(sock)" "$@"; }

node_config() {
  cat > node.toml <<TOML
node = 1
data_dir = "$DEMO_DIR/data"
admin_socket = "$(sock)"
trusted_keys = ["$SIGNER"]
policy = "$DEMO_DIR/policy.toml"
${1:-}
TOML
  [ -f policy.toml ] || : > policy.toml
}

NODE_PID=
start_node() {
  "$BULKHEAD" node --config "$DEMO_DIR/node.toml" >> "$DEMO_DIR/node.log" 2>&1 &
  NODE_PID=$!
  for _ in $(seq 600); do [ -S "$(sock)" ] && return; sleep 0.05; done
  echo "the node did not start; see $DEMO_DIR/node.log" >&2
  exit 1
}

stop_node() {
  [ -n "$NODE_PID" ] || return 0
  kill -TERM "$NODE_PID" 2>/dev/null || true
  local status=0
  wait "$NODE_PID" || status=$?
  NODE_PID=
  return "$status"
}

cleanup() {
  stop_node || true
  rm -f "$(sock)"
  echo
  echo "node stopped; its config, data and log are in $DEMO_DIR"
}
trap cleanup EXIT

deploy() {
  local manifest=$1 program=$2 name
  name=$(basename "$(dirname "$manifest")")-$(basename "$manifest" .toml)
  "$BULKHEAD" bundle "$manifest" "$program" --key signer.key --out "$name.rbp" >/dev/null
  b deploy "$name.rbp"
}

pid_of() { b ps | awk -v app="$1" '$2 == app { print $1; exit }'; }
running() {
  local pid
  pid=$(pid_of "$1")
  [ -n "$pid" ] && [ "$pid" != "${2:-}" ]
}

until_ok() {
  local deadline=$(($(date +%s) + $1))
  shift
  until "$@" >/dev/null 2>&1; do
    if [ "$(date +%s)" -ge "$deadline" ]; then
      echo "timed out waiting for: $*" >&2
      exit 1
    fi
    sleep 0.05
  done
}

show() {
  echo "\$ $*"
  "$@"
  echo
}

showb() {
  echo "\$ bulkhead $*"
  b "$@"
  echo
}

expect() {
  if [ "$2" != "$3" ]; then
    printf 'unexpected %s:\n  got:  %q\n  want: %q\n' "$1" "$2" "$3" >&2
    exit 1
  fi
}

expect_has() {
  case "$2" in
  *"$3"*) ;;
  *)
    printf 'unexpected %s: %q lacks %q\n' "$1" "$2" "$3" >&2
    exit 1
    ;;
  esac
}

pause() {
  echo
  if [ "$AUTO" = 1 ]; then
    echo "-- $1"
  else
    read -r -p "-- $1 [Enter] "
  fi
}
