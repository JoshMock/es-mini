#!/usr/bin/env bash
# Builds fixtures/es-9.5.5-doc-values: boots the pinned Elasticsearch in Docker on its data
# directory and writes the `dv` index (1 shard): 3000 docs in one bulk, flushed as one segment
# (refresh_interval -1, so no periodic refresh splits it),
# with multi-valued and sparse keyword (`kw`, `one`, over 1024 distinct terms each) and long
# (`num`) fields. es-codec/tests/docvalues_golden.rs mirrors the doc values `doc` computes here;
# keep the two in sync. The index UUID goes to index_uuid. Bytes differ per run (UUIDs,
# timestamps); the logical content does not. Resources are capped: 1 CPU, 1 GB RAM.
set -euo pipefail
cd "$(dirname "$0")"
VERSION=9.5.5
NAME=es-mini-fixture-dv
ES=http://localhost:9200
OUT="$PWD/es-$VERSION-doc-values"
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

req PUT /dv '{
  "settings": {"number_of_shards": 1, "number_of_replicas": 0, "refresh_interval": "-1"},
  "mappings": {"properties": {
    "kw": {"type": "keyword"}, "one": {"type": "keyword"}, "num": {"type": "long"}
  }}}' >/dev/null
BULK=$(python3 - <<'PY'
import json
def doc(d):
    out = {}
    if d % 3:  # every third doc has none; up to three of 2000 terms
        out["kw"] = [f"term-{(d * 7 + i * 613) % 2000:04}" for i in range(1 + d % 3)]
    if d % 2:  # one term per odd doc, 1500 in all
        out["one"] = f"v-{d:05}"
    if d % 5:  # every fifth doc has none; up to two values, negative and large
        out["num"] = [d * 1_000_003 - 1_500_000_000] if d % 2 == 0 else [-d, d << 33]
    return out
for d in range(3000):
    print(json.dumps({"index": {"_id": str(d)}}))
    print(json.dumps(doc(d)))
PY
)
req POST /dv/_bulk "$BULK"$'\n' | python3 -c 'import json,sys; assert not json.load(sys.stdin)["errors"]'
req POST /dv/_flush >/dev/null
req GET /_cat/segments/dv?v >&2
req GET /dv/_settings | python3 -c 'import json,sys; print(json.load(sys.stdin)["dv"]["settings"]["index"]["uuid"])' > "$OUT/index_uuid"

docker stop -t 60 "$NAME" >/dev/null
echo "fixture written to $DATA"
