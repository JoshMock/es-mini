#!/usr/bin/env bash
# Like gen.sh, but runs Lucene 10.5.1 from the Elasticsearch 9.5.5 Docker image (its bundled JDK
# and lib/ jars), for sandboxes where Java cannot start. Usage: gen_docker.sh GenName data-dir-name
set -euo pipefail
cd "$(dirname "$0")"
gen=${1:?GenName}; out=../data/${2:?data-dir-name}
rm -rf "$out"; mkdir -p "$out"
docker run --rm --cpus 1 --memory 1g --user "$(id -u):0" --entrypoint /usr/share/elasticsearch/jdk/bin/java \
  -v "$PWD:/gen:ro" -v "$(cd "$out" && pwd):/out" docker.elastic.co/elasticsearch/elasticsearch:9.5.5 \
  -cp '/usr/share/elasticsearch/lib/*' --add-modules jdk.incubator.vector "/gen/Gen$gen.java" /out 2>&1 | grep -v WARNING || true
rm -f "$out/write.lock"
ls "$out"
