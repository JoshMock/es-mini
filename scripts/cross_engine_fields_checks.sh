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
exit $fail
