#!/usr/bin/env bash
# Done-check for a goal made from ralph/ponytail-ledger.md: none of the goal's `ponytail:` markers
# remain in the code (fixed, or rewritten to name a smaller ceiling), and its tests pass: tests
# whose names start with the goal name in snake case, at least one per marker.
#   ralph/ledger-check.sh date-formats
set -euo pipefail
cd "$(dirname "$0")/.."
goal=$1 prefix=${1//-/_}_ fail=0 n=0
while IFS= read -r text; do
  n=$((n + 1))
  # The whole first line: a 40-character prefix let goals with similar markers block each other.
  if rg -qF --type rust "ponytail: $text" .; then echo "still marked: ${text:0:80}"; fail=1; fi
done < <(sed -n "s/^- \`[^\`]*\` \*\*$goal\*\*: //p" ralph/ponytail-ledger.md)
[ "$n" -gt 0 ] || { echo "no markers for goal $goal"; exit 2; }
tests=$(rg -c --type rust "fn ${prefix}" . | awk -F: '{s+=$2} END {print s+0}')
[ "$tests" -ge "$n" ] || { echo "$tests tests named ${prefix}*, need at least $n"; fail=1; }
[ "$fail" -eq 0 ] && cargo test -q --workspace "$prefix"
