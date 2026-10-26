#!/usr/bin/env bash
# Task: es-mini writes a 20000-doc index (full postings blocks, level-1 skip data) and
# Elasticsearch 9.5.5 (Docker, 1 CPU, 1 GB, docker exec, container always removed) must return
# the right _count for term, match, range and bool queries on every field, and for ranges over
# a 100000-doc index (multi-leaf BKD trees, 21-bit doc ids).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DATA="$(mktemp -d)"; PORT=${ES_MINI_PORT:-9392}
NAME=es-mini-cross-engine-big; ES=http://localhost:9200
cd "$ROOT"
cargo build -q -j 2 -p es-mini
BIN="$(cargo metadata --format-version 1 --no-deps | python3 -c 'import json,sys;print(json.load(sys.stdin)["target_directory"])')/debug/es-mini"
ES_MINI_DATA="$DATA" ES_MINI_PORT=$PORT "$BIN" >"$DATA.log" 2>&1 &
PID=$!
cleanup() { kill $PID 2>/dev/null || true; docker rm -f "$NAME" >/dev/null 2>&1 || true; rm -rf "$DATA" "$DATA.log"; }
trap cleanup EXIT
for _ in $(seq 50); do curl -s "localhost:$PORT/" >/dev/null && break; sleep 0.2; done
curl -sf -X PUT "localhost:$PORT/big" -H 'content-type: application/json' --data-binary \
  '{"settings":{"number_of_shards":1,"number_of_replicas":0},"mappings":{"properties":{"kw":{"type":"keyword"},"flag":{"type":"boolean"},"body":{"type":"text"},"n":{"type":"long"}}}}' >/dev/null
# kw: "even"/"odd"; flag: i%3==0; body: "common" everywhere, "rare" every 997th doc, "mid" every 5th,
# "freq" repeated i%9+1 times; n: i.
python3 - <<'PY' > "$DATA.bulk"
for i in range(20000):
    words = ["common"] + ["freq"] * (i % 9 + 1) + (["mid"] if i % 5 == 0 else []) + (["rare"] if i % 997 == 0 else [])
    print('{"index":{"_id":"%d"}}' % i)
    print('{"kw":"%s","flag":%s,"body":"%s","n":%d}' % ("even" if i % 2 == 0 else "odd", "true" if i % 3 == 0 else "false", " ".join(words), i))
PY
curl -sf -X POST "localhost:$PORT/big/_bulk?refresh=true" -H 'content-type: application/x-ndjson' --data-binary @"$DATA.bulk" >/dev/null
# range: n = i*7 - 350000 (distinct, negative to positive); dup = i % 100 (low cardinality)
curl -sf -X PUT "localhost:$PORT/range" -H 'content-type: application/json' --data-binary \
  '{"settings":{"number_of_shards":1,"number_of_replicas":0},"mappings":{"properties":{"n":{"type":"long"},"dup":{"type":"integer"}}}}' >/dev/null
# in 25000-doc requests: es-mini takes bodies up to 2 MB
for start in 0 25000 50000 75000; do
  python3 - $start <<'PY' > "$DATA.bulk"
import sys
for i in range(int(sys.argv[1]), int(sys.argv[1]) + 25000):
    print('{"index":{"_id":"%d"}}' % i)
    print('{"n":%d,"dup":%d}' % (i * 7 - 350000, i % 100))
PY
  curl -sf -X POST "localhost:$PORT/range/_bulk" -H 'content-type: application/x-ndjson' --data-binary @"$DATA.bulk" >/dev/null
done
curl -sf -X POST "localhost:$PORT/range/_refresh" >/dev/null
rm -f "$DATA.bulk"
kill $PID; wait $PID 2>/dev/null || true

rm -f "$DATA/node.lock"
docker rm -f "$NAME" >/dev/null 2>&1 || true
docker run -d --name "$NAME" --cpus 1 --memory 1g \
  -e discovery.type=single-node -e xpack.security.enabled=false -e xpack.ml.enabled=false \
  -e ES_JAVA_OPTS="-Xms512m -Xmx512m" -e cluster.name=es-mini-fixture -e node.name=node-0 \
  --user "$(id -u):0" -v "$DATA:/usr/share/elasticsearch/data" \
  "docker.elastic.co/elasticsearch/elasticsearch:9.5.5" >/dev/null
req() { printf '%s' "${3:-}" | docker exec -i "$NAME" curl -s -X "$1" "$ES$2" \
  -H 'content-type: application/json' ${3:+--data-binary @-}; }
tries=0
until req GET '/_cluster/health?wait_for_status=green&timeout=1s' 2>/dev/null | grep -q '"timed_out":false'; do
  tries=$((tries + 1))
  [ $tries -lt 60 ] || { docker logs "$NAME" 2>&1 | grep -E "big|Caused|ERROR" | cut -c1-700 | tail -8 >&2; echo "ES did not come up" >&2; exit 1; }
  sleep 2
done

fail=0
check() { # expected query-json [index]
  raw=$(req POST "/${3:-big}/_count" "{\"query\":$2}")
  got=$(printf '%s' "$raw" | python3 -c 'import json,sys;print(json.load(sys.stdin).get("count"))')
  if [ "$got" = "$1" ]; then echo "ok   $2"; else echo "FAIL $2: want $1 got $got: ${raw:0:400}"; fail=1; fi
}
check 20000 '{"match_all":{}}'
check 10000 '{"term":{"kw":"even"}}'
check 10000 '{"term":{"kw":"odd"}}'
check 6667 '{"term":{"flag":true}}'
check 13333 '{"term":{"flag":false}}'
check 20000 '{"match":{"body":"common"}}'
check 20000 '{"match":{"body":"freq"}}'
check 4000 '{"match":{"body":"mid"}}'
check 21 '{"match":{"body":"rare"}}'
check 4016 '{"match":{"body":"mid rare"}}'
check 20000 '{"range":{"n":{"gte":0}}}'
check 1000 '{"range":{"n":{"gte":5000,"lt":6000}}}'
check 5000 '{"bool":{"must":[{"term":{"kw":"even"}},{"match":{"body":"common"}}],"filter":{"range":{"n":{"lt":10000}}}}}'
check 667 '{"bool":{"must":[{"term":{"flag":true}},{"match":{"body":"mid"}}],"must_not":{"term":{"kw":"odd"}}}}'
check 4 '{"bool":{"must":[{"match":{"body":"rare"}},{"term":{"flag":true}},{"term":{"kw":"even"}}]}}'
check 100000 '{"match_all":{}}' range
check 50000 '{"range":{"n":{"gte":0}}}' range
check 285 '{"range":{"n":{"gte":-1000,"lt":1000}}}' range
check 143 '{"range":{"n":{"lt":-349000}}}' range
check 7142 '{"range":{"n":{"gt":300000}}}' range
check 10000 '{"range":{"dup":{"gte":10,"lte":19}}}' range
check 1000 '{"term":{"dup":42}}' range
check 143 '{"bool":{"filter":[{"range":{"n":{"gte":-1000,"lt":1000}}},{"range":{"dup":{"lt":50}}}]}}' range
exit $fail
