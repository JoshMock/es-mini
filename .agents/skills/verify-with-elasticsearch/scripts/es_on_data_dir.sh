#!/usr/bin/env bash
# Boots Elasticsearch 9.5.5 in Docker on a COPY of a data directory and runs requests against it.
# Capped at 1 CPU / 1 GB; requests go through `docker exec` (the sandbox cannot reach Docker's
# published ports); the container and the copy are always removed.
# Usage: es_on_data_dir.sh DATA_DIR REQUESTS_FILE
# REQUESTS_FILE: one request per line, `METHOD PATH [JSON BODY]`; `#` lines are skipped.
set -euo pipefail
src=${1:?usage: es_on_data_dir.sh DATA_DIR REQUESTS_FILE}; requests=${2:?requests file}
name=es-mini-check; work=$(mktemp -d)
trap 'docker rm -f "$name" >/dev/null 2>&1 || true; rm -rf "$work"' EXIT
docker rm -f "$name" >/dev/null 2>&1 || true
cp -r "$src" "$work/data"; rm -f "$work/data/node.lock"
docker run -d --name "$name" --cpus 1 --memory 1g -e discovery.type=single-node \
  -e xpack.security.enabled=false -e xpack.ml.enabled=false -e ES_JAVA_OPTS="-Xms512m -Xmx512m" \
  -e cluster.name=es-mini-fixture -e node.name=node-0 --user "$(id -u):0" \
  -v "$work/data:/usr/share/elasticsearch/data" docker.elastic.co/elasticsearch/elasticsearch:9.5.5 >/dev/null
req() { printf '%s' "${3:-}" | docker exec -i "$name" curl -s -X "$1" "localhost:9200$2" \
  -H 'content-type: application/json' ${3:+--data-binary @-}; }
for _ in $(seq 90); do
  req GET '/_cluster/health?wait_for_status=yellow&timeout=1s' 2>/dev/null | grep -q '"timed_out":false' && break
  sleep 2
done
while read -r method path body; do
  case "$method" in ''|'#'*) continue ;; esac
  echo "> $method $path $body"; req "$method" "$path" "$body" | cut -c1-2000; echo
done < "$requests"
echo "== errors in the ES log"
docker logs "$name" 2>&1 | grep -iE 'exception|corrupt' | grep -v rest.suppressed | cut -c1-400 | head -10 || true
