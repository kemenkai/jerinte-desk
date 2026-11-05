#!/usr/bin/env bash
# Looks for what must never reach this public repository (where our servers are, their path
# prefixes, internal addresses and names) in the tree and in the messages of the commits given.
# The patterns are extended regexes, one per line, in a file kept out of the repository: the list
# itself would tell what to look for. Prints where, never what: file:line and commit ids only.
#   leak_scan.sh PATTERN_FILE [REV_RANGE]     exit 1 on a hit
set -euo pipefail
patterns=$1 range=${2:-}
if [ ! -s "$patterns" ]; then
  echo "leak_scan: no patterns in $patterns" >&2
  exit 2
fi
found=0
hits=$(git grep -n -I -E -f "$patterns" -- . | cut -d: -f1,2 || true)
if [ -n "$hits" ]; then
  echo "leak_scan: the tree has something that must not be public, at:"
  echo "$hits"
  found=1
fi
if [ -n "$range" ]; then
  for c in $(git rev-list "$range"); do
    if git log -1 --format=%B "$c" | grep -q -E -f "$patterns"; then
      echo "leak_scan: the message of commit $c has something that must not be public"
      found=1
    fi
  done
fi
exit $found
