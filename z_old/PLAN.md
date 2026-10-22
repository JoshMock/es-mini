# es-mini MVP plan

## Goal

A Rust rewrite of the most-used Elasticsearch API endpoints, producing a small, efficient single-binary executable that exposes an HTTP API that can do:

- create/delete/configure indices
- bulk document ingest
- document CRUD operations
- document search (JSON search DSL and ES|QL only)

The executable should support the exact same on-disk format as Elasticsearch, so that the two could be used interchangeably on the same data volumes (though likely not at the same time, to avoid the complexity of any lock-related conflicts, race conditions, etc).

### Supported endpoints

Indices:
- `PUT /{index}` (create + mappings/settings)
- `DELETE /{index}`
- `GET /{index}`, `HEAD /{index}`
- `PUT /{index}/_settings`, `GET /{index}/_settings`
- `GET /{index}/_mapping`
- `POST /{index}/_close`, `POST /{index}/_open` (closed indices keep metadata and settings but reject document reads, writes, and searches)

Aliases:
- `PUT`/`POST`/`DELETE /{index}/_alias/{name}` (and `_aliases/{name}`)
- `GET /_alias`, `/_alias/{name}`, `/{index}/_alias`, `/{index}/_alias/{name}`; `HEAD` on the named forms
- `POST /_aliases` (`add`, `remove`, `remove_index` actions)
- Aliases resolve for reads (search, count, ES|QL, GET; with alias `filter`) and writes (single index or `is_write_index`); `require_alias` honored

Documents:
- `POST`/`PUT /{index}/_doc`, `/{index}/_doc/{id}`, `/{index}/_create/{id}`
- `GET`/`HEAD`/`DELETE /{index}/_doc/{id}`, `GET /{index}/_source/{id}`
- `GET`/`POST /_mget`, `/{index}/_mget`
- `POST /{index}/_update/{id}`
- `POST /_bulk`, `/{index}/_bulk`

Search:
- `POST`/`GET /{index}/_search` (JSON DSL)
- `POST /_query` (ES|QL)
- `POST /{index}/_count`

Cluster (minimum viable):
- `GET /` (info), `GET /_cluster/health`

### Elasticsearch features that are out of scope

This is a non-exhaustive list:

- multi-node; single-node support only
- `_cat` APIs
- index lifecycle management
- vector storage (for now, unless it's easy to support "for free")
- multi-search, async search
- any other search dialect aside from JSON DSL and ES|QL
- scripting (Painless), runtime fields, ingest pipelines/processors
- highlighting, suggesters, percolator, geo queries
- nested/join field types (flat mappings first)
- cross-cluster search, snapshots/restore
- all aggregations deferred to a post-MVP iteration

In-scope query core: `match`, `term`, `terms`, `ids`, `exists`, `wildcard`, `bool`, `constant_score`, `range`, pagination (`from`/`size`), basic `sort`.

## Tech stack

- HTTP: `axum` on `tokio` (hyper)
- JSON: `serde` + `serde_json`
- ES|QL: custom parser (`nom` or `chumsky`)
- On-disk format: custom Lucene codec, **read-only reader prototyped first**. Writer, `tantivy`, and FFI-to-Lucene approaches all deferred until read compatibility is proven.
- Target version: **Elasticsearch 9.x, latest minor** (Lucene 10 codec), pinned to a specific patch version for on-disk and REST/YAML-test compatibility.
- Codec commitment: hard-commit to a native Rust read-only reader as the prototype goal. No FFI-to-Lucene fallback built preemptively; only revisit FFI if the prototype proves the native approach infeasible.
- Text analysis (MVP): `standard` analyzer (standard tokenizer + lowercase filter) and `keyword` only. Term generation must byte-match Lucene's UAX#29 output. Language analyzers, custom char/token filters, synonyms, and stemming deferred.
- Ralph loop: written in TypeScript using the pi agent SDK
- Security (MVP): unsecured, local-only (bind `127.0.0.1`). No TLS, no auth. TLS, API keys, basic auth, and RBAC deferred post-MVP.
- Performance (MVP): no formal targets. Correctness/compatibility first. Functional aim: single-node dataset on one volume, low millions of docs, no pathological latency. Benchmarking deferred post-MVP.

## Methodology

1. Using a [Ralph](https://github.com/snarktank/ralph)-type loop, iteratively implement the public HTTP contracts for each of a selected set of Elasticsearch APIs
2. Go through all the [YAML-based REST test definitions](https://github.com/elastic/elasticsearch/blob/main/rest-api-spec/src/yamlRestTest/resources/rest-api-spec/test/) and copy all the definitions to this repo that test only the features this version will support
3. write a test runner that uses those test definitions as its source of test definitions
4. run the tests and, with each Ralph iteration, aim to fix exactly one broken test per iteration; **DO NOT** change the test definition at all without explicit permission
4. move on to on-disk read-only compatibility testing: spin up a Java-based Elasticsearch cluster locally, use it to create some indices and documents. then, shut down the instance and test whether the Rust implementation can do **read-only** operations on the generated files. for each Ralph iteration, work until exactly one point of failure is fixed
4. repeat the same compatibility testing, but using the Rust implementation to add/update/delete documents and indices, then verify whether the Java implementation still works with the now-modified data on disk

## Resources

- The [Elasticsearch specification](https://github.com/elastic/elasticsearch-specification) `main` branch should be used to establish the public API contract for the supported endpoints
- Code generators that operate on that spec may also be useful for learning how to read it. e.g. [JS code generator](https://github.com/elastic/elastic-client-generator-js)
