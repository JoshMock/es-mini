#!/usr/bin/env bash
# Fetches a source file from Lucene or Elasticsearch at the versions pinned in COMPATIBILITY.md
# into /tmp/es-mini-src/<project>/<path> and prints the local path.
# Usage: fetch_source.sh lucene lucene/core/src/java/org/apache/lucene/codecs/lucene104/Lucene104PostingsWriter.java
#        fetch_source.sh es server/src/main/java/org/elasticsearch/lucene/similarity/LegacyBM25Similarity.java
#        fetch_source.sh es-find LegacyBM25Similarity    # find a class's path in elastic/elasticsearch
set -euo pipefail
case "${1:?usage: fetch_source.sh lucene|es|es-find PATH}" in
  lucene) base=https://raw.githubusercontent.com/apache/lucene/releases/lucene/10.5.1 ;;
  es) base=https://raw.githubusercontent.com/elastic/elasticsearch/v9.5.5 ;;
  es-find) gh api "search/code?q=class+${2:?class}+repo:elastic/elasticsearch" -q '.items[].path'; exit ;;
  *) echo "unknown project $1" >&2; exit 1 ;;
esac
out="/tmp/es-mini-src/$1/${2:?path}"
mkdir -p "$(dirname "$out")"
curl -sfL "$base/$2" -o "$out" || { echo "not found at the pinned version: $2" >&2; exit 1; }
echo "$out"
