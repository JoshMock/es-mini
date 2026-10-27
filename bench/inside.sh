#!/usr/bin/env bash
# Runs in the client container: loads the dataset into both servers, checks they answer alike,
# then benchmarks each request with hyperfine. Writes to /results.
set -euo pipefail
SERVERS=(es es-mini)
H='content-type: application/json'
for s in "${SERVERS[@]}"; do
  until curl -sf "http://$s:9200/" >/dev/null; do sleep 1; done
  curl -sf -X PUT "http://$s:9200/bench" -H "$H" --data-binary @/bench/mapping.json >/dev/null
  # Chunked (2 lines per doc): es-mini rejects bulk bodies over 2 MB, Elasticsearch allows 100 MB.
  rm -rf /tmp/chunks; mkdir /tmp/chunks; split -l 4000 /tmp/data.ndjson /tmp/chunks/c
  for c in /tmp/chunks/c*; do
    curl -sf -X POST "http://$s:9200/bench/_bulk" -H 'content-type: application/x-ndjson' \
      --data-binary "@$c" -o /dev/null
  done
  curl -sf -X POST "http://$s:9200/bench/_refresh" >/dev/null
  echo "$s: $(curl -s "http://$s:9200/bench/_count")"
done
mkdir -p /tmp/bodies
grep -v '^#' /bench/requests.txt | grep . | while IFS='|' read -r name method path body; do
  args=(-s -o /dev/null -X "$method" -H content-type:application/json)
  [ -z "$body" ] || { printf '%s' "$body" > "/tmp/bodies/$name.json"; args+=(--data-binary "@/tmp/bodies/$name.json"); }
  echo "== $name"
  for s in "${SERVERS[@]}"; do  # sanity check: status and, for searches, the hit count
    out=$(curl -s -w ' HTTP %{http_code}' -X "$method" -H "$H" ${body:+--data-binary "@/tmp/bodies/$name.json"} "http://$s:9200$path")
    total=$(printf '%s' "$out" | grep -oE '"(value|count)":[0-9]+' | head -1 || true)
    printf '  %-8s HTTP %s %s\n' "$s" "${out##* HTTP }" "${total:-}"
  done
  hyperfine -N --warmup "${WARMUP:-5}" --runs "${RUNS:-50}" --style basic \
    --export-markdown "/results/$name.md" --export-json "/results/$name.json" \
    -n es "curl ${args[*]} http://es:9200$path" \
    -n es-mini "curl ${args[*]} http://es-mini:9200$path"
done
for f in /results/*.md; do
  [ "$f" = /results/summary.md ] || printf '### %s\n\n%s\n\n' "$(basename "$f" .md)" "$(cat "$f")"
done > /results/summary.md
