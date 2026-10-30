#!/usr/bin/env bash
# Records Elasticsearch 9.5.5's responses to searches over 3-shard indices in shard_search.json:
# hit order and its tie-breaks across shards, per-shard and dfs scoring, from/size,
# search_after, aggregation reduction (terms shard_size and doc_count_error_upper_bound), and
# every page of a scroll and of a point in time. Boots a fresh Elasticsearch (no data volume;
# 1 CPU, 1 GB) and removes it afterwards. es-mini/tests/shard_parity.rs replays `setup` and
# compares each case.
set -euo pipefail
cd "$(dirname "$0")"
NAME=es-mini-shards
docker rm -f "$NAME" >/dev/null 2>&1 || true
trap 'docker rm -f "$NAME" >/dev/null 2>&1 || true' EXIT

docker run -d --name "$NAME" --cpus 1 --memory 1g \
  -e discovery.type=single-node -e xpack.security.enabled=false -e xpack.ml.enabled=false \
  -e ES_JAVA_OPTS="-Xms512m -Xmx512m" -e cluster.name=es-mini-fixture -e node.name=node-0 \
  docker.elastic.co/elasticsearch/elasticsearch:9.5.5 >/dev/null
for _ in $(seq 90); do
  docker exec "$NAME" curl -sf 'localhost:9200/_cluster/health?wait_for_status=green&timeout=1s' \
    >/dev/null 2>&1 && break
  sleep 2
done

python3 - "$NAME" > shard_search.json <<'EOF'
import json, random, subprocess, sys
name = sys.argv[1]

def call(method, path, body=None):
    data = body if isinstance(body, str) or body is None else json.dumps(body)
    ctype = "application/x-ndjson" if "_bulk" in path else "application/json"
    out = subprocess.run(
        ["docker", "exec", "-i", name, "curl", "-s", "-X", method, "localhost:9200" + path,
         "-H", "content-type: " + ctype] + (["--data-binary", "@-"] if data else []),
        input=(data or "").encode(), capture_output=True, check=True).stdout
    r = json.loads(out)
    r.pop("took", None)
    return r

words = ["fox", "dog", "quick", "brown", "lazy", "cat", "red", "jumps"]
rng = random.Random(7)
mappings = {"properties": {"title": {"type": "text"}, "tag": {"type": "keyword"},
                           "n": {"type": "long"}, "seq": {"type": "long"}, "x": {"type": "double"}}}
setup = []
for index, shards in [("ss", 3), ("ss2", 3)]:
    body = {"settings": {"number_of_shards": shards, "number_of_replicas": 0}, "mappings": mappings}
    setup.append({"method": "PUT", "path": "/" + index, "body": body})
def bulk(index, ids):
    lines = []
    for i in ids:
        doc = {"title": " ".join(rng.choice(words) for _ in range(rng.randint(1, 6))),
               "tag": rng.choices("abcdefghij", weights=[30, 20, 14, 10, 8, 6, 5, 3, 2, 2])[0],
               "n": i % 5, "seq": i, "x": round(rng.uniform(0, 100), 2)}
        lines += [json.dumps({"index": {"_index": index, "_id": str(i)}}), json.dumps(doc)]
    return {"method": "POST", "path": "/_bulk?refresh=true", "body": "\n".join(lines) + "\n"}
setup += [bulk("ss", range(1, 41)), bulk("ss", range(41, 61)), bulk("ss2", range(1, 13))]
for s in setup:
    r = call(s["method"], s["path"], s["body"])
    assert r.get("acknowledged") or r.get("errors") is False, r

