---
name: verify-with-elasticsearch
description: Check es-mini's behavior or on-disk output against a real Elasticsearch 9.5.5 in Docker. Use when a change affects files es-mini writes, when a response's correctness against real ES is in doubt, or when a goal's done-criteria require Elasticsearch to read Rust-written data.
---

# Verify with Elasticsearch 9.5.5

Real Elasticsearch is the ground truth for es-mini. Use the recorded responses when they cover
the case, and boot Elasticsearch when they do not.

## Safety Rules (Do Not Violate)

- One container at a time, `--cpus 1 --memory 1g`, always removed afterwards, even on failure.
  Check `docker ps -a --filter name=es-mini` before and after.
- Send requests through `docker exec ... curl`; the sandbox cannot reach ports Docker publishes.
  (Plain localhost connections to a local es-mini process do work.)
- Boot Elasticsearch on a copy of a data directory, never the original: it rewrites the files
  it opens.
- Never rerun `fixtures/generate.sh` or write into `fixtures/es-9.5.5/` (including by pointing
  `ES_MINI_DATA` at it): tests depend on those exact bytes and the recorded index UUID.

## Choose the cheapest check

1. Recorded responses (no Docker): `cargo test -p es-mini --test fixture_parity` (replays the
   fixture's writes in memory) and `--test disk_parity` (reads the ES-written data directory).
   Both compare against `fixtures/expected/*.json`.
2. Elasticsearch reading es-mini's output: `scripts/cross_engine_rust.sh` replays the fixture's
   writes through the es-mini binary into a fresh data directory, then `scripts/cross_engine.sh`
   boots Elasticsearch on it and compares 9 recorded responses (scores excluded; they depend on
   segment layout). About 3 minutes.
3. Anything else: write a data directory (e.g. a test that calls `es_index::disk::save` and
   leaves its scratch directory), then run
   `.agents/skills/verify-with-elasticsearch/scripts/es_on_data_dir.sh <data-dir> <requests-file>` with one `METHOD PATH [BODY]` per line,
   and compare the counts and hits with what es-mini returns.

## Notes

- Elasticsearch's cluster metadata in `_state/` is compressed; inspect it with
  `es_codec::cluster_state::read_indices` (e.g. a throwaway `examples/` file you delete
  afterwards), not `strings`.
- The fixture's data directory also holds Elasticsearch's own system indices (e.g.
  `.ds-.logs-elasticsearch.deprecation-*`); the `fixture` index's UUID is in
  `fixtures/expected/index_uuid`.
- Elasticsearch scores with `LegacyBM25Similarity` (Lucene BM25 times `1 + k1`), and its term
  statistics count updated and deleted docs until segments merge.
