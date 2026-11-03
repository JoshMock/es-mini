#!/usr/bin/env bash
# Records translog.tlog: the translog Elasticsearch 9.5.5 writes for the single-document writes
# below on a 1-shard index, killed before any flush. Docker: 1 CPU, 1 GB, requests via
# docker exec, container always removed.
set -euo pipefail
OUT="$(cd "$(dirname "$0")" && pwd)"
DATA="$(mktemp -d)"; NAME=es-mini-translog-record
trap 'docker rm -f "$NAME" >/dev/null 2>&1 || true; rm -rf "$DATA"' EXIT
docker run -d --name "$NAME" --cpus 1 --memory 1g \
  -e discovery.type=single-node -e xpack.security.enabled=false -e xpack.ml.enabled=false \
  -e ES_JAVA_OPTS="-Xms512m -Xmx512m" --user "$(id -u):0" \
  -v "$DATA:/usr/share/elasticsearch/data" \
  "docker.elastic.co/elasticsearch/elasticsearch:9.5.5" >/dev/null
es() { docker exec "$NAME" curl -sf -X "$1" "http://localhost:9200$2" \
  -H 'content-type: application/json' ${3:+--data-binary "$3"}; }
until es GET '/_cluster/health?wait_for_status=green&timeout=1s' >/dev/null 2>&1; do sleep 2; done
es PUT /t '{"settings":{"number_of_shards":1,"number_of_replicas":0}}' >/dev/null
es PUT /t/_mapping '{"properties":{"v":{"type":"long"}}}' >/dev/null
es PUT /t/_doc/1 '{"v":1}' >/dev/null
es PUT '/t/_doc/2?routing=r' '{"v":2}' >/dev/null
es PUT /t/_doc/1 '{"v":3}' >/dev/null
es DELETE /t/_doc/1 >/dev/null
es POST /t/_doc '{"v":4}' >/dev/null
docker kill "$NAME" >/dev/null
cp "$(find "$DATA" -name 'translog-*.tlog' -size +100c)" "$OUT/translog.tlog"
