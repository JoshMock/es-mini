#!/usr/bin/env bash
# es-mini writes docs with date, float, double, multi-valued and sparse fields into a fresh data dir;
# Elasticsearch 9.5.5 (via scripts/cross_engine.sh) must answer the right _count for term, range and
# exists queries on each of them, phrase queries on text, term queries on dynamic object fields,
# and stored_fields of text mapped with `store`.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DATA="$(mktemp -d)"; PORT=${ES_MINI_PORT:-9392}
cargo build -q -j 2 -p es-mini
BIN="$(cargo metadata --format-version 1 --no-deps | python3 -c 'import json,sys;print(json.load(sys.stdin)["target_directory"])')/debug/es-mini"
ES_MINI_DATA="$DATA" ES_MINI_PORT=$PORT "$BIN" >"$DATA.log" 2>&1 &
PID=$!
trap 'kill $PID 2>/dev/null || true; rm -rf "$DATA" "$DATA.log"' EXIT
for _ in $(seq 50); do curl -s "localhost:$PORT/" >/dev/null && break; sleep 0.2; done
mini() { curl -sf -X "$1" "localhost:$PORT$2" -H 'content-type: application/json' --data-binary "$3" >/dev/null; }
mini PUT /f '{"settings":{"number_of_shards":1,"number_of_replicas":0},"mappings":{"properties":{"d":{"type":"date"},"fl":{"type":"float"},"db":{"type":"double"},"arr":{"type":"long"},"kw":{"type":"keyword"},"t":{"type":"text"},"m":{"type":"text"},"st":{"type":"text","store":true}}}}'
mini POST '/f/_bulk?refresh=true' '{"index":{"_id":"1"}}
{"d":"2024-01-01","fl":1.5,"db":2.5,"arr":[1,2,3],"kw":["a","b"],"t":"quick fox","o":{"n":5,"s":"x"},"st":["one two","three"]}
{"index":{"_id":"2"}}
{"d":"2024-02-01T10:00:00Z","fl":2.5,"db":-1.5,"arr":[3],"m":["brown cat","red hen"]}
{"index":{"_id":"3"}}
{"d":"2024-03-01","arr":[10,20],"kw":"b","t":"lazy dog"}
{"index":{"_id":"4"}}
{"fl":-3.5,"db":7,"kw":["c","c"]}
'
# Writes only reach the translog, which Elasticsearch would replay and index itself; creating an
# index changes the metadata, which makes es-mini commit them as segments.
mini PUT /commit '{"settings":{"number_of_shards":1,"number_of_replicas":0}}'
ls "$DATA"/indices/*/0/index/*.cfs >/dev/null
kill $PID; wait $PID 2>/dev/null || true
EXTRA_CHECKS="$ROOT/scripts/cross_engine_fields_checks.sh" "$ROOT/scripts/cross_engine.sh" "$DATA"
