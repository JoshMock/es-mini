#!/usr/bin/env bash
# Records which shard Elasticsearch 9.5.5 routes documents to, in shard_routing.json.
# Boots a fresh Elasticsearch (no data volume; 1 CPU, 1 GB), creates indices with several
# shard counts (default and explicit number_of_routing_shards), bulk-indexes documents by _id
# and by custom routing, and reads each hit's `_shard` from an explain search.
set -euo pipefail
cd "$(dirname "$0")"
NAME=es-mini-routing
WORK=$(mktemp -d)
docker rm -f "$NAME" >/dev/null 2>&1 || true
trap 'docker rm -f "$NAME" >/dev/null 2>&1 || true; rm -rf "$WORK"' EXIT

docker run -d --name "$NAME" --cpus 1 --memory 1g \
  -e discovery.type=single-node -e xpack.security.enabled=false -e xpack.ml.enabled=false \
  -e ES_JAVA_OPTS="-Xms512m -Xmx512m" docker.elastic.co/elasticsearch/elasticsearch:9.5.5 >/dev/null
req() {
  docker exec -i "$NAME" curl -sf -X "$1" "localhost:9200$2" \
    -H "content-type: ${3:-application/json}" --data-binary @-
}
for _ in $(seq 90); do
  echo -n | req GET '/_cluster/health?wait_for_status=yellow&timeout=1s' 2>/dev/null \
    | grep -q '"timed_out":false' && break
  sleep 2
done

# index name -> settings; ids and routings cover ASCII, multi-byte UTF-8, and UTF-16 surrogates.
INDICES='{"r1":{"number_of_shards":1},"r2":{"number_of_shards":2},"r3":{"number_of_shards":3},
"r5":{"number_of_shards":5},"r7":{"number_of_shards":7,"number_of_routing_shards":14},
"r8":{"number_of_shards":8,"number_of_routing_shards":8}}'
IDS='["1","2","3","4","5","6","7","8","9","10","11","12","13","14","15","16","17","18","19","20",
"abc","hello world","ünïcödé","日本語","😀emoji","aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]'
ROUTINGS='["user1","x","日本","😀","42"]'

for n in $(python3 -c "import json,sys; print(*json.loads(sys.argv[1]))" "$INDICES"); do
  python3 -c "import json,sys; s=json.loads(sys.argv[1])[sys.argv[2]]; s['number_of_replicas']=0
print(json.dumps({'settings':{'index':s}}))" "$INDICES" "$n" | req PUT "/$n" >/dev/null
done
python3 -c "
import json,sys
idx,ids,rts=(json.loads(a) for a in sys.argv[1:])
for n in idx:
    for i in ids: print(json.dumps({'index':{'_index':n,'_id':i}})); print('{}')
    for r in rts: print(json.dumps({'index':{'_index':n,'_id':'rt-'+r,'routing':r}})); print('{}')
" "$INDICES" "$IDS" "$ROUTINGS" > "$WORK/bulk.ndjson"
req POST '/_bulk?refresh=true' application/x-ndjson < "$WORK/bulk.ndjson" | grep -q '"errors":false'

echo -n | req GET '/_cluster/state/metadata/r*?filter_path=metadata.indices.*.routing_num_shards' \
  > "$WORK/meta.json"
for n in $(python3 -c "import json,sys; print(*json.loads(sys.argv[1]))" "$INDICES"); do
  echo '{"size":100,"explain":true}' \
    | req POST "/$n/_search?filter_path=hits.hits._id,hits.hits._shard" > "$WORK/hits_$n.json"
done

python3 - "$WORK" "$INDICES" > shard_routing.json <<'EOF'
import json, re, sys
work, indices = sys.argv[1], json.loads(sys.argv[2])
meta = json.load(open(f"{work}/meta.json"))["metadata"]["indices"]
out = []
for n, s in indices.items():
    cases = []
    for h in json.load(open(f"{work}/hits_{n}.json"))["hits"]["hits"]:
        i = h["_id"]
        routing = i[3:] if i.startswith("rt-") else None
        cases.append({"id": i, "routing": routing, "shard": int(re.search(r"\[(\d+)\]$", h["_shard"]).group(1))})
    cases.sort(key=lambda c: c["id"])
    out.append({"index": n, "settings": s,
                "routing_num_shards": meta[n]["routing_num_shards"], "cases": cases})
print(json.dumps(out, ensure_ascii=False, indent=1))
EOF
