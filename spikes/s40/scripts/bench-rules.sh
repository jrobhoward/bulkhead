#!/bin/sh
# bench-rules.sh TARGET DIR: rules files of 2, 16 and 64 rules, the two
# the bench needs (TARGET ports 7001 and 7004) last, so a fence that
# checks in order walks every other rule first.
target=$1; dir=$2
for n in 2 16 64; do
  f=$dir/bench-$n.toml
  echo "connect = [" > "$f"
  i=1
  while [ "$i" -le "$((n - 2))" ]; do
    echo "  { cidr = \"10.$((i / 256)).$((i % 256)).0/24\", ports = \"8000-8100\" }," >> "$f"
    i=$((i + 1))
  done
  echo "  { cidr = \"$target/32\", ports = \"7001\" }," >> "$f"
  echo "  { cidr = \"$target/32\", ports = \"7004\" }," >> "$f"
  echo "]" >> "$f"
done
