#!/usr/bin/env bash
# Times ES|QL right after a write (the cold path) on a release build of the working tree with the
# 100k bench dataset, and fails if any bench ES|QL request's median exceeds MAX_MS (default 40).
#   ralph/esql-cold.sh            # MAX_MS=40 RUNS=7
set -euo pipefail
cd "$(dirname "$0")/.."
nice cargo build -q --release -j "${JOBS:-2}" -p es-mini
bin="$(cargo metadata --format-version 1 --no-deps | python3 -c 'import json,sys;print(json.load(sys.stdin)["target_directory"])')/release/es-mini"
port=$((19500 + RANDOM % 400)) H='content-type: application/json' work=$(mktemp -d /tmp/esm-cold.XXXXXX)
ES_MINI_DATA=$work/data ES_MINI_PORT=$port "$bin" >/dev/null 2>&1 & srv=$!
trap 'kill $srv 2>/dev/null; wait $srv 2>/dev/null; rm -rf "$work"' EXIT
until curl -s "localhost:$port" >/dev/null; do sleep 0.2; done
curl -sf -X PUT "localhost:$port/bench" -H "$H" --data-binary @bench/mapping.json >/dev/null
bench/gen_data.py 100000 | split -l 4000 - "$work/c"
for c in "$work"/c*; do
  curl -sf -X POST "localhost:$port/bench/_bulk" -H 'content-type: application/x-ndjson' --data-binary "@$c" -o /dev/null
done
curl -sf -X POST "localhost:$port/bench/_refresh" >/dev/null
fail=0
while IFS='|' read -r name _ path body; do
  printf '%s' "$body" > "$work/q.json"
  ms=$(for i in $(seq "${RUNS:-7}"); do
    curl -sf -X PUT "localhost:$port/bench/_doc/cold-$i?refresh=true" -H "$H" -d '{"views":1}' -o /dev/null
    curl -sf -o /dev/null -w '%{time_total}\n' -X POST "localhost:$port$path" -H "$H" --data-binary "@$work/q.json"
  done | sort -n | awk '{a[NR]=$1} END {printf "%.1f", a[int((NR+1)/2)]*1000}')
  over=$(awk -v m="$ms" -v x="${MAX_MS:-40}" 'BEGIN {print (m > x)}')
  echo "$name after a write: $ms ms$([ "$over" = 0 ] || echo "  FAIL")"
  [ "$over" = 0 ] || fail=1
done < <(grep -E '^esql[^|]*\|' bench/requests.txt)
exit $fail
