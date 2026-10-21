# es-mini

A small, single-binary reimplementation of the most-used Elasticsearch APIs in Rust, targeting
Elasticsearch 9.5.5 (see [COMPATIBILITY.md](COMPATIBILITY.md)). It speaks the same HTTP API and
can read and write the same on-disk format (Lucene 10), so the two can take turns on one data
directory.

Single node, local only (binds `127.0.0.1`), no security.

## Features

- **Indices:** create, delete, get, exists, settings, mappings, open/close, aliases
- **Documents:** index, create, get, update (partial and upsert), delete, bulk
- **Search:** `_search` and `_count` with `match`, `term`, `range`, `bool`, sorting and paging,
  scored with Elasticsearch's BM25
- **ES|QL:** `POST /_query` with `FROM`, `WHERE`, `KEEP`, `DROP`, `SORT` and `LIMIT`
- **Storage:** in memory by default, or an Elasticsearch-compatible data directory

Not supported: aggregations, scripting, multi-node clusters, and other APIs outside these lists.

## Build and run

Requires the Rust toolchain pinned in `rust-toolchain.toml`.

```sh
cargo build -p es-mini
cargo run -p es-mini                                # in memory, on 127.0.0.1:9200
ES_MINI_DATA=/path/to/data cargo run -p es-mini     # persist to a data directory
ES_MINI_PORT=9201 cargo run -p es-mini              # another port
```

```sh
curl -X PUT localhost:9200/books/_doc/1 -H 'content-type: application/json' -d '{"title": "Dune"}'
curl 'localhost:9200/books/_search?q=title:dune'
```

## Test

```sh
scripts/check.sh    # formatting, lints, tests (including Elasticsearch's YAML REST tests), docs
```
