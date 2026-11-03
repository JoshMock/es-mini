#!/usr/bin/env bash
# Builds fixtures/es-9.5.5-non-compound: boots the pinned Elasticsearch in Docker on its data
# directory and writes the `nocfs` index (index.compound_format: false, 1 shard). Two refreshes
# flush two compound segments; a force merge merges them into one segment that, with
# noCFSRatio 0, is left non-compound. A third refresh then deletes a doc of the merged segment
# and adds one, as a compound segment, and a flush commits it all. The index UUID goes to
# index_uuid. Bytes differ per run (UUIDs, timestamps); the logical content does not.
# Resources are capped: 1 CPU, 1 GB RAM.
set -euo pipefail
cd "$(dirname "$0")"
VERSION=9.5.5
NAME=es-mini-fixture-nocfs
ES=http://localhost:9200
OUT="$PWD/es-$VERSION-non-compound"
DATA="$OUT/data"

docker rm -f "$NAME" >/dev/null 2>&1 || true
rm -rf "$OUT"
mkdir -p "$DATA"
trap 'docker rm -f "$NAME" >/dev/null 2>&1 || true' EXIT

docker run -d --name "$NAME" --cpus 1 --memory 1g \
  -e discovery.type=single-node -e xpack.security.enabled=false -e xpack.ml.enabled=false \
  -e ES_JAVA_OPTS="-Xms512m -Xmx512m" -e cluster.name=es-mini-fixture -e node.name=node-0 \
  --user "$(id -u):0" -v "$DATA:/usr/share/elasticsearch/data" \
  "docker.elastic.co/elasticsearch/elasticsearch:$VERSION" >/dev/null

echo "waiting for Elasticsearch..."
req() {
  local out code
  out=$(printf '%s' "${3:-}" | docker exec -i "$NAME" curl -s -w '\n%{http_code}' -X "$1" "$ES$2" \
    -H 'content-type: application/json' ${3:+--data-binary @-}) || return 1
  code=${out##*$'\n'}
  if [ "$code" -ge 400 ]; then echo "$1 $2 -> $code ${out%$'\n'*}" >&2; return 1; fi
  printf '%s\n' "${out%$'\n'*}"
}
until req GET '/_cluster/health?wait_for_status=green&timeout=1s' >/dev/null 2>&1; do sleep 2; done

req PUT /nocfs '{
  "settings": {"number_of_shards": 1, "number_of_replicas": 0, "index.compound_format": false},
  "mappings": {"properties": {
    "title": {"type": "text"}, "tag": {"type": "keyword"}, "views": {"type": "long"}
  }}}' >/dev/null
req POST '/nocfs/_bulk?refresh=true' "$(cat <<'NDJSON'
{"index":{"_id":"1"}}
{"title":"The quick brown fox","tag":"animal","views":10}
{"index":{"_id":"2"}}
{"title":"Quick quick rabbits","tag":"animal","views":25}
{"index":{"_id":"3"}}
{"title":"Lazy dog sleeps","tag":"pet","views":3}
NDJSON
)"$'\n' >/dev/null
req POST '/nocfs/_bulk?refresh=true' "$(cat <<'NDJSON'
{"index":{"_id":"4"}}
{"title":"A brown dog and a fox","tag":"pet","views":7}
{"index":{"_id":"5"}}
{"title":"Fresh fox news","tag":"news","views":42}
NDJSON
)"$'\n' >/dev/null
req POST '/nocfs/_forcemerge?max_num_segments=1' >/dev/null
req POST '/nocfs/_bulk?refresh=true' "$(cat <<'NDJSON'
{"delete":{"_id":"3"}}
{"index":{"_id":"6"}}
{"title":"Slow green turtle","tag":"animal","views":1}
NDJSON
)"$'\n' >/dev/null
req POST /nocfs/_flush >/dev/null
req GET /_cat/segments/nocfs?v >&2
req GET /nocfs/_settings | python3 -c 'import json,sys; print(json.load(sys.stdin)["nocfs"]["settings"]["index"]["uuid"])' > "$OUT/index_uuid"

docker stop -t 60 "$NAME" >/dev/null
echo "fixture written to $DATA"
