# es-mini execution plan

Ordered implementation tasks. Each task lists a **definition of done (DoD)**. Work top to
bottom; later tasks assume earlier ones are complete. Tasks are sized so that most fit in a
single Ralph iteration or a small handful.

Guiding principle: the public HTTP contract is built and tested first (against Elasticsearch's
own YAML REST tests), then on-disk read compatibility, then on-disk write compatibility. Do not
start a codec until the HTTP layer it feeds is exercised by tests.

---

## Phase 0 — Repository and CI bootstrap

### 1. Initialize the Cargo workspace
- Create a Cargo **workspace** at the repo root with member crates:
  - `es-mini` (binary crate: HTTP server entrypoint)
  - `es-codec` (library: Lucene on-disk read/write)
  - `es-query` (library: JSON DSL + ES|QL parsing/execution)
  - `es-index` (library: index/document model, mappings, analysis, storage glue)
- Root `Cargo.toml` pins the Rust edition and a `rust-toolchain.toml` pins a specific stable
  toolchain version.
- **DoD:** `cargo build` and `cargo test` succeed on an empty skeleton (each crate has a trivial
  passing test). Crate boundaries compile against each other in the intended dependency direction
  (`es-mini` → `es-query`/`es-index` → `es-codec`).

### 2. CI workflow
- Add `.github/workflows/ci.yml` running on push and PR.
- Jobs: `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test`, and a build of the
  release binary.
- Cache the Cargo registry/target between runs.
- Add `rustfmt.toml` and `clippy` config as needed; add a `.gitignore` for `target/`.
- **DoD:** A pushed branch triggers CI; all jobs pass green on the Phase 0 skeleton. A deliberately
  malformatted commit fails the `fmt` job (verified once, then reverted).

### 3. Pin the Elasticsearch target version
- Choose and document the exact ES 9.x patch version (Lucene 10 codec) in a top-level
  `COMPATIBILITY.md`. Record the matching commit/tag of `elasticsearch-specification` and of the
  `rest-api-spec` YAML tests.
- Scope is **ES 9.x / Lucene 10 only**. No support for other major versions or older Lucene
  codecs; reject/ignore out-of-version on-disk data with a clear error.
- **DoD:** `COMPATIBILITY.md` names the pinned ES 9.x version, the Lucene 10 codec version, and
  source git refs, and states the single-version support policy explicitly.

---

## Phase 1 — HTTP contract skeleton

### 4. HTTP server scaffold
- Stand up `axum` on `tokio`. Bind `127.0.0.1` on a configurable port (env var or flag, with a
  default). Structured logging via `tracing`.
- Implement `GET /` (info) and `GET /_cluster/health` returning static/plausible payloads shaped
  per the ES specification.
- **DoD:** Binary starts, serves both endpoints, and their JSON responses match the field
  shape/types defined in `elasticsearch-specification` for the pinned version. A smoke test hits
  both endpoints over HTTP and asserts status + key fields.

### 5. In-memory index & document model
- Define the core domain types in `es-index`: index metadata (settings, mappings), a document
  store, and mapping types for the MVP field types (text, keyword, numeric, boolean, date).
- Back everything with an **in-memory** store for now (no disk). This unblocks the whole HTTP
  contract before the codec exists.
- **DoD:** Library types exist with unit tests for mapping parse/serialize and document
  put/get/delete against the in-memory store. No HTTP wiring required yet.

### 6. Index management endpoints
- Wire `PUT /{index}`, `DELETE /{index}`, `GET /{index}`, `HEAD /{index}`,
  `PUT`/`GET /{index}/_settings`, `GET /{index}/_mapping` to the in-memory model.
- Enforce ES-shaped error responses (e.g. `resource_already_exists_exception`,
  `index_not_found_exception`) with correct HTTP status codes.
- **DoD:** Each endpoint returns spec-correct success and error payloads. Integration tests cover
  create/get/delete happy paths and the main error cases.

### 6a. Index aliases
- Add alias metadata (`filter`, `is_write_index`, routing fields stored but unused on one shard) to
  the index model, and the alias endpoints listed in `PLAN.md`, including `POST /_aliases` and
  aliases supplied at index creation.
