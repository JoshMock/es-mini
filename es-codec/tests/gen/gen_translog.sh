#!/usr/bin/env bash
# Writes tests/data/es-translog-records: the translog of an index Elasticsearch 9.5.5 left with
# pending operations, killed (SIGKILL) before any flush. Batch indexing is switched on (feature
# flag plus indices.batch_indexing), so index-only bulks become IndexBatch records; a bulk that
# mixes in a delete is written as single operations. Capped at 1 CPU / 1 GB, requests via
# `docker exec`, container always removed. Usage: gen_translog.sh
set -euo pipefail
cd "$(dirname "$0")"
out=../data/es-translog-records; name=es-mini-gen-translog; work=$(mktemp -d)
trap 'docker rm -f "$name" >/dev/null 2>&1 || true; rm -rf "$work"' EXIT
docker rm -f "$name" >/dev/null 2>&1 || true
mkdir "$work/data"
docker run -d --name "$name" --cpus 1 --memory 1g -e discovery.type=single-node \
  -e xpack.security.enabled=false -e xpack.ml.enabled=false -e indices.batch_indexing=true \
  -e ES_JAVA_OPTS="-Xms512m -Xmx512m -Des.batch_indexing_feature_flag_enabled=true" \
  --user "$(id -u):0" -v "$work/data:/usr/share/elasticsearch/data" \
  docker.elastic.co/elasticsearch/elasticsearch:9.5.5 >/dev/null
req() { printf '%s' "${3:-}" | docker exec -i "$name" curl -s -X "$1" "localhost:9200$2" \
  -H "content-type: application/${4:-json}" ${3:+--data-binary @-}; echo; }
for _ in $(seq 90); do
  req GET '/_cluster/health?wait_for_status=green&timeout=1s' 2>/dev/null | grep -q '"timed_out":false' && break
  sleep 2
done
# Batches need every field mapped up front (ShardBatchMapper falls back on dynamic mapping).
req PUT /t '{"settings":{"number_of_shards":1,"number_of_replicas":0},"mappings":{"properties":{"w":{"type":"text","analyzer":"keyword"},"a":{"type":"long"},"n":{"type":"long"},"s":{"type":"keyword"},"o":{"properties":{"x":{"type":"long"}}},"f":{"type":"float"},"t":{"type":"boolean"},"z":{"type":"keyword"},"k":{"type":"keyword"},"g":{"type":"double"},"l":{"type":"text"},"u":{"type":"object","dynamic":false}}}}'
immense=$(printf 'x%.0s' $(seq 33000))
# Mixed bulk: single records. Index with routing (seq 0), delete (1), and a document Lucene
# rejects after it took a sequence number, which Elasticsearch logs as a NoOp (2): an immense
# term in a text field (the keyword mapper would reject it before assigning one).
req POST /t/_bulk "$(printf '%s\n' \
  '{"index":{"_id":"r1","routing":"user1"}}' '{"a":1}' \
  '{"delete":{"_id":"gone"}}' \
  '{"index":{"_id":"big"}}' "{\"w\":\"$immense\"}")
" x-ndjson | cut -c1-600
# Index-only bulks: IndexBatch records of at most 32 docs (40 docs, one routed; then a doc and
# one with an immense term, a NoOp inside the batch).
bulk=$(for i in $(seq 1 40); do
  if [ "$i" = 7 ]; then echo "{\"index\":{\"_id\":\"b$i\",\"routing\":\"user2\"}}"; else echo "{\"index\":{\"_id\":\"b$i\"}}"; fi
  echo "{\"n\":$i,\"s\":\"v$i\",\"o\":{\"x\":-$i},\"f\":1.5,\"t\":true,\"z\":null}"
done)
req POST /t/_bulk "$bulk
" x-ndjson | cut -c1-300
req POST /t/_bulk "$(printf '%s\n' '{"index":{"_id":"c1"}}' '{"a":2}' '{"index":{"_id":"c2"}}' "{\"w\":\"$immense\"}")
" x-ndjson | cut -c1-600
# Arrays in mapped fields make Elasticsearch fall back to a single record (45); a double and
# escapes batch (46), with a row whose strings outgrow a small row's 16-bit offsets (47).
req POST /t/_bulk "$(printf '%s\n' '{"index":{"_id":"d1"}}' '{"k":["x","y"]}')
" x-ndjson | cut -c1-300
req POST /t/_bulk "$(printf '%s\n' '{"index":{"_id":"d2"}}' '{"g":2.5e300,"s":"q\"\u00e9\ud83d\ude00\n"}' \
  '{"index":{"_id":"d3"}}' "{\"l\":\"$(printf 'ab %.0s' $(seq 22000))\"}")
" x-ndjson | cut -c1-300
# Unmapped values under a dynamic:false object batch whatever their shape (48).
req POST /t/_bulk "$(printf '%s\n' '{"index":{"_id":"e1"}}' \
  '{"u":{"arr":[1,"a",[2.5,null],false],"fix":[1,2],"objs":[{"p":1,"q":[true]}],"e":{}}}')
" x-ndjson | cut -c1-300
req GET '/t/_stats/translog?filter_path=**.translog' | cut -c1-400
uuid=$(req GET '/_cat/indices/t?h=uuid' | tr -d '[:space:]')
docker kill "$name" >/dev/null
rm -rf "$out"; mkdir -p "$out"
cp "$work/data/indices/$uuid/0/translog/"* "$out/"
ls -l "$out"
