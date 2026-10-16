#!/usr/bin/env bash
# Builds the on-disk fixture: boots the pinned Elasticsearch in Docker on fixtures/es-9.5.5/data,
# creates an index, ingests known documents, records expected query responses in
# fixtures/expected/, then shuts Elasticsearch down cleanly.
#
# File contents include UUIDs and timestamps, so bytes differ per run; the logical data and the
# expected responses are deterministic. Resources are capped: 1 CPU, 1 GB RAM.
set -euo pipefail
cd "$(dirname "$0")"
VERSION=9.5.5
NAME=es-mini-fixture
ES=http://localhost:9200
DATA="$PWD/es-$VERSION/data"

docker rm -f "$NAME" >/dev/null 2>&1 || true
rm -rf "es-$VERSION" expected
mkdir -p "$DATA" expected
trap 'docker rm -f "$NAME" >/dev/null 2>&1 || true' EXIT

docker run -d --name "$NAME" --cpus 1 --memory 1g \
  -e discovery.type=single-node -e xpack.security.enabled=false -e xpack.ml.enabled=false \
  -e ES_JAVA_OPTS="-Xms512m -Xmx512m" -e cluster.name=es-mini-fixture -e node.name=node-0 \
  --user "$(id -u):0" -v "$DATA:/usr/share/elasticsearch/data" \
  "docker.elastic.co/elasticsearch/elasticsearch:$VERSION" >/dev/null

echo "waiting for Elasticsearch..."
# Requests run inside the container, so no host port is published.
req() {
  local out code
  out=$(printf '%s' "${3:-}" | docker exec -i "$NAME" curl -s -w '\n%{http_code}' -X "$1" "$ES$2" \
    -H 'content-type: application/json' ${3:+--data-binary @-}) || return 1
  code=${out##*$'\n'}
  if [ "$code" -ge 400 ] && [ "${ALLOW_404:-}" != "$code" ]; then echo "$1 $2 -> $code ${out%$'\n'*}" >&2; return 1; fi
  printf '%s\n' "${out%$'\n'*}"
}
until req GET '/_cluster/health?wait_for_status=green&timeout=1s' >/dev/null 2>&1; do sleep 2; done

req PUT /fixture '{
  "settings": {"number_of_shards": 1, "number_of_replicas": 0},
  "mappings": {"properties": {
    "title": {"type": "text"}, "tag": {"type": "keyword"}, "views": {"type": "long"},
    "rating": {"type": "double"}, "published": {"type": "boolean"}, "date": {"type": "date"}
  }}}' >/dev/null

req POST '/fixture/_bulk?refresh=true' "$(cat <<'NDJSON'
{"index":{"_id":"1"}}
{"title":"The quick brown fox","tag":"animal","views":10,"rating":4.5,"published":true,"date":"2024-01-01"}
{"index":{"_id":"2"}}
{"title":"Quick quick rabbits","tag":"animal","views":25,"rating":3.0,"published":false,"date":"2024-02-15"}
{"index":{"_id":"3"}}
{"title":"Lazy dog sleeps","tag":"pet","views":3,"rating":2.5,"published":true,"date":"2023-12-31"}
{"index":{"_id":"4"}}
{"title":"A brown dog and a fox","tag":"pet","views":7,"rating":5.0,"published":true,"date":"2024-03-10"}
{"index":{"_id":"5"}}
{"title":"Ünïcödé café naïve 東京","tag":"misc","views":0,"rating":1.0,"published":false,"date":"2024-04-01"}
NDJSON
)"$'\n' >/dev/null
# A second segment with an update and a delete, so live-docs and multiple segments are exercised.
req POST '/fixture/_bulk?refresh=true' "$(cat <<'NDJSON'
{"index":{"_id":"6"}}
{"title":"Fresh fox news","tag":"news","views":42,"rating":3.5,"published":true,"date":"2024-05-05"}
{"update":{"_id":"2"}}
{"doc":{"views":26}}
{"delete":{"_id":"5"}}
NDJSON
)"$'\n' >/dev/null
req POST /fixture/_flush >/dev/null

# Expected responses, minus timing fields that vary per run.
VOLATILE='["took","start_time_in_millis","completion_time_in_millis","expiration_time_in_millis","cpu_nanos","read_nanos","bytes_read","rows_emitted"]'
strip() { python3 -c 'import json,sys; d=json.load(sys.stdin); [d.pop(k,None) for k in json.loads(sys.argv[1])]; print(json.dumps(d, indent=2, sort_keys=True, ensure_ascii=False))' "$VOLATILE"; }
record() { req "$2" "$3" "${4:-}" | strip > "expected/$1.json"; }
record get_1 GET /fixture/_doc/1
ALLOW_404=404 record get_5_deleted GET /fixture/_doc/5
record match_all POST /fixture/_search '{"query":{"match_all":{}},"sort":[{"views":"asc"}]}'
record match_fox POST /fixture/_search '{"query":{"match":{"title":"fox"}}}'
record term_tag POST /fixture/_search '{"query":{"term":{"tag":"pet"}},"sort":["views"]}'
record range_views POST /fixture/_search '{"query":{"range":{"views":{"gte":7,"lt":30}}},"sort":["views"]}'
record bool POST /fixture/_search '{"query":{"bool":{"must":{"match":{"title":"dog"}},"must_not":{"term":{"tag":"pet"}}}}}'
record count_published POST /fixture/_count '{"query":{"term":{"published":true}}}'
record esql POST /_query '{"query":"FROM fixture | WHERE views > 5 | SORT views DESC | KEEP tag, views | LIMIT 3"}'
record mapping GET /fixture/_mapping

docker stop -t 60 "$NAME" >/dev/null
echo "fixture written to $DATA"
