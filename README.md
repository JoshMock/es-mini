# es-mini

A small, single-binary reimplementation of the most-used Elasticsearch APIs in Rust, targeting
Elasticsearch 9.5.5 (see [COMPATIBILITY.md](COMPATIBILITY.md)). It speaks the same HTTP API and
can read and write the same on-disk format (Lucene 10), so the two can take turns on one data
directory.

Single node, no security; binds `127.0.0.1` unless `ES_MINI_HOST` says otherwise.

## Features

- **Indices:** create, delete, get, exists, settings, mappings, open/close, aliases, stats, `_refresh`
- **Documents:** index, create, get, `_source`, `_mget`, update (partial and upsert), delete, `_bulk`,
  `_update_by_query`, `_delete_by_query`
- **Search APIs:** `_search`, `_count`, `_msearch`, search templates, `_field_caps`, scroll and
  point-in-time; sorting and paging, scored with Elasticsearch's BM25
- **Queries:** `match_all`, `match`, `match_phrase`, `match_phrase_prefix`, `multi_match`, `term`,
  `terms`, `prefix`, `wildcard`, `range`, `exists`, `ids`, `bool`, `constant_score`,
  `query_string`, `simple_query_string`
- **Aggregations:** metrics (`avg`, `sum`, `min`, `max`, `value_count`, `stats`, `extended_stats`,
  `cardinality`, `percentiles`, `percentile_ranks`, `top_hits`); buckets (`terms`, `histogram`,
  `date_histogram`, `range`, `date_range`, `filter`, `filters`, `missing`, `nested`, `global`);
  pipeline (`derivative`, `max_bucket`, `bucket_selector`, `bucket_sort`)
- **ES|QL:** `POST /_query` with `FROM`, `ROW`, `SHOW`, `WHERE`, `EVAL`, `STATS ... BY`, `KEEP`,
  `DROP`, `RENAME`, `SORT`, `LIMIT`; aggregates (`COUNT`, `COUNT_DISTINCT`, `SUM`, `AVG`, `MIN`,
  `MAX`, `MEDIAN`, `PERCENTILE`, `VALUES`) and string, math, date (`DATE_TRUNC`, `BUCKET`,
  `DATE_EXTRACT`, `DATE_FORMAT`), conversion (`TO_*`), multi-value and `CASE`/`COALESCE` functions
- **Cluster info:** `/`, `_cluster/health`, `_cluster/stats`, `_cluster/settings`, `_nodes`
- **Storage:** in memory by default, or an Elasticsearch-compatible Lucene 10 data directory.
  Writes are fsynced to a translog and replayed on start; segments are committed on flush, so
  Elasticsearch and es-mini can take turns on one directory.
- **Speed:** vs Elasticsearch 9.5.5 on 100,000 documents, es-mini is about 1.1-1.4x faster on
  document get/index, match and aggregations, and 2.5-12x slower on `match_all`, `count`, and
  ES|QL; see [bench/RESULTS.md](bench/RESULTS.md).

Out of scope: multi-node clusters, `_cat`, ILM, vector storage, async search, scripting (Painless),
runtime fields, ingest pipelines, highlighting, suggesters, percolator, geo queries, nested/join
fields (see `z_old/PLAN.md`).

## Build and run

Requires the Rust toolchain pinned in `rust-toolchain.toml`.

```sh
cargo build -p es-mini
cargo run -p es-mini                                # in memory, on 127.0.0.1:9200
ES_MINI_DATA=/path/to/data cargo run -p es-mini     # persist to a data directory
ES_MINI_PORT=9201 cargo run -p es-mini              # another port
ES_MINI_HOST=0.0.0.0 cargo run -p es-mini           # another bind address (default 127.0.0.1)
```

```sh
curl -X PUT localhost:9200/books/_doc/1 -H 'content-type: application/json' -d '{"title": "Dune"}'
curl 'localhost:9200/books/_search?q=title:dune'
```

## Test

```sh
scripts/check.sh    # formatting, lints, tests (including Elasticsearch's YAML REST tests), docs
```