searches = [
    ("match_all", "/ss/_search", {"size": 100}),
    ("match", "/ss/_search", {"size": 100, "query": {"match": {"title": "fox"}}}),
    ("match_two_terms", "/ss/_search", {"size": 100, "query": {"match": {"title": "fox lazy"}}}),
    ("dfs", "/ss/_search?search_type=dfs_query_then_fetch",
     {"size": 100, "query": {"match": {"title": "fox lazy"}}}),
    ("from_size", "/ss/_search", {"from": 7, "size": 9, "query": {"match": {"title": "dog"}}}),
    ("constant_from_size", "/ss/_search", {"from": 20, "size": 15, "query": {"term": {"tag": "a"}}}),
    ("sort_ties", "/ss/_search", {"size": 30, "sort": [{"n": "asc"}]}),
    ("sort_desc_ties", "/ss/_search", {"size": 30, "sort": [{"n": "desc"}, {"tag": "asc"}]}),
    ("search_after", "/ss/_search",
     {"size": 10, "sort": [{"n": "asc"}, {"seq": "desc"}], "search_after": [2, 33]}),
    ("search_after_ties", "/ss/_search", {"size": 15, "sort": [{"n": "asc"}], "search_after": [1]}),
    ("two_indices", "/ss,ss2/_search", {"size": 100}),
    ("two_indices_sorted", "/ss2,ss/_search", {"size": 40, "sort": [{"n": "desc"}]}),
    ("terms_shard_size", "/ss/_search",
     {"size": 0, "aggs": {"t": {"terms": {"field": "tag", "size": 3, "shard_size": 3}}}}),
    ("terms_error", "/ss/_search",
     {"size": 0, "aggs": {"t": {"terms": {"field": "tag", "size": 2, "shard_size": 2,
                                          "show_term_doc_count_error": True}}}}),
    ("terms_default", "/ss/_search",
     {"size": 0, "aggs": {"t": {"terms": {"field": "tag", "size": 4}},
                          "n": {"terms": {"field": "n", "size": 2, "shard_size": 2}}}}),
    ("terms_asc", "/ss/_search",
     {"size": 0, "aggs": {"t": {"terms": {"field": "tag", "size": 3, "shard_size": 3,
                                          "order": {"_count": "asc"}}}}}),
    ("terms_sub", "/ss/_search",
     {"size": 0, "query": {"match": {"title": "dog"}},
      "aggs": {"t": {"terms": {"field": "tag", "size": 3, "shard_size": 4},
                     "aggs": {"x": {"avg": {"field": "x"}},
                              "top": {"top_hits": {"size": 2, "_source": False}}}}}}),
    ("metrics", "/ss/_search",
     {"size": 0, "aggs": {"s": {"stats": {"field": "x"}}, "c": {"cardinality": {"field": "tag"}},
                          "p": {"percentiles": {"field": "x", "percents": [10, 50, 90]}},
                          "h": {"histogram": {"field": "n", "interval": 2}}}}),
]
cases = [{"name": n, "kind": "search", "path": p, "body": b, "responses": [call("POST", p, b)]}
         for n, p, b in searches]

def scroll(n, path, body):
    pages = []
    r = call("POST", path, body)
    while True:
        sid = r.pop("_scroll_id")
        pages.append(r)
        if not r["hits"]["hits"]:
            break
        r = call("POST", "/_search/scroll", {"scroll": "1m", "scroll_id": sid})
    call("DELETE", "/_search/scroll", {"scroll_id": sid})
    return {"name": n, "kind": "scroll", "path": path, "body": body, "responses": pages}
cases.append(scroll("scroll", "/ss/_search?scroll=1m", {"size": 7, "query": {"match": {"title": "dog"}}}))
cases.append(scroll("scroll_sorted", "/ss/_search?scroll=1m", {"size": 11, "sort": [{"n": "asc"}]}))
cases.append(scroll("scroll_doc", "/ss,ss2/_search?scroll=1m", {"size": 13, "sort": ["_doc"]}))

# Each page after the first searches after the last hit of the one before.
def pit(n, index, body):
    pid = call("POST", f"/{index}/_pit?keep_alive=1m")["id"]
    pages, after = [], None
    while True:
        b = dict(body, pit={"id": pid, "keep_alive": "1m"}, **({"search_after": after} if after else {}))
        r = call("POST", "/_search", b)
        pid = r.pop("pit_id")
        pages.append(r)
        if not r["hits"]["hits"]:
            break
        after = r["hits"]["hits"][-1]["sort"]
    call("DELETE", "/_pit", {"id": pid})
    return {"name": n, "kind": "pit", "path": f"/{index}/_pit?keep_alive=1m", "body": body,
            "responses": pages}
cases.append(pit("pit_sorted", "ss", {"size": 12, "sort": [{"n": "asc"}]}))
cases.append(pit("pit_score", "ss", {"size": 8, "query": {"match": {"title": "fox"}},
                                     "sort": ["_score"]}))
print(json.dumps({"setup": setup, "cases": cases}, ensure_ascii=False, indent=1))
EOF
docker stop -t 60 "$NAME" >/dev/null