- Resolve index expressions (comma lists, `*` wildcards, `_all`, aliases) in one place. Reads span
  every target index and apply alias filters; writes go to the single index or the write index.
- **DoD:** Alias endpoints return spec-shaped responses and errors (`aliases_not_found_exception`,
  `invalid_alias_name_exception`, `invalid_index_name_exception`). Integration tests cover reads
  across a multi-index alias, filtered aliases, and write-index selection.

### 7. Document CRUD endpoints
- Wire `POST`/`PUT /{index}/_doc[/{id}]`, `/{index}/_create/{id}`, `GET`/`DELETE /{index}/_doc/{id}`,
  `POST /{index}/_update/{id}`.
- Implement auto-ID generation, version counters, and `result` fields (`created`/`updated`/
  `deleted`/`noop`) as ES returns them.
- **DoD:** Full CRUD works against the in-memory store with spec-correct response envelopes
  (`_index`, `_id`, `_version`, `result`, `_shards`). Integration tests cover create-vs-index
  semantics, missing-doc 404s, and update merge behavior.

### 8. Bulk ingest endpoint
- Implement `POST /_bulk` and `POST /{index}/_bulk` parsing the newline-delimited action/source
  format. Support `index`, `create`, `update`, `delete` actions.
- **DoD:** Bulk requests process each action, returning the per-item `items` array with correct
  status/result per action. Integration test covers a mixed-action bulk body including at least one
  failing item alongside successful ones.

---

## Phase 2 — Text analysis and search

### 9. Analysis: standard + keyword analyzers
- Implement the `keyword` analyzer (identity) and the `standard` analyzer (UAX#29 tokenizer +
  lowercase filter) in `es-index`.
- Term output **must byte-match** Lucene's UAX#29 tokenization. Use an established Unicode
  segmentation crate if it matches; otherwise document the divergence.
- **DoD:** A table-driven unit test asserts tokenization output against a corpus of known
  ES/Lucene term outputs (including punctuation, CJK, mixed-case, numbers). Byte-exact match
  required for the corpus.

### 10. In-memory inverted index + JSON DSL query core
- Build a minimal in-memory inverted index over analyzed fields in `es-query`.
- Implement query execution for `match`, `term`, `bool` (must/should/must_not/filter), `range`,
  plus `from`/`size` pagination and basic `sort`.
- Wire `POST`/`GET /{index}/_search` and `POST /{index}/_count`.
- **DoD:** Search returns spec-correct hit envelopes (`hits.total`, `_score`, `_source`, sorting,
  pagination). Integration tests cover each query type and combinations; `_count` agrees with
  `hits.total`.

### 11. ES|QL parser + executor (minimum viable)
- Implement an ES|QL parser (`nom` or `chumsky`) covering the subset needed to run the in-scope
  YAML tests: source command plus `WHERE`, `LIMIT`, `KEEP`/`DROP`, `SORT` at minimum.
- Wire `POST /_query` returning the columnar `{columns, values}` response shape.
- **DoD:** Parser has unit tests for each supported clause and for parse errors. `POST /_query`
  returns spec-correct columnar results against the in-memory store. Scope of supported grammar is
  documented; unsupported clauses return a clear error rather than silently mis-parsing.

---

## Phase 3 — YAML REST test harness

### 12. Vendor the in-scope YAML REST tests
- Copy YAML test definitions from the pinned `rest-api-spec` ref into `tests/yaml/`, including
  **only** files exercising in-scope features. Record which files were included/excluded and why.
- **Do not modify** any copied test definition. Excluding a whole file is allowed; editing its
  contents is not.
- **DoD:** `tests/yaml/` contains the selected files verbatim with a manifest (e.g.
  `tests/yaml/INCLUDED.md`) listing inclusions/exclusions and rationale.

### 13. YAML REST test runner
- Build a test runner (Rust integration test or a `xtask`-style helper) that parses the ES YAML
  test format: `setup`/`teardown`, `do` blocks, `match`/`length`/`is_true`/`is_false` assertions,
  skip/version stanzas, and stashed values (`$body`, `set`).
- The runner boots the server (in-process or subprocess) and drives it over HTTP.
- **DoD:** The runner executes the vendored suite and produces a pass/fail report per test. It runs
  in CI (allowed to have known failures at this stage, tracked as an explicit allowlist/xfail
  list). At least the Phase 1–2 endpoints have passing tests.

