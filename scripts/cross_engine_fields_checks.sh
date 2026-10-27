# Sourced by cross_engine.sh (EXTRA_CHECKS): `req` talks to Elasticsearch; sets the exit status.
fail=0
count() { # expected query-json
  got=$(req POST /f/_count "{\"query\":$2}" | python3 -c 'import json,sys;print(json.load(sys.stdin).get("count","error"))')
  if [ "$got" = "$1" ]; then echo "ok   $2"; else echo "FAIL $2: want $1, got $got"; fail=1; fi
}
# Aggregations read doc values themselves (both fields below are table-encoded).
agg() { # expected aggregation field
  got=$(req POST /f/_search "{\"size\":0,\"aggs\":{\"a\":{\"$2\":{\"field\":\"$3\"}}}}" |
    python3 -c 'import json,sys;print(json.load(sys.stdin).get("aggregations",{}).get("a",{}).get("value","error"))')
  if [ "$got" = "$1" ]; then echo "ok   $2 $3"; else echo "FAIL $2 $3: want $1, got $got"; fail=1; fi
}
agg 39.0 sum arr
agg 1709251200000.0 max d
count 1 '{"term":{"d":"2024-01-01"}}'
count 2 '{"range":{"d":{"gte":"2024-02-01"}}}'
count 3 '{"exists":{"field":"d"}}'
count 1 '{"term":{"fl":1.5}}'
count 2 '{"range":{"fl":{"gte":0}}}'
count 1 '{"range":{"fl":{"lt":0}}}'
count 3 '{"exists":{"field":"fl"}}'
count 1 '{"term":{"db":7}}'
count 2 '{"range":{"db":{"gte":0}}}'
count 3 '{"exists":{"field":"db"}}'
count 2 '{"term":{"arr":3}}'
count 1 '{"range":{"arr":{"gte":10}}}'
count 3 '{"range":{"arr":{"gte":2,"lte":10}}}'
count 3 '{"exists":{"field":"arr"}}'
count 2 '{"term":{"kw":"b"}}'
count 1 '{"term":{"kw":"c"}}'
count 3 '{"exists":{"field":"kw"}}'
count 1 '{"match":{"t":"quick"}}'
count 2 '{"exists":{"field":"t"}}'
count 1 '{"match_phrase":{"t":"quick fox"}}'
count 0 '{"match_phrase":{"t":"fox quick"}}'
count 1 '{"match_phrase":{"t":{"query":"fox quick","slop":2}}}'
# Values of an array are 100 positions apart (position_increment_gap).
count 0 '{"match_phrase":{"m":{"query":"cat red","slop":99}}}'
count 1 '{"match_phrase":{"m":{"query":"cat red","slop":100}}}'
# Dynamic object fields, written under their dotted paths.
count 1 '{"term":{"o.n":5}}'
count 1 '{"match":{"o.s":"x"}}'
count 1 '{"term":{"o.s.keyword":"x"}}'
# Values longer than ignore_above are not indexed.
count 1 '{"term":{"lg":"abc"}}'
count 0 '{"term":{"lg":"abcd"}}'
count 1 '{"exists":{"field":"lg"}}'
count 2 '{"term":{"_ignored":"lg"}}'
count 2 '{"exists":{"field":"_ignored"}}'
# Values of another JSON type, coerced as Elasticsearch does.
count 1 '{"term":{"ck":"42"}}'
count 1 '{"term":{"ck":"true"}}'
count 1 '{"term":{"cn":7}}'
count 1 '{"range":{"cn":{"gte":8}}}'
count 1 '{"term":{"cb":true}}'
# date_nanos keep nanosecond precision.
count 1 '{"term":{"dn":"2024-01-01T00:00:00.123456789Z"}}'
count 1 '{"range":{"dn":{"gt":"2024-01-01T00:00:00.123456Z","lt":"2024-01-01T00:00:00.1234568Z"}}}'
count 1 '{"range":{"dn":{"lt":"2024-01-01T00:00:00.1231Z"}}}'
count 2 '{"exists":{"field":"dn"}}'
# Ranges: gt/lt bounds exclude their value, gt and lte dates round up to the day's end.
count 2 '{"term":{"lr":4}}'
count 1 '{"term":{"lr":10}}'
count 2 '{"term":{"lr":15}}'
count 1 '{"term":{"lr":-10}}'
count 1 '{"range":{"lr":{"gte":5,"lte":9}}}'
count 1 '{"range":{"lr":{"gte":0,"lte":30,"relation":"within"}}}'
count 2 '{"range":{"lr":{"gte":12,"lte":14,"relation":"contains"}}}'
count 3 '{"exists":{"field":"lr"}}'
count 2 '{"term":{"dr":"2024-01-18"}}'
count 1 '{"term":{"dr":"2024-01-10"}}'
count 1 '{"range":{"dr":{"lte":"2024-01-15"}}}'
count 1 '{"range":{"dr":{"gte":"2024-01-21","lte":"2024-01-25"}}}'
count 2 '{"exists":{"field":"dr"}}'
# Nested objects are child documents: matched one object at a time, and not documents themselves.
count 5 '{"match_all":{}}'
count 2 '{"nested":{"path":"ns","query":{"term":{"ns.a":"p"}}}}'
count 1 '{"nested":{"path":"ns","query":{"bool":{"must":[{"term":{"ns.a":"p"}},{"term":{"ns.n":1}}]}}}}'
count 0 '{"nested":{"path":"ns","query":{"bool":{"must":[{"term":{"ns.a":"q"}},{"term":{"ns.n":1}}]}}}}'
count 1 '{"nested":{"path":"ns.r","query":{"term":{"ns.r.x":"u"}}}}'
count 0 '{"term":{"ns.a":"p"}}'
# Range aggregations read the binary doc values.
buckets() { # expected query aggregation
  got=$(req POST /f/_search "{\"size\":0,\"query\":$2,\"aggs\":{\"a\":$3}}" |
    python3 -c 'import json,sys;print(" ".join("%s:%s" % (b["key"], b["doc_count"]) for b in json.load(sys.stdin).get("aggregations",{}).get("a",{}).get("buckets",[])) or "error")')
  if [ "$got" = "$1" ]; then echo "ok   $3"; else echo "FAIL $3: want $1, got $got"; fail=1; fi
}
buckets "0.0:1 10.0:1 20.0:1" '{"term":{"lr":2}}' '{"histogram":{"field":"lr","interval":10}}'
buckets "1705363200000:1 1705449600000:1 1705536000000:1 1705622400000:1 1705708800000:1" \
  '{"ids":{"values":["2"]}}' '{"date_histogram":{"field":"dr","calendar_interval":"day"}}'
# Text mapped with `store` comes back from the Lucene stored field, not `_source`.
got=$(req POST /f/_search '{"stored_fields":["st"],"query":{"ids":{"values":["1"]}}}' |
  python3 -c 'import json,sys;print(json.load(sys.stdin)["hits"]["hits"][0].get("fields"))')
want="{'st': ['one two', 'three']}"
if [ "$got" = "$want" ]; then echo "ok   stored_fields st"; else echo "FAIL stored_fields st: want $want, got $got"; fail=1; fi
exit $fail
