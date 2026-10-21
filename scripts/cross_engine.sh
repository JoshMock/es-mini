#!/usr/bin/env bash
# Cross-engine write check: copies a data directory, boots Elasticsearch 9.5.5 on the copy in Docker
# (1 CPU, 1 GB, requests via docker exec, container always removed) and compares a fixed query set
# against fixtures/expected/. Usage: scripts/cross_engine.sh [DATA_DIR]
# DATA_DIR defaults to the ES-written fixture (a baseline that proves the harness itself works).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SRC="${1:-$ROOT/fixtures/es-9.5.5/data}"
NAME=es-mini-cross-engine
ES=http://localhost:9200
WORK="$(mktemp -d)"
cleanup() { [ -z "${VERBOSE:-}" ] || docker logs "$NAME" 2>&1 | grep -E "fixture|Caused|Exception" | grep -v rest.suppressed | cut -c1-600 | head -15 | sed "s/.*\"message\":\"\([^\"]*\)\".*/\1/"
  docker rm -f "$NAME" >/dev/null 2>&1 || true; rm -rf "$WORK"; }
trap cleanup EXIT
docker rm -f "$NAME" >/dev/null 2>&1 || true
cp -r "$SRC" "$WORK/data"
rm -f "$WORK/data/node.lock"

docker run -d --name "$NAME" --cpus 1 --memory 1g \
  -e discovery.type=single-node -e xpack.security.enabled=false -e xpack.ml.enabled=false \
  -e ES_JAVA_OPTS="-Xms512m -Xmx512m" -e cluster.name=es-mini-fixture -e node.name=node-0 \
  --user "$(id -u):0" -v "$WORK/data:/usr/share/elasticsearch/data" \
  "docker.elastic.co/elasticsearch/elasticsearch:9.5.5" >/dev/null

req() { printf '%s' "${3:-}" | docker exec -i "$NAME" curl -s -X "$1" "$ES$2" \
  -H 'content-type: application/json' ${3:+--data-binary @-}; }
tries=0
until req GET '/_cluster/health?wait_for_status=green&timeout=1s' 2>/dev/null | grep -q '"timed_out":false'; do
  tries=$((tries + 1))
  [ $tries -lt 60 ] || { docker logs "$NAME" 2>&1 | grep -E "fixture|Caused|ERROR|WARN" | grep -v rest.suppressed | cut -c1-700 | tail -8 >&2; req GET /_cat/shards?v >&2; req GET /_cluster/allocation/explain | cut -c1-4000 >&2; echo "ES did not come up" >&2; exit 1; }
  sleep 2
done

VOLATILE='["took","start_time_in_millis","completion_time_in_millis","expiration_time_in_millis","cpu_nanos","read_nanos","bytes_read","rows_emitted"]'
strip() { python3 -c 'import json,sys; d=json.load(sys.stdin); [d.pop(k,None) for k in json.loads(sys.argv[1])]; print(json.dumps(d, indent=2, sort_keys=True, ensure_ascii=False))' "$VOLATILE"; }
fail=0
check() { # name method path [body]
  if req "$2" "$3" "${4:-}" | strip | diff -q - "$ROOT/fixtures/expected/$1.json" >/dev/null 2>&1; then
    echo "ok   $1"
  else
    echo "FAIL $1"; fail=1
    [ -z "${VERBOSE:-}" ] || req "$2" "$3" "${4:-}" | strip | diff - "$ROOT/fixtures/expected/$1.json" | head -20 || true
  fi
}
check get_1 GET /fixture/_doc/1
check get_5_deleted GET /fixture/_doc/5
check match_all POST /fixture/_search '{"query":{"match_all":{}},"sort":[{"views":"asc"}]}'
check match_fox POST /fixture/_search '{"query":{"match":{"title":"fox"}}}'
check term_tag POST /fixture/_search '{"query":{"term":{"tag":"pet"}},"sort":["views"]}'
check range_views POST /fixture/_search '{"query":{"range":{"views":{"gte":7,"lt":30}}},"sort":["views"]}'
check bool POST /fixture/_search '{"query":{"bool":{"must":{"match":{"title":"dog"}},"must_not":{"term":{"tag":"pet"}}}}}'
check count_published POST /fixture/_count '{"query":{"term":{"published":true}}}'
check mapping GET /fixture/_mapping
[ -z "${VERBOSE:-}" ] || { req GET /_cat/shards?v; req GET /_cluster/allocation/explain | cut -c1-1500; }
exit $fail
