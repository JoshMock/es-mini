#!/usr/bin/env bash
# Benchmarks Elasticsearch 9.5.5 against es-mini with hyperfine. Both servers run in Docker with
# the same limits (CPUS, MEMORY) on a private network; hyperfine and curl run in a third
# container there, so the setup is the same for both and no host ports are needed. Needs
# hyperfine on the host (it is mounted into the client container).
#
#   bench/run.sh            # DOCS=10000 RUNS=50 WARMUP=5 CPUS=1 MEMORY=1g JOBS=2
#
# Results: bench/results/<timestamp>/ (summary.md, plus per-request .md and .json).
set -euo pipefail
cd "$(dirname "$0")"
DOCS=${DOCS:-10000} CPUS=${CPUS:-1} MEMORY=${MEMORY:-1g}
NET=es-mini-bench
OUT="$PWD/results/$(date +%Y%m%d-%H%M%S)"
WORK=$(mktemp -d)
stop() {
  docker rm -f es-mini-bench-es es-mini-bench-mini es-mini-bench-client >/dev/null 2>&1 || true
  docker network rm "$NET" >/dev/null 2>&1 || true
  docker volume rm es-mini-bench-es-data es-mini-bench-mini-data >/dev/null 2>&1 || true
}
trap 'stop; rm -rf "$WORK"' EXIT
stop

echo "== building es-mini (release)"
nice cargo build -q --release -j "${JOBS:-2}" -p es-mini
BIN="$(cargo metadata --format-version 1 --no-deps | python3 -c 'import json,sys;print(json.load(sys.stdin)["target_directory"])')/release/es-mini"
HYPERFINE=$(readlink -f "$(command -v hyperfine)")
./gen_data.py "$DOCS" > "$WORK/data.ndjson"

echo "== starting servers (cpus=$CPUS memory=$MEMORY)"
docker network create "$NET" >/dev/null
limits=(--cpus "$CPUS" --memory "$MEMORY" --network "$NET")
docker run -d --name es-mini-bench-es --network-alias es "${limits[@]}" \
  -e discovery.type=single-node -e xpack.security.enabled=false -e xpack.ml.enabled=false \
  -e ES_JAVA_OPTS="-Xms512m -Xmx512m" -v es-mini-bench-es-data:/usr/share/elasticsearch/data \
  docker.elastic.co/elasticsearch/elasticsearch:9.5.5 >/dev/null
# Both servers keep data on a Docker volume, as deployments do. Keep it on a real disk:
# es-mini fsyncs several times per save, so tmpfs would hide most of its write cost.
# Stock images with mounted binaries: nothing to build (es-mini needs glibc >= 2.34).
docker run -d --name es-mini-bench-mini --network-alias es-mini "${limits[@]}" \
  -e ES_MINI_HOST=0.0.0.0 -e ES_MINI_PORT=9200 -e ES_MINI_DATA=/data/es-mini \
  -v es-mini-bench-mini-data:/data \
  -v "$BIN:/usr/local/bin/es-mini:ro" debian:bookworm-slim es-mini >/dev/null

mkdir -p "$OUT"
echo "== benchmarking ($DOCS docs)"
# The client reuses the Elasticsearch image for its curl and bash, plus the host's hyperfine.
docker run --name es-mini-bench-client --network "$NET" --user "$(id -u):0" --entrypoint bash \
  -e RUNS="${RUNS:-50}" -e WARMUP="${WARMUP:-5}" -v "$HYPERFINE:/usr/local/bin/hyperfine:ro" \
  -v "$PWD:/bench:ro" -v "$WORK/data.ndjson:/tmp/data.ndjson:ro" -v "$OUT:/results" \
  docker.elastic.co/elasticsearch/elasticsearch:9.5.5 /bench/inside.sh
echo "== results in $OUT/summary.md"
