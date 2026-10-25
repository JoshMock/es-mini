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
