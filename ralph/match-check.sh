#!/usr/bin/env bash
# Benchmarks the `match` request against Elasticsearch 9.5.5 with the published 100k config and
# fails unless es-mini's mean is at least MIN_SPEEDUP (default 1.10) times faster.
#   ralph/match-check.sh
set -euo pipefail
cd "$(dirname "$0")/.."
out=$(mktemp -d /tmp/esm-match.XXXXXX)
trap 'rm -rf "$out"' EXIT
DOCS=100000 RUNS=100 CPUS=2 MEMORY=2g ONLY=match OUT="$out" bench/run.sh >/dev/null
python3 - "$out/match.json" "${MIN_SPEEDUP:-1.10}" <<'EOF'
import json, statistics, sys
runs = {r["name"]: statistics.mean(m["time_wall_clock"]["value"] for m in r["measurements"])
        for r in json.load(open(sys.argv[1]))["results"]}
speedup = runs["es"] / runs["es-mini"]
print(f"match: es {runs['es'] * 1e3:.2f} ms, es-mini {runs['es-mini'] * 1e3:.2f} ms, {speedup:.2f}x")
sys.exit(speedup < float(sys.argv[2]))
EOF
