#!/usr/bin/env bash
# Task 20: replays fixtures/generate.sh's writes through es-mini into a fresh data dir, then runs
# scripts/cross_engine.sh on it (Elasticsearch 9.5.5 must open it and match fixtures/expected/).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DATA="$(mktemp -d)"; PORT=${ES_MINI_PORT:-9391}
cargo build -q -j 2 -p es-mini
BIN="$(cargo metadata --format-version 1 --no-deps | python3 -c 'import json,sys;print(json.load(sys.stdin)["target_directory"])')/debug/es-mini"
ES_MINI_DATA="$DATA" ES_MINI_PORT=$PORT "$BIN" >"$DATA.log" 2>&1 &
PID=$!
trap 'kill $PID 2>/dev/null || true; rm -rf "$DATA" "$DATA.log"' EXIT
for _ in $(seq 50); do curl -s "localhost:$PORT/" >/dev/null && break; sleep 0.2; done
req() { curl -sf -X "$1" "localhost:$PORT$2" -H 'content-type: application/json' --data-binary "$3" >/dev/null; }
req PUT /fixture '{"settings":{"number_of_shards":1,"number_of_replicas":0},"mappings":{"properties":{"title":{"type":"text"},"tag":{"type":"keyword"},"views":{"type":"long"},"rating":{"type":"double"},"published":{"type":"boolean"},"date":{"type":"date"}}}}'
req POST '/fixture/_bulk?refresh=true' '{"index":{"_id":"1"}}
{"title":"The quick brown fox","tag":"animal","views":10,"rating":4.5,"published":true,"date":"2024-01-01"}
{"index":{"_id":"2"}}
{"title":"Quick quick rabbits","tag":"animal","views":25,"rating":3.0,"published":false,"date":"2024-02-15"}
{"index":{"_id":"3"}}
{"title":"Lazy dog sleeps","tag":"pet","views":3,"rating":2.5,"published":true,"date":"2023-12-31"}
{"index":{"_id":"4"}}
{"title":"A brown dog and a fox","tag":"pet","views":7,"rating":5.0,"published":true,"date":"2024-03-10"}
{"index":{"_id":"5"}}
{"title":"Ünïcödé café naïve 東京","tag":"misc","views":0,"rating":1.0,"published":false,"date":"2024-04-01"}
'
req POST '/fixture/_bulk?refresh=true' '{"index":{"_id":"6"}}
{"title":"Fresh fox news","tag":"news","views":42,"rating":3.5,"published":true,"date":"2024-05-05"}
{"update":{"_id":"2"}}
{"doc":{"views":26}}
{"delete":{"_id":"5"}}
'
kill $PID; wait $PID 2>/dev/null || true
"$ROOT/scripts/cross_engine.sh" "$DATA"
