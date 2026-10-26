#!/usr/bin/env bash
# Crash recovery across engines. (1) Elasticsearch 9.5.5 writes documents and is killed (SIGKILL)
# before any flush; es-mini opens a copy and must serve them by replaying Elasticsearch's translog.
# (2) es-mini writes documents into its translog and is killed (SIGKILL); Elasticsearch opens a copy
# and must serve them by replaying es-mini's translog. Docker: 1 CPU, 1 GB, requests via
# docker exec, container always removed.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="$(mktemp -d)"; PORT=${ES_MINI_PORT:-9393}
NAME=es-mini-cross-engine-crash; ES=http://localhost:9200; PID=
cd "$ROOT"
cleanup() { [ -z "$PID" ] || kill -9 "$PID" 2>/dev/null || true; docker rm -f "$NAME" >/dev/null 2>&1 || true; rm -rf "$WORK"; }
trap cleanup EXIT
cargo build -q -j 2 -p es-mini
BIN="$(cargo metadata --format-version 1 --no-deps | python3 -c 'import json,sys;print(json.load(sys.stdin)["target_directory"])')/debug/es-mini"

INDEX='{"settings":{"number_of_shards":1,"number_of_replicas":0},"mappings":{"properties":{"title":{"type":"text"},"tag":{"type":"keyword"},"n":{"type":"long"}}}}'
# 30 docs (tag t<i%3>, n=i), then doc 1 updated to n=100, doc 2 deleted and doc 31 added (none of them flushed). Every doc's
# title holds "crashmark", so the translog files can be checked for unflushed operations.
BULK="$(for i in $(seq 1 30); do echo "{\"index\":{\"_id\":\"$i\"}}"; echo "{\"title\":\"crashmark doc $i\",\"tag\":\"t$((i % 3))\",\"n\":$i}"; done)
"
EDITS='{"update":{"_id":"1"}}
{"doc":{"n":100}}
{"delete":{"_id":"2"}}
{"index":{"_id":"31"}}
{"title":"crashmark late","tag":"t1","n":31}
'

start_es() { # data-dir
  docker rm -f "$NAME" >/dev/null 2>&1 || true
  docker run -d --name "$NAME" --cpus 1 --memory 1g \
    -e discovery.type=single-node -e xpack.security.enabled=false -e xpack.ml.enabled=false \
    -e ES_JAVA_OPTS="-Xms512m -Xmx512m" -e cluster.name=es-mini-fixture -e node.name=node-0 \
    --user "$(id -u):0" -v "$1:/usr/share/elasticsearch/data" \
    "docker.elastic.co/elasticsearch/elasticsearch:9.5.5" >/dev/null
  local tries=0
  until es GET '/_cluster/health?wait_for_status=green&timeout=1s' 2>/dev/null | grep -q '"timed_out":false'; do
    tries=$((tries + 1))
    [ $tries -lt 60 ] || { docker logs "$NAME" 2>&1 | grep -E "crash|Caused|ERROR" | cut -c1-700 | tail -8 >&2; echo "ES did not come up" >&2; exit 1; }
    sleep 2
  done
}
es() { printf '%s' "${3:-}" | docker exec -i "$NAME" curl -s -X "$1" "$ES$2" \
  -H "content-type: application/${4:-json}" ${3:+--data-binary @-}; }
start_mini() { # data-dir
  ES_MINI_DATA="$1" ES_MINI_PORT=$PORT "$BIN" >"$WORK/mini.log" 2>&1 &
  PID=$!
  for _ in $(seq 50); do curl -s "localhost:$PORT/" >/dev/null && return; sleep 0.2; done
  cat "$WORK/mini.log" >&2; echo "es-mini did not come up" >&2; exit 1
}
mini() { curl -s -X "$1" "localhost:$PORT$2" -H "content-type: application/${4:-json}" ${3:+--data-binary "$3"}; }
# Fails unless the shard's translog holds unflushed operations, so recovery really replays them.
pending() { # data-dir
  grep -rlq crashmark --include='*.tlog' "$1" || { echo "FAIL no translog operations in $1" >&2; exit 1; }
}

fail=0
check() { # engine label expected-python-expression method path [body]
  local raw
  raw=$("$1" "$4" "$5" "${6:-}")
  if printf '%s' "$raw" | python3 -c 'import json,sys; d=json.load(sys.stdin); sys.exit(0 if eval(sys.argv[1]) else 1)' "$3" 2>/dev/null; then
    echo "ok   $1 $2"
  else
    echo "FAIL $1 $2: ${raw:0:400}"; fail=1
  fi
}
checks() { # engine
  check "$1" count 'd["count"] == 30' POST /crash/_count '{"query":{"match_all":{}}}'
  check "$1" get-updated 'd["_source"]["n"] == 100 and d["_source"]["title"] == "crashmark doc 1"' GET /crash/_doc/1
  check "$1" get-deleted 'd["found"] is False' GET /crash/_doc/2
  check "$1" get-late 'd["_source"]["title"] == "crashmark late"' GET /crash/_doc/31
  check "$1" match 'd["hits"]["total"]["value"] == 30' POST /crash/_search '{"query":{"match":{"title":"crashmark"}}}'
  check "$1" term 'd["count"] == 11' POST /crash/_count '{"query":{"term":{"tag":"t1"}}}'
  check "$1" range 'd["count"] == 4' POST /crash/_count '{"query":{"range":{"n":{"gte":29}}}}'
}

echo "== (1) Elasticsearch writes, is killed; es-mini replays"
mkdir "$WORK/es"
start_es "$WORK/es"
es PUT /crash "$INDEX" >/dev/null
es POST '/crash/_bulk?refresh=true' "$BULK" x-ndjson >/dev/null
es POST /crash/_bulk "$EDITS" x-ndjson >/dev/null
docker kill "$NAME" >/dev/null
docker rm -f "$NAME" >/dev/null
pending "$WORK/es"
cp -r "$WORK/es" "$WORK/es-copy"
start_mini "$WORK/es-copy"
checks mini
kill -9 "$PID"; wait "$PID" 2>/dev/null || true; PID=

echo "== (2) es-mini writes, is killed; Elasticsearch replays"
mkdir "$WORK/mini"
start_mini "$WORK/mini"
mini PUT /crash "$INDEX" >/dev/null
mini POST '/crash/_bulk?refresh=true' "$BULK" x-ndjson >/dev/null
mini POST /crash/_bulk "$EDITS" x-ndjson >/dev/null
kill -9 "$PID"; wait "$PID" 2>/dev/null || true; PID=
pending "$WORK/mini"
cp -r "$WORK/mini" "$WORK/mini-copy"
rm -f "$WORK/mini-copy/node.lock"
start_es "$WORK/mini-copy"
checks es
exit $fail
