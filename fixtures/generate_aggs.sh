#!/usr/bin/env bash
# Records Elasticsearch 9.5.5's responses to aggregation requests in fixtures/expected-aggs/.
# Boots Elasticsearch in Docker on a COPY of fixtures/es-9.5.5/data (generate.sh's `fixture`
# index), so the recorded data stays untouched and es-mini can be compared in memory (replaying
# generate.sh's ingest) and from disk. Resources are capped: 1 CPU, 1 GB RAM.
# Each `record` line is one case: NAME METHOD PATH 'BODY' (single-line body, no single quotes).
set -euo pipefail
cd "$(dirname "$0")"
NAME=es-mini-aggs
WORK=$(mktemp -d)

docker rm -f "$NAME" >/dev/null 2>&1 || true
trap 'docker rm -f "$NAME" >/dev/null 2>&1 || true; rm -rf "$WORK"' EXIT
cp -r es-9.5.5/data "$WORK/data"
rm -f "$WORK/data/node.lock"
rm -rf expected-aggs
mkdir -p expected-aggs

docker run -d --name "$NAME" --cpus 1 --memory 1g \
  -e discovery.type=single-node -e xpack.security.enabled=false -e xpack.ml.enabled=false \
  -e ES_JAVA_OPTS="-Xms512m -Xmx512m" -e cluster.name=es-mini-fixture -e node.name=node-0 \
  --user "$(id -u):0" -v "$WORK/data:/usr/share/elasticsearch/data" \
  docker.elastic.co/elasticsearch/elasticsearch:9.5.5 >/dev/null

echo "waiting for Elasticsearch..."
req() {
  local out code
  out=$(printf '%s' "${3:-}" | docker exec -i "$NAME" curl -s -w '\n%{http_code}' -X "$1" "localhost:9200$2" \
    -H 'content-type: application/json' ${3:+--data-binary @-}) || return 1
  code=${out##*$'\n'}
  if [ "$code" -ge 400 ]; then echo "$1 $2 -> $code ${out%$'\n'*}" >&2; return 1; fi
  printf '%s\n' "${out%$'\n'*}"
}
until req GET '/_cluster/health/fixture?wait_for_status=green&timeout=1s' >/dev/null 2>&1; do sleep 2; done

strip() { python3 -c 'import json,sys; d=json.load(sys.stdin); d.pop("took",None); print(json.dumps(d, indent=2, sort_keys=True, ensure_ascii=False))'; }
record() { req "$2" "$3" "${4:-}" | strip > "expected-aggs/$1.json"; }
record metrics POST /fixture/_search '{"size":0,"aggs":{"avg_views":{"avg":{"field":"views"}},"sum_views":{"sum":{"field":"views"}},"min_rating":{"min":{"field":"rating"}},"max_date":{"max":{"field":"date"}},"count_tag":{"value_count":{"field":"tag"}},"stats_rating":{"stats":{"field":"rating"}},"ext_views":{"extended_stats":{"field":"views"}}}}'
record cardinality_percentiles POST /fixture/_search '{"size":0,"aggs":{"tags":{"cardinality":{"field":"tag"}},"pct":{"percentiles":{"field":"rating","percents":[25,50,99]}},"ranks":{"percentile_ranks":{"field":"views","values":[10,30]}}}}'
record terms POST /fixture/_search '{"size":0,"aggs":{"tags":{"terms":{"field":"tag"},"aggs":{"v":{"avg":{"field":"views"}}}},"pub":{"terms":{"field":"published"}},"by_key":{"terms":{"field":"views","order":{"_key":"desc"},"size":2}}}}'
record terms_query POST /fixture/_search '{"size":0,"query":{"match":{"title":"fox"}},"aggs":{"tags":{"terms":{"field":"tag","order":{"_count":"asc"}}}}}'
record histogram POST /fixture/_search '{"size":0,"aggs":{"h":{"histogram":{"field":"views","interval":10}},"h0":{"histogram":{"field":"rating","interval":1,"min_doc_count":0,"extended_bounds":{"min":0,"max":6}}}}}'
record date_histogram POST /fixture/_search '{"size":0,"aggs":{"m":{"date_histogram":{"field":"date","calendar_interval":"month","format":"yyyy-MM"}},"d":{"date_histogram":{"field":"date","fixed_interval":"30d","keyed":true}}}}'
record ranges POST /fixture/_search '{"size":0,"aggs":{"r":{"range":{"field":"views","ranges":[{"to":5},{"from":5,"to":20},{"from":20}]}},"dr":{"date_range":{"field":"date","format":"yyyy-MM-dd","ranges":[{"to":"2024-02-01"},{"from":"2024-02-01"}]}}}}'
record filters POST /fixture/_search '{"size":0,"query":{"term":{"tag":"animal"}},"aggs":{"f":{"filter":{"term":{"published":true}},"aggs":{"s":{"sum":{"field":"views"}}}},"fs":{"filters":{"filters":{"foxes":{"match":{"title":"fox"}},"dogs":{"match":{"title":"dog"}}}}},"miss":{"missing":{"field":"nope"}},"all":{"global":{},"aggs":{"c":{"value_count":{"field":"views"}}}}}}'
record top_hits POST /fixture/_search '{"size":0,"aggs":{"tags":{"terms":{"field":"tag","size":2},"aggs":{"top":{"top_hits":{"size":1,"sort":[{"views":"desc"}],"_source":{"includes":["title"]}}}}}}}'
record typed_keys POST /fixture/_search?typed_keys '{"size":0,"aggs":{"tags":{"terms":{"field":"tag"},"aggs":{"mx":{"max":{"field":"views"}}}},"st":{"stats":{"field":"views"}}}}'

docker stop -t 60 "$NAME" >/dev/null
echo "recorded $(ls expected-aggs | wc -l) responses in expected-aggs/"
