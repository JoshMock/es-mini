#!/usr/bin/env bash
# Summarizes commits since REV for review: trailers, protected paths touched, list counts,
# goals removed (with their Done-checks), files by churn, leftover containers, and the run logs.
# Usage: batch_summary.sh REV   (e.g. the commit the batch started from)
set -euo pipefail
rev=${1:?usage: batch_summary.sh REV}
cd "$(git rev-parse --show-toplevel)"
echo "== commits ($(git rev-list --count "$rev"..HEAD))"
git log --format='%h %s' "$rev"..HEAD
echo; echo "== trailers"
git log --format='%(trailers:key=Assisted-by,valueonly)' "$rev"..HEAD | sed '/^$/d' | sort | uniq -c
missing=$(git log --format='%h %(trailers:key=Assisted-by,valueonly)' "$rev"..HEAD | awk 'NF==1')
[ -z "$missing" ] || echo "MISSING TRAILER: $missing"
echo; echo "== protected paths touched (should be empty)"
git diff --name-only "$rev" HEAD -- z_old tests/yaml tests/yaml-out-of-scope.txt es-mini/tests/yaml.rs \
  fixtures/expected fixtures/es-9.5.5 fixtures/generate.sh es-mini/tests/fixture_parity.rs es-mini/tests/common
echo; echo "== lists (now; before)"
for l in tests/yaml-xfail.txt tests/yaml-out-of-scope.txt fixtures/parity-xfail.txt ralph/goals.txt; do
  now=$(grep -cvE '^#|^$' "$l" 2>/dev/null || true)
  before=$(git show "$rev:$l" 2>/dev/null | grep -cvE '^#|^$' || true)
  echo "$l: ${now:-0}; ${before:-0}"
done
echo; echo "== goals removed (verify each Done-check held)"
git diff "$rev" HEAD -- ralph/goals.txt | grep '^-[a-z]' | cut -c2-200 || true
echo; echo "== churn"
git diff --stat "$rev" HEAD | sort -t'|' -k2 -rn | head -15
echo; echo "== leftover containers (should be empty)"
docker ps -a --filter name=es-mini --format '{{.Names}} {{.Status}}' 2>/dev/null || echo "(docker unavailable)"
for f in ralph/REPORT.md ralph/NEEDS_HUMAN.md; do
  [ -s "$f" ] && { echo; echo "== $f (last 15 lines)"; tail -15 "$f" | cut -c1-220; }
done
true
