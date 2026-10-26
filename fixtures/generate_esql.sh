#!/usr/bin/env bash
# Records Elasticsearch 9.5.5's responses to the ES|QL queries in esql-queries.txt (one per line;
# blank lines and `#` comments skipped) in expected-esql/NNN.json, as {"query", "response"}.
# Boots a fresh Elasticsearch in Docker with no data volume (fixtures/es-9.5.5 stays untouched),
# ingests the `emp` and `emp_extra` indices below, and removes the container afterwards.
# Resources are capped: 1 CPU, 1 GB RAM. es-mini/tests/esql_parity.rs replays the same ingest
# (the `req PUT` and NDJSON blocks are parsed from this file) and compares every response.
set -euo pipefail
cd "$(dirname "$0")"
NAME=es-mini-esql

docker rm -f "$NAME" >/dev/null 2>&1 || true
trap 'docker rm -f "$NAME" >/dev/null 2>&1 || true' EXIT
rm -rf expected-esql
mkdir -p expected-esql

docker run -d --name "$NAME" --cpus 1 --memory 1g \
  -e discovery.type=single-node -e xpack.security.enabled=false -e xpack.ml.enabled=false \
  -e ES_JAVA_OPTS="-Xms512m -Xmx512m" -e cluster.name=es-mini-fixture -e node.name=node-0 \
  docker.elastic.co/elasticsearch/elasticsearch:9.5.5 >/dev/null

echo "waiting for Elasticsearch..."
# Prints the body, then the HTTP status on its own line.
call() {
  printf '%s' "${3:-}" | docker exec -i "$NAME" curl -s -w '\n%{http_code}' -X "$1" "localhost:9200$2" \
    -H 'content-type: application/json' ${3:+--data-binary @-}
}
req() {
  local out code
  out=$(call "$@") || return 1
  code=${out##*$'\n'}
  if [ "$code" -ge 400 ]; then echo "$1 $2 -> $code ${out%$'\n'*}" >&2; return 1; fi
}
until req GET '/_cluster/health?wait_for_status=green&timeout=1s' 2>/dev/null; do sleep 2; done

req PUT /emp '{"settings":{"number_of_shards":1,"number_of_replicas":0},"mappings":{"properties":{"name":{"type":"keyword"},"bio":{"type":"text"},"age":{"type":"integer"},"salary":{"type":"long"},"height":{"type":"double"},"active":{"type":"boolean"},"hired":{"type":"date"},"tags":{"type":"keyword"},"scores":{"type":"integer"}}}}'
req PUT /emp_extra '{"settings":{"number_of_shards":1,"number_of_replicas":0},"mappings":{"properties":{"name":{"type":"keyword"},"age":{"type":"integer"},"dept":{"type":"keyword"}}}}'
req POST '/_bulk?refresh=true' "$(cat <<'NDJSON'
{"index":{"_index":"emp","_id":"1"}}
{"name":"Alice","bio":"Loves Rust and coffee","age":34,"salary":85000,"height":1.68,"active":true,"hired":"2019-03-15T09:30:00Z","tags":["dev","lead"],"scores":[90,75,82]}
{"index":{"_index":"emp","_id":"2"}}
{"name":"Bob","bio":"  Plays chess  ","age":28,"salary":62000,"height":1.82,"active":false,"hired":"2021-07-01T00:00:00Z","tags":"dev","scores":60}
{"index":{"_index":"emp","_id":"3"}}
{"name":"Carol","bio":"Writes poetry","age":45,"salary":120000,"height":1.75,"active":true,"hired":"2015-11-20T14:00:00Z","tags":["mgr","ops"],"scores":[88,95]}
{"index":{"_index":"emp","_id":"4"}}
{"name":"Dave","age":28,"salary":58000,"height":1.9,"active":true,"hired":"2022-01-10T00:00:00Z","tags":"ops"}
{"index":{"_index":"emp","_id":"5"}}
{"name":"Eve","bio":"Security researcher","salary":99000,"active":false,"hired":"2018-05-05T00:00:00Z","tags":["sec","dev"],"scores":[70]}
{"index":{"_index":"emp","_id":"6"}}
{"name":"Frank","bio":"New hire","age":23,"height":1.7,"active":true,"hired":"2024-02-29T08:15:00Z","scores":[55,65]}
{"index":{"_index":"emp","_id":"7"}}
{"name":"Grace","bio":"Database Ninja","age":51,"salary":150000,"height":1.6,"active":true,"hired":"2010-09-01T00:00:00Z","tags":["mgr"],"scores":[99]}
{"index":{"_index":"emp","_id":"8"}}
{"name":"Héloïse","bio":"Ünïcödé café 東京","age":39,"salary":77000,"height":1.65,"active":false,"hired":"2020-12-31T23:59:59Z","tags":["dev"],"scores":[80,80]}
{"index":{"_index":"emp_extra","_id":"9"}}
{"name":"Ivan","age":30,"dept":"eng"}
{"index":{"_index":"emp_extra","_id":"10"}}
{"name":"Judy","age":41,"dept":"ops"}
NDJSON
)"$'\n'

n=0
while IFS= read -r q; do
  case "$q" in '' | '#'*) continue ;; esac
  n=$((n + 1))
  body=$(python3 -c 'import json,sys; print(json.dumps({"query": sys.argv[1]}))' "$q")
  call POST /_query "$body" | sed '$d' | python3 -c '
import json, sys
d = json.load(sys.stdin)
d.pop("took", None)
print(json.dumps({"query": sys.argv[1], "response": d}, indent=2, sort_keys=True, ensure_ascii=False))' "$q" \
    > "expected-esql/$(printf %03d "$n").json"
done < esql-queries.txt

echo "recorded $n responses in expected-esql/"
