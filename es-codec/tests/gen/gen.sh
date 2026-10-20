#!/usr/bin/env bash
# Regenerates tests/data/big-postings with Lucene 10.5.1 (needs Java 21+ and network for the jar).
set -euo pipefail
cd "$(dirname "$0")"
cp=""
for m in core analysis-common; do
  jar=/tmp/lucene-$m-10.5.1.jar
  [ -f "$jar" ] || curl -sfL -o "$jar" https://repo1.maven.org/maven2/org/apache/lucene/lucene-$m/10.5.1/lucene-$m-10.5.1.jar
  cp="$cp:$jar"
done
rm -rf ../data/big-postings
java -cp "$cp" --add-modules jdk.incubator.vector GenBigPostings.java ../data/big-postings
rm -f ../data/big-postings/write.lock
