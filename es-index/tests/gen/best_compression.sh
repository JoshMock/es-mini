#!/usr/bin/env bash
# Writes es-index/tests/data/best-compression/: an Elasticsearch 9.5.5 data directory holding
# index `bc` (index.codec=best_compression, so Zstd stored fields) with 200 known documents in
# one flushed segment. Doc i: {"n": i, "tag": "t<i%7>", "body": words[(i*k) % 40] for k in 0..30}.
# Docker capped at 1 CPU / 1 GB; requests via docker exec; the container is always removed.
set -euo pipefail
cd "$(dirname "$0")"
NAME=es-mini-best-compression
out=$PWD/../data/best-compression
docker rm -f "$NAME" >/dev/null 2>&1 || true
trap 'docker rm -f "$NAME" >/dev/null 2>&1 || true' EXIT
rm -rf "$out"; mkdir -p "$out"
docker run -d --name "$NAME" --cpus 1 --memory 1g \
  -e discovery.type=single-node -e xpack.security.enabled=false -e xpack.ml.enabled=false \
  -e ES_JAVA_OPTS="-Xms512m -Xmx512m" -e cluster.name=es-mini-fixture -e node.name=node-0 \
  --user "$(id -u):0" -v "$out:/usr/share/elasticsearch/data" \
  docker.elastic.co/elasticsearch/elasticsearch:9.5.5 >/dev/null
req() { printf '%s' "${3:-}" | docker exec -i "$NAME" curl -sf -X "$1" "localhost:9200$2" \
  -H 'content-type: application/json' ${3:+--data-binary @-}; }
until req GET '/_cluster/health?wait_for_status=green&timeout=1s' >/dev/null 2>&1; do sleep 2; done
req PUT /bc '{"settings": {"number_of_shards": 1, "number_of_replicas": 0,
  "index.codec": "best_compression"},
  "mappings": {"properties": {"n": {"type": "long"}, "tag": {"type": "keyword"},
  "body": {"type": "text"}}}}' >/dev/null
bulk=$(python3 - <<'PY'
import json
words = ("alpha bravo charlie delta echo foxtrot golf hotel india juliet kilo lima mike "
         "november oscar papa quebec romeo sierra tango uniform victor whiskey xray yankee "
         "zulu red orange yellow green blue indigo violet black white grey brown pink cyan "
         "magenta").split()
for i in range(200):
    body = " ".join(words[(i * k) % 40] for k in range(30))
    print(json.dumps({"index": {"_id": str(i)}}))
    print(json.dumps({"n": i, "tag": f"t{i % 7}", "body": body}))
PY
)
req POST '/bc/_bulk?refresh=true' "$bulk"$'\n' >/dev/null
req POST /bc/_flush >/dev/null
docker stop -t 60 "$NAME" >/dev/null
rm -f "$out/node.lock"
du -sh "$out"