### 14. Ralph loop: drive YAML tests to green
- Implement the Ralph loop (TypeScript + pi agent SDK) that, per iteration, fixes **exactly one**
  failing YAML test without editing any test definition.
- **DoD:** Loop is runnable and documented. The xfail list shrinks over iterations until all
  in-scope YAML tests pass (or remaining failures are documented as genuinely out-of-scope). CI
  enforces "no regressions" against the current pass set.

---

## Phase 4 — On-disk Lucene read compatibility

### 15. Java ES fixture generator
- Script (in `fixtures/` or `xtask`) that boots a real Java Elasticsearch of the pinned version in
  Docker, creates indices, ingests known documents, then shuts it down cleanly, leaving the data
  volume on disk.
- **DoD:** Running the script produces a deterministic on-disk dataset checked for reuse (documented
  layout, reproducible from a single command). The generated data volume is available to Rust tests.

### 16. Lucene 10 read-only codec reader
- In `es-codec`, implement a **read-only** reader for the Lucene 10 segment format needed by the
  MVP: segment infos, stored fields, inverted index (terms/postings), doc values, and the field
  metadata the query core needs. Read-only: no writes yet.
- Build incrementally, one file format at a time, each with its own decode tests against the
  fixture bytes.
- **DoD:** Given the Phase 4 fixture, the reader enumerates segments, reads stored `_source`, and
  reads terms/postings for indexed fields. Unit tests decode each supported Lucene file against
  known expected values.

### 17. Wire the disk reader behind the query/CRUD layer
- Replace (or make swappable with) the in-memory store so that `GET /{index}/_doc/{id}`,
  `_search`, `_count`, and `_query` read from the on-disk codec.
- **DoD:** With the server pointed at the Java-generated data volume, read endpoints return results
  matching what the Java ES instance returned for the same queries. A compatibility test asserts
  parity on a fixed query set.

### 18. Ralph loop: read-compatibility to green
- Per iteration, fix **exactly one** on-disk read point of failure.
- **DoD:** All read-path compatibility checks against the fixture pass, or remaining gaps are
  documented as out-of-scope. Runs in CI (may be a slower/gated job).

---

## Phase 5 — On-disk Lucene write compatibility

### 19. Lucene 10 segment writer
- Extend `es-codec` to **write** segments in the pinned Lucene 10 format: create new segments,
  stored fields, postings, doc values, and update `segments_N` / commit points correctly.
- **DoD:** The Rust server can create an index and ingest documents that produce on-disk segments
  in the correct format. Round-trip test: Rust writes, Rust reads back identical data.

### 20. Cross-engine write compatibility
- Verify the Java ES instance can open and correctly read/search data that the Rust implementation
  wrote (indices created, documents added/updated/deleted).
- **DoD:** After Rust performs writes on a data volume, the pinned Java ES boots on that volume and
  returns correct results for a fixed query set. A compatibility test automates boot → query →
  assert.

### 21. Ralph loop: write-compatibility to green
- Per iteration, fix **exactly one** write-compatibility point of failure.
- **DoD:** All cross-engine write-compatibility checks pass, or remaining gaps are documented as
  out-of-scope. Runs in CI (slower/gated job).

---

## Cross-cutting standards (apply to every task)

- **Contract source of truth:** shape all request/response types against the pinned
  `elasticsearch-specification`. Where practical, derive types from the spec rather than
  hand-writing them.
- **Testing strategy:** unit tests live beside their crate; HTTP integration tests under each
  crate's `tests/`; the YAML suite and cross-engine compatibility tests are their own gated
  workflows. Every task lands with tests green in CI before the next begins.
- **Error shapes:** all error responses match ES's `{error: {type, reason, ...}, status}` envelope.
- **No test-definition edits:** vendored YAML tests are read-only. Fix the implementation, not the
  test.
- **Single-node, local-only:** bind `127.0.0.1`, no auth/TLS, single volume throughout the MVP.
- iteration 19: norms::write_norms landed (all-docs 1-byte/constant norms, byte-equal to fixture). Remaining: SORTED/SORTED_SET DV writer, BKD points (or dims=0), doc->segment assembler, manifest index_generations, run cross_engine.sh.
