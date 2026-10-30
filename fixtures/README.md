# On-disk fixtures

`generate.sh` boots Elasticsearch 9.5.5 in Docker (1 CPU, 1 GB) on `es-9.5.5/data`, creates the
`fixture` index (text, keyword, long, double, boolean, date fields; 1 shard, 0 replicas), ingests
documents in two refreshes (two segments, including an update and a delete), flushes, records
responses to fixed queries in `expected/`, and stops Elasticsearch cleanly.

```sh
fixtures/generate.sh   # regenerates everything; about 3 minutes
```

Layout: `es-9.5.5/data/indices/<index-uuid>/0/index/` holds the Lucene files (`segments_N`, `.si`,
compound `.cfs`/`.cfe`, doc values). The index-name-to-UUID mapping is in the cluster metadata
under `es-9.5.5/data/_state/`. File bytes include UUIDs and timestamps and differ per run; the
logical content and `expected/` are deterministic.

`generate_aggs.sh` boots Elasticsearch on a copy of `es-9.5.5/data` (leaving it untouched) and
records its responses to aggregation requests in `expected-aggs/`; the parity tests compare
those too, in memory and from disk.

`generate_esql.sh` boots a fresh Elasticsearch (no data volume), ingests its `emp` and
`emp_extra` indices, and records the response to each query in `esql-queries.txt` (one per line)
as `expected-esql/NNN.json`. `es-mini/tests/esql_parity.rs` replays the ingest in memory and
compares columns and values, or the error type for invalid queries; known differences are in
`esql-parity-xfail.txt`, which may only shrink.

`generate_shard_routing.sh` boots a fresh Elasticsearch, creates indices with several shard
counts, indexes documents by `_id` and by custom routing, and records the shard each landed on
in `shard_routing.json`; `es-index/tests/shard_routing.rs` checks `es_index::routing` against it.
