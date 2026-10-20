#!/usr/bin/env bash
# Regenerates tests/data/* with Lucene 10.5.1 (needs Java 21+ and network for the jars).
# Usage: gen.sh [data-dir-name...]; with no arguments, regenerates everything.
set -euo pipefail
cd "$(dirname "$0")"
cp=""
for m in core analysis-common; do
  jar=/tmp/lucene-$m-10.5.1.jar
  [ -f "$jar" ] || curl -sfL -o "$jar" https://repo1.maven.org/maven2/org/apache/lucene/lucene-$m/10.5.1/lucene-$m-10.5.1.jar
  cp="$cp:$jar"
done
for gen in BigPostings:big-postings Positions:positions BinaryDocValues:binary-doc-values SortedDocValues:sorted-doc-values ManyTerms:many-terms; do
  out=../data/${gen#*:}
  [ $# -eq 0 ] || [[ " $* " == *" ${gen#*:} "* ]] || continue
  rm -rf "$out"
  java -cp "$cp" --add-modules jdk.incubator.vector Gen${gen%:*}.java "$out" 2>/dev/null
  rm -f "$out/write.lock"
done
