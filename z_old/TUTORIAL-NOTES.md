# Tutorial Series: Shared Agent Notes

This file is maintained by tutorial-authoring subagents. Each agent appends notes for the next agent after writing its tutorial.

## Series Overview

Target audience: 20+ years experience in JS/Python/Linux/DevOps/SRE. Familiar with Elasticsearch as an API. Not experienced with Rust.

Tone: Instructional, instructive. Explains Rust concepts by analogy to JS/Python where helpful. Each tutorial ends with a numbered Definition of Done checklist with specific validation steps.

## Project: es-mini

A Rust reimplementation of Elasticsearch core (read/write compatible with ES 9.x / Lucene 10).
Crates:
- `es-mini` — binary, HTTP server entrypoint
- `es-codec` — library, Lucene on-disk read/write
- `es-query` — library, JSON DSL + ES|QL parsing/execution
- `es-index` — library, index/document model, mappings, analysis, storage glue

Dependency direction: `es-mini` → `es-query`/`es-index` → `es-codec`

## Tutorial Index

| # | Title | Phase/Tasks | Status |
|---|-------|-------------|--------|
| 01 | Cargo Workspaces & Project Bootstrap | Phase 0, Task 1 | **done** |
| 02 | CI, Formatting, and Linting | Phase 0, Task 2 | **done** |
| 03 | Rust Types, Structs, Enums, and Serde | Phase 0-1 transition | **done** |
| 04 | Async Rust: Tokio and Axum Basics | Phase 1, Task 4 | **done** |
| 05 | Error Handling the Rust Way | cross-cutting (needed before Task 6) | **done** |
| 06 | Shared State in Axum (Arc, Mutex, RwLock) | Phase 1 infrastructure | **done** |
| 07 | Index Management Endpoints | Phase 1, Task 6 | **done** |
| 08 | Document CRUD Endpoints | Phase 1, Task 7 | **done** |
| 09 | Bulk Ingest and NDJSON Parsing | Phase 1, Task 8 | **done** |
| 10 | Text Analysis: Iterators, Traits, and Unicode | Phase 2, Task 9 | **done** |
| 11 | In-Memory Inverted Index and JSON Query DSL | Phase 2, Task 10 | **done** |
| 12 | Parser Combinators with nom: ES\|QL Subset | Phase 2, Task 11 | **done** |
| 13 | Integration Testing and YAML REST Test Harness | Phase 3, Tasks 12-13 | **done** |
| 14 | Reading Binary Formats: Lucene Codec | Phase 4, Tasks 15-17 | **done** |
| 15 | Writing Binary Formats: Lucene Segment Writer | Phase 5, Tasks 19-21 | **done** |

## Accumulated Notes

---

### Tutorial 01 — Cargo Workspaces & Project Bootstrap

**Written:** Phase 0, Task 1

**Concepts covered:**
- `rustup` installation and toolchain management (analogized to nvm/pyenv)
- `rust-toolchain.toml` TOML format with `[toolchain]` section — NOT the legacy plain-text format
- Cargo workspace structure: root `Cargo.toml` with `[workspace]`, no `[package]` at root
- `resolver = "2"` — required/default for edition 2021+, explicit for edition 2024
- `[workspace.package]` inheritance with `version.workspace = true`, `edition.workspace = true`
- Binary crate (`src/main.rs`) vs library crate (`src/lib.rs`) convention
- Path dependencies: `{ path = "../es-codec" }` — workspace-internal linking
- `[workspace.dependencies]` shorthand — mentioned but deferred as premature for 4 crates
- `cargo check` / `cargo build` / `cargo run` / `cargo test` — differences and when to use each
- `-p` flag for targeting a specific workspace member
- `target/` directory structure and why to `.gitignore` it
- `Cargo.lock` commit policy: commit for binary-containing workspaces, not for library-only crates
- Hyphen vs underscore: package names use hyphens, Rust module identifiers use underscores

**Rust version specifics:**
- Current stable as of authoring: **1.97.1** (8bab26f4f, 2026-07-14)
- `rustup` version: 1.29.0
- Edition 2024 is stable as of Rust 1.85 — used in this project as it's greenfield
- `rust-toolchain.toml` TOML format: `[toolchain]` + `channel = "1.97.1"` — verify this version is still current when Tutorial 02 is authored and update if needed

**DoD structure used:**
- 10 items, mix of `cargo` command output verification and structural checks
- Item 9 ("verify negative compile") is a useful pattern — tests that the dependency wall holds
- Item 10 (grep for stray `[package]` in root) is a common beginner mistake, worth keeping

**Suggestions for Tutorial 02 — CI, Formatting, and Linting:**
- Reader already has the workspace from Tutorial 01; Tutorial 02 should assume it exists and passes `cargo build`
- Cover: `.github/workflows/ci.yml` with jobs for `fmt --check`, `clippy -- -D warnings`, `test`, release build
- Cover: `rustfmt.toml` minimal config (max_width, edition)
- Cover: why `clippy -- -D warnings` (warnings as errors in CI, not locally) and how to allow specific lints with `#[allow(...)]`
- Cover: caching the Cargo registry and `target/` with `actions/cache` or `Swatinem/rust-cache`
- Cover: the `actions-rs` vs direct `cargo` invocation debate — just use `cargo` directly, simpler
- Note: `rust-toolchain.toml` is automatically respected by GitHub Actions runners when `rustup` is present (it is on all `ubuntu-latest` runners)
- Good DoD validation: push a deliberately malformatted commit, verify fmt job fails; revert
- The `cargo fmt` vs `cargo fmt --check` distinction is important: `--check` exits non-zero without modifying files

---

### Tutorial 02 — CI, Formatting, and Linting

**Written:** Phase 0, Task 2

**Concepts covered:**
- `rustfmt` as Prettier/Black analogue — opinionated, non-negotiable, community-standard output
- `cargo fmt` (in-place) vs `cargo fmt --all --check` (CI mode, exits non-zero without modifying)
- `rustfmt.toml` — minimal config: `edition = "2021"`, `max_width = 100`; deep config discouraged
- `clippy` as ESLint + flake8 + pylint analogue; catches logic bugs, not just style
- `cargo clippy --workspace --all-targets -- -D warnings` — the canonical CI invocation
- `--all-targets` includes test code, examples, benchmarks — not just lib/bin
- Lint levels: `allow`, `warn`, `deny`, `forbid`
- `#![deny(clippy::all)]` inner attribute in each crate root — applies to whole crate
- `#!` prefix = "applies to enclosing item" (the crate for root files)
- `#[allow(clippy::specific_lint)]` per-item suppression — localized and reviewable
- `-D warnings` flag at CLI level catches rustc warnings too (beyond just clippy lints)
- `#[cfg(test)]` — conditional compilation; entire test module stripped from release binary
- `mod tests { use super::*; }` — standard test module pattern
- `#[test]` attribute marks test functions; must take no args, return `()`
- `assert_eq!(left, right)` macro — panics with diagnostic on mismatch; panicking test = failing test
- Fallible tests: can return `Result<(), E>` instead of `()`
- `cargo test -p <crate>` vs `cargo test --workspace`

**CI yaml specifics (for later tutorials to reuse/extend):**
- Action: `dtolnay/rust-toolchain@stable` — current standard, `actions-rs` is unmaintained (last commit 2022)
- Action: `Swatinem/rust-cache@v2` — handles `~/.cargo/registry`, `~/.cargo/git`, `target/` with automatic key invalidation
- Cache must come AFTER toolchain install (cache key includes resolved rustc version)
- `CARGO_TERM_COLOR: always` — forces color in CI logs; GitHub Actions renders ANSI colors
- `RUST_BACKTRACE: 1` — prints backtraces on panics in CI; essential for debugging test failures
- Single job `ci:` with steps: checkout → toolchain → cache → fmt → clippy → test
- `cargo-deny-action@v2` from EmbarkStudios — prebuilt binary, avoids compile step
- `rust-toolchain.toml` is automatically respected on `ubuntu-latest` (rustup is pre-installed)

**cargo deny:**
- Still the recommended supply-chain tool in 2025 — actively maintained by EmbarkStudios
- Minimum Rust requirement: 1.88.0 (per README badge at time of writing)
- `cargo install --locked cargo-deny && cargo deny init && cargo deny check`
- Checks: advisories (RustSec DB), licenses, banned crates, allowed source registries
- CI: use `EmbarkStudios/cargo-deny-action@v2` (prebuilt binary, no compile time)
- `deny.toml` created by `cargo deny init` in workspace root

**Release profile:**
- `[profile.release]` in root `Cargo.toml` only
- `lto = true` — full LTO; `lto = "thin"` if compile time becomes painful at scale
- `codegen-units = 1` — single optimizer pass; better optimization, slower compile
- `strip = true` — strips debug symbols; 5-10x binary size reduction
- `strip = "debuginfo"` alternative — keeps symbol names for crash reporting

**Files created/modified in this tutorial:**
- `.github/workflows/ci.yml` — new
- `rustfmt.toml` — new, workspace root
- `deny.toml` — new (optional, from `cargo deny init`)
- `Cargo.toml` (root) — `[profile.release]` section added
- `es-mini/src/main.rs` — `#![deny(clippy::all)]` added
- `es-codec/src/lib.rs` — `#![deny(clippy::all)]` added
- `es-query/src/lib.rs` — `#![deny(clippy::all)]` + `placeholder()` fn + `#[cfg(test)]` module
- `es-index/src/lib.rs` — `#![deny(clippy::all)]` added

**Suggestions for Tutorial 03 — Rust Types, Structs, Enums, and Serde:**
- CI is now green; Tutorial 03 should add real dependencies (`serde`, `serde_json`) to `[workspace.dependencies]`
- Introduce `[workspace.dependencies]` here — now there's an actual reason to use it
- Cover: `derive` macros (`#[derive(Debug, Clone, Serialize, Deserialize)]`) — connect to Python dataclasses / TypeScript interfaces
- Cover: enums as sum types (not C-style enums) — critical for Rust idiom, no analogue in JS/Python
- Cover: `Option<T>` as the null-safety replacement — no `null`, no `None` that isn't typed
- Cover: `impl` blocks — methods on structs vs free functions
- Target data types: `IndexSettings`, `Mapping`, `FieldType` enum — first real es-mini domain types
- The `#[serde(rename_all = "snake_case")]` attribute will be needed for ES JSON compat immediately

---

### Tutorial 03 — Rust Types, Structs, Enums, and Serde

**Written:** Phase 0–1 transition

**Concepts covered:**
- Numeric types: `i32`, `i64`, `u32`, `u64`, `f64`, `f32`, `usize`, `bool` — purpose and when to use each
- `String` (owned, heap-allocated) vs `&str` (borrowed slice) — the core ownership distinction for strings
- `&String` coerces to `&str` automatically at call sites
- Structs: definition, field visibility (`pub` on struct AND on fields required for external access)
- `impl` blocks: associated functions (`fn new(...)` — no `self`), methods (`fn name(&self)`)
- `Self` keyword in impl blocks
- Struct update syntax `..other` — like JS spread, must be last
- Enums as algebraic data types (not C-style int constants)
- `match` expression: exhaustiveness enforced at compile time — missing arm = compile error
- Enums with data (preview, not fully implemented yet): `FieldValue::Text(String)` pattern
- `Option<T>`: `Some(v)` / `None` — replaces null/undefined; `unwrap()`, `expect()`, `unwrap_or()`, `if let Some(x)`
- `Result<T, E>`: `Ok(v)` / `Err(e)` — functions that can fail; `?` operator for early return propagation
- `Vec<T>`: dynamic array, `push`, `len`, indexing, `vec![]` macro, iterate with `&vec`
- `HashMap<K,V>`: hash map, `insert`, `get` returns `Option<&V>`, `.entry().or_insert()`
- `BTreeMap<K,V>`: sorted iteration — use for deterministic output (e.g., `_cat/indices` by name)
- Ownership basics: move semantics, `.clone()` for explicit copy, `&T` references for borrowing
- Copy types (`i32`, `u32`, `f64`, `bool`, `usize`) vs non-Copy types (`String`, `Vec`)
- `#[derive(Debug)]` — always add to domain types for `{:?}` formatting
- `#[derive(Clone)]` — deep copy; needed when storing in HashMap and also returning references

**Serde patterns established:**
- Workspace dependency declaration: `serde = { version = "1", features = ["derive"] }` in `[workspace.dependencies]`
- Member crate opt-in: `serde = { workspace = true }` in `[dependencies]`
- `serde_json = { version = "1" }` — the JSON backend; separate crate from serde core
- `#[derive(Serialize, Deserialize)]` on structs and enums
- `#[serde(rename_all = "snake_case")]` on enums — PascalCase Rust variants → snake_case JSON strings
- `#[serde(skip_serializing_if = "Option::is_none")]` — omit absent Optional fields from JSON output
- `#[serde(rename = "field_name")]` — override a specific field's JSON name (used for `pri`, `rep`, `docs.count`)
- `#[serde(default)]` — use `Default::default()` for missing fields during deserialization (mentioned not heavily used yet)
- `serde_json::to_string(&val)` — serialize to JSON string
- `serde_json::from_str::<T>(json)` — deserialize from JSON string
- `serde_json::from_value::<T>(v)` / `serde_json::to_value(&val)` — convert between `Value` and typed structs
- `serde_json::Value` — dynamic JSON escape hatch; like TS `any` or Python `dict`
- `json!({...})` macro for building `Value` inline
- `#[derive(PartialEq)]` needed for `assert_eq!` comparisons in tests

**Types defined in `es-index/src/lib.rs`** (do not redefine in later tutorials):
- `FieldType` enum: `Text`, `Keyword`, `Integer`, `Float`, `Boolean`, `Date` — serde snake_case
- `IndexMetadata` struct: `name: String`, `number_of_shards: u32`, `number_of_replicas: u32`, `uuid: Option<String>`, `creation_date: Option<i64>`
  - `impl IndexMetadata { pub fn new(...) -> Self; pub fn name(&self) -> &str }`
- `ClusterHealthResponse` struct — matches actual ES `/_cluster/health` JSON field names exactly; all snake_case fields
  - `impl ClusterHealthResponse { pub fn healthy_empty(cluster_name: &str) -> Self }`
- `IndexInfo` struct — matches ES `/_cat/indices?format=json` JSON field names; uses `#[serde(rename)]` for `pri`, `rep`, `docs.count`, `docs.deleted`, `store.size`
  - `impl IndexInfo { pub fn from_metadata(meta: &IndexMetadata) -> Self }`
- `DynamicJson` type alias: `pub type DynamicJson = serde_json::Value`
- `IndexStore` struct: `indices: HashMap<String, IndexMetadata>` — in-memory store
  - `impl IndexStore { new, create, get, delete, list (sorted), len, is_empty }`
  - `#[derive(Debug, Default)]` on `IndexStore`

**Ownership concepts introduced** (reference in later tutorials without re-explaining):
- Move semantics: assigning a non-Copy value transfers ownership; original is invalid
- `.clone()` for explicit deep copy
- `&T` references: borrow without taking ownership; can borrow multiple times from same owner
- `&str` vs `String`: `&str` for read-only access to string data, `String` for owned storage
- `&String` auto-coerces to `&str` at function call sites
- Functions that store data should take owned values or clone; functions that read should take `&T`
- `self` (consume) vs `&self` (read borrow) vs `&mut self` (mutable borrow) in impl methods

**Deferred to later tutorials:**
- Lifetimes (`'a`) — not needed for owned-field structs; deferred to Tutorial 10
- Trait definitions and implementations — `#[derive]` only here; manual `impl Trait` in Tutorial 10
- Custom error types — Tutorial 05 builds `EsError`; this tutorial uses `serde_json::Error` directly
- `Arc<RwLock<IndexStore>>` — deferred to Tutorial 06; `IndexStore` as written is single-threaded
- `&mut self` and `Mutex`/`RwLock` — deferred to Tutorial 06

**Suggestions for Tutorial 04 — Async Rust: Tokio and Axum:**
- `ClusterHealthResponse::healthy_empty("es-mini")` should be the response body for `GET /_cluster/health`
- `IndexStore` will need to be wrapped in `Arc<RwLock<...>>` — introduce or defer to Tutorial 06
- Add `tokio` and `axum` to `[workspace.dependencies]` in root `Cargo.toml`
- `es-mini/Cargo.toml` needs `tokio` and `axum` added
- ES-compatible info endpoint `GET /`: returns JSON with `name`, `cluster_name`, `version.number`, `tagline`
- Introduce `#[tokio::main]` and `async fn main()`
- Axum `Router`, `axum::Json(val)`, `axum::extract::State(state)` handler pattern
- Keep state injection minimal in Tutorial 04 — use `Arc<ClusterInfo>` (read-only) before introducing `RwLock`
- Note: `serde_json::to_string` → Axum's `Json(val)` handles serialization; don't double-serialize
- `IndexStore` import will be `use es_index::IndexStore` in `es-mini`


## Tutorial 04 — Async Rust: Tokio and Axum Basics

**Written:** Phase 1, Task 4

### Versions established
- `axum = "0.8"` (current: 0.8.9)
- `tokio = "1"` with `features = ["full"]` (current: 1.53.1)
- `tracing = "0.1"` (current: 0.1.44)
- `tracing-subscriber = "0.3"` with `features = ["env-filter", "json"]` (current: 0.3.23)

### Axum 0.8 API patterns established
- `Router::new().route("/path", get(handler)).with_state(state)`
- Handler signature: `async fn handler(State(s): State<Arc<AppState>>) -> impl IntoResponse`
- Response: `(StatusCode, Json<T>)` tuple — implements `IntoResponse`
- `State<T>` requires `T: Clone` — use `Arc<AppState>` so clone is cheap
- `axum::serve(listener, app).with_graceful_shutdown(signal).await`
- Debug attribute: `#[axum::debug_handler]` for better handler type errors
- `Json<T>` as return value: serializes T, sets Content-Type application/json
- 404 for unknown routes is automatic — no explicit handler needed

### Tokio patterns established
- `#[tokio::main]` — multi-thread runtime (default), suitable for HTTP servers
- `#[tokio::main(flavor = "current_thread")]` — single-thread, for tests/tools
- `tokio::net::TcpListener::bind(addr).await` — async TCP binding
- `tokio::signal::ctrl_c().await` — graceful shutdown signal
- `tokio::select!` — race multiple futures (for SIGTERM + SIGINT)
- `tokio::spawn(async move { ... })` — background tasks; captures must be `Send + 'static`

### Tracing setup pattern
```rust
tracing_subscriber::fmt()
    .with_env_filter(
        tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| "es_mini=info".into()),
    )
    .init();
```
- Controlled by `RUST_LOG` env var
- `.json()` chain for JSON output (production)
- `info!()`, `warn!()`, `error!()`, `debug!()` macros
- `#[instrument]` attribute for automatic function spans

### Endpoint shapes implemented
**`GET /`** → 200 `application/json`
```json
{
  "name": "es-mini",
  "cluster_name": "<CLUSTER_NAME env or 'es-mini'>",
  "cluster_uuid": "_na_",
  "version": {
    "number": "9.0.0",
    "build_flavor": "default",
    "build_type": "tar",
    "lucene_version": "10.1.0"
  },
  "tagline": "You Know, for Search"
}
```
Struct: `RootResponse` + `VersionInfo` in `es-mini/src/main.rs` (not shared, local to binary)

**`GET /_cluster/health`** → 200 `application/json`
Body: `ClusterHealthResponse::healthy_empty(cluster_name)` from `es-index`
All fields present: `cluster_name`, `status`, `timed_out`, `number_of_nodes`, `number_of_data_nodes`,
`active_primary_shards`, `active_shards`, `relocating_shards`, `initializing_shards`,
`unassigned_shards`, `delayed_unassigned_shards`, `number_of_pending_tasks`,
`number_of_in_flight_fetch`, `task_max_waiting_in_queue_millis`, `active_shards_percent_as_number`

### Configuration
- `ES_HOST` env var — default `127.0.0.1`
- `ES_PORT` env var — default `9200`
- `CLUSTER_NAME` env var — default `es-mini`
- No config crate yet — `std::env::var()` is sufficient

### AppState shape at end of Tutorial 04
```rust
#[derive(Clone)]
struct AppState {
    cluster_name: String,
}
// Wrapped as Arc<AppState> in .with_state()
```
Tutorial 06 expands this to include `Arc<RwLock<IndexStore>>`.

### Notes for Tutorial 05 (Error Handling)
- Current handlers use `.unwrap()` in main — acceptable for bind failure
- Handlers return `impl IntoResponse` — Error tutorial will replace this with a typed `EsError`
- ES error shape: `{"error": {"type": "..._exception", "reason": "..."}, "status": NNN}`
- `IntoResponse` impl on `EsError` is the key pattern — handlers then return `Result<impl IntoResponse, EsError>`
- Common error types needed: `index_not_found_exception` (404), `resource_already_exists_exception` (400), `mapper_parsing_exception` (400), `action_request_validation_exception` (400)

---

### Tutorial 05 — Error Handling the Rust Way

**Written:** Cross-cutting (needed before Task 6 — index management endpoints)

**Concepts covered:**
- Errors as values (`Result<T, E>`), no exceptions for recoverable errors — analogy to Go `(value, error)` and the `Either` monad
- `#[must_use]` on `Result` — why ignoring a `Result` warns instead of silently swallowing
- `?` operator — sugar for match + early return + `From::from` conversion; common mistake (using `?` in a non-`Result`-returning fn) with compiler output
- `unwrap()`/`expect()` — acceptable in tests/prototyping/genuinely unreachable branches (paired with `unreachable!()`); NOT acceptable in HTTP handlers (untrusted input must never panic a handler task)
- `Box<dyn std::error::Error>` — fast composition via blanket `From<E>` impl; fine for `main()`/tools, bad at library API boundaries (no structured `match` without downcasting)
- `thiserror` — `#[derive(Error)]`, `#[error("...")]` (Display format string, `{0}`/`{field}`), `#[from]` (auto `From` impl enabling `?`)
- `anyhow` — `anyhow::Result<T>`, `.context()` (cheap, fixed string), `.with_context(|| ...)` (lazy closure); analogy to Python `raise X from original`
- thiserror vs anyhow decision table: thiserror for libraries (`es-index`, `es-query`, `es-codec`) where callers `match` on variants; anyhow for application/binary code with opaque, ergonomic error wrapping
- axum `IntoResponse` pattern: implement once on a custom error enum, every handler returns `Result<T, AppError>` and uses `?` freely
- `axum::response::IntoResponse` / `axum::response::Response` — confirmed still at this path in axum 0.8.x (unchanged since 0.6)

**Versions verified (compiled and tested against, `cargo test` passing):**
- `thiserror = "2"` → resolves to **2.0.19**. Derive API (`#[derive(Error)]`, `#[error("...")]`, `#[from]`) unchanged from v1 — the 2.0 breaking changes were internal (MSRV etc.), not the derive surface.
- `anyhow = "1"` → resolves to **1.0.104**. NOT added as a dependency yet in `es-mini` — no call site needs it (deferred until multi-step fallible setup code exists, e.g. config loading).
- Confirms axum 0.8.9 / tokio 1.53.1 still current (matches Tutorial 04's pinned versions).

**`AppError` enum defined in `es-mini/src/error.rs`** (do not redefine in later tutorials — extend with new variants as needed):
```rust
#[derive(Debug, Error)]
pub enum AppError {
    IndexNotFound(String),                      // 404, "index_not_found_exception"
    ResourceAlreadyExists(String),               // 400, "resource_already_exists_exception"
    DocumentNotFound { index: String, id: String }, // 404, "document_missing_exception" (real ES exception type name — NOT "document_not_found_exception")
    InvalidRequest(String),                      // 400, "action_request_validation_exception"
    Internal(String),                             // 500, "internal_server_exception"
}
```
- `status_code(&self) -> StatusCode` and `error_type(&self) -> &'static str` are private methods on `AppError` — exhaustive `match`, compiler forces new variants to get both a status and a type string
- `.to_string()` (via `#[error("...")]` Display) becomes the `reason` field — no separate reason-formatting logic needed

**`IntoResponse` pattern for Tutorial 06+ to reuse:**
```rust
#[derive(Debug, Serialize, PartialEq, Eq)]
struct EsErrorBody {
    #[serde(rename = "type")]
    error_type: String,
    reason: String,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
struct EsError {
    error: EsErrorBody,
    status: u16,
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = self.status_code();
        let body = EsError {
            error: EsErrorBody { error_type: self.error_type().to_string(), reason: self.to_string() },
            status: status.as_u16(),
        };
        (status, Json(body)).into_response()
    }
}
```
- Handler shape from Tutorial 06 onward: `async fn handler(...) -> Result<impl IntoResponse, AppError>` — use `.ok_or_else(|| AppError::IndexNotFound(name.clone()))?` etc. inside
- ES error shape implemented is the flat two-level form (`{"error": {"type", "reason"}, "status"}`) — deliberately omits real ES's `root_cause` array (multi-shard aggregation, out of scope for single-node in-memory MVP)
- Test pattern for handler-adjacent code: `axum::body::to_bytes(response.into_body(), usize::MAX).await` to read a `Response` body back in a test — reuse in Tutorial 07 handler integration tests

**Which crate owns what error type:**
- `es-mini` (binary): `AppError` — HTTP-facing, `thiserror`-based, implements `IntoResponse`. Lives here because HTTP status/JSON shape is a presentation concern, not a domain concern.
- `es-index` (library): **no error type yet** — `IndexStore` methods (Tutorial 03) don't return `Result`. Deferred until concurrent access (Tutorial 06, `RwLock`) introduces real failure modes (e.g. lock poisoning) worth modeling. When added, it should be a separate `thiserror` enum (e.g. `IndexStoreError`) with zero HTTP/axum knowledge; `AppError` would gain a variant wrapping it via `#[from]`.
- `es-query`, `es-codec`: no error type yet, not reached.

**Files created/modified in this tutorial:**
- `es-mini/src/error.rs` — new: `AppError` enum, `EsError`/`EsErrorBody` structs, `IntoResponse` impl, 6 unit tests (all variant mappings + one `IntoResponse` body/status test)
- `es-mini/src/main.rs` — `mod error; pub use error::AppError;` added; no handler changes (Tutorial 04's two handlers can't fail, nothing to wire yet)
- `Cargo.toml` (root) — `thiserror = { version = "2" }` added to `[workspace.dependencies]`
- `es-mini/Cargo.toml` — `thiserror = { workspace = true }` added
- `anyhow` deliberately NOT added anywhere yet

**Suggestions for Tutorial 06 — Shared State in Axum (Arc, Mutex, RwLock):**
- `IndexStore` (Tutorial 03) needs wrapping in `Arc<RwLock<IndexStore>>` for `.with_state()` — this is the first place `AppError::Internal` might get a real call site (e.g. mapping a poisoned lock)
- Consider whether `IndexStore` methods should start returning `Result` here, or defer that to Tutorial 07 when `PUT /{index}` needs `AppError::ResourceAlreadyExists` — Tutorial 07 is the more natural landing spot since that's the first real fallible domain operation
- `AppError::IndexNotFound` and `AppError::ResourceAlreadyExists` currently have zero call sites in the codebase — Tutorial 06/07 is where they get exercised for the first time by an actual handler
- The `Result<impl IntoResponse, AppError>` handler shape (§7/§9 of Tutorial 05) is ready to use as soon as there's a fallible operation to wrap

---

### Tutorial 06 — Shared State in Axum: Arc, Mutex, and RwLock

**Written:** Phase 1 infrastructure

**Concepts covered:**
- Why sharing `IndexStore` directly across axum handlers is either a compile error (bare struct field, no wrapper) or a silent correctness bug (clone-per-handler with `State<T>: Clone` — each handler mutates its own copy)
- `Arc<T>` — atomic refcounted pointer; `Arc::clone()`/`.clone()` is a refcount bump, not a deep copy; `Arc` vs `Rc` (multi-thread vs single-thread), enforced by `Send` bound at compile time
- `Arc<T>` alone only yields `&T` — doesn't solve mutation, only multi-ownership
- `Mutex<T>`: exclusive lock, `.lock()` blocks, returns `MutexGuard<T>` (RAII — auto-unlocks on drop)
- `RwLock<T>`: many readers OR one writer, `.read()`/`.write()`, preferred for es-mini's read-heavy shape
- Lock poisoning: panic while holding lock → `Err(PoisonError<T>)` on next acquire; `.unwrap()` in tests, `.unwrap_or_else(|p| p.into_inner())` in handler code (recover-and-continue policy, not a silent swallow)
- `std::sync::RwLock` is **not reentrant** — acquiring a second lock (even read) while already holding one from the same `RwLock`, same task, deadlocks. Fix: scope the guard with `{ }` so it drops before the next lock acquisition
- Negative-compile example: two `thread::spawn` closures both capturing `&mut IndexStore` → `E0499` at compile time, before any thread runs. Fix requires `Arc<Mutex<_>>`/`Arc<RwLock<_>>` — there is no other way past the borrow checker for this case
- `std::sync::RwLock` vs `tokio::sync::RwLock`: use `tokio::sync` only if a guard is held across an `.await`; es-mini's `IndexStore` ops are all synchronous in-memory lookups, so `std::sync::RwLock` is correct and stays correct through Phase 1–3 (revisit at Phase 4 on-disk codec if async I/O needs to happen mid-lock)

**Design decision — outer `Arc<AppState>` wrapper removed:**
- Tutorial 04 wrapped `AppState` in an outer `Arc` because `State<T>: Clone` was needed and `AppState` only held a `String`
- Now that `AppState` holds `Arc<RwLock<IndexStore>>` (already cheap to clone), the whole struct is cheap to `#[derive(Clone)]` directly — the outer `Arc<AppState>` is redundant and was dropped
- Every handler's extractor changed from `State<Arc<AppState>>` to `State<AppState>` — flagged as a common mismatch-after-refactor error (same `E0277` shape as Tutorial 04 §14's original example, different direction)

**Final `AppState` struct (in `es-mini/src/main.rs`, do not redefine — extend with new fields as needed):**
```rust
use std::sync::{Arc, RwLock};
use es_index::IndexStore;

#[derive(Clone)]
struct AppState {
    cluster_name: String,
    store: Arc<RwLock<IndexStore>>,
}
```
- Constructed once in `main()`: `let store = Arc::new(RwLock::new(IndexStore::new())); let state = AppState { cluster_name, store };`
- Passed directly to `.with_state(state)` — no outer `Arc::new(state)` wrapper (see design decision above)
- All handlers: `State(state): State<AppState>`

**The `Arc<RwLock<IndexStore>>` handler pattern — follow this in Tutorial 07+:**
```rust
async fn some_handler(State(state): State<AppState>) -> impl IntoResponse {
    // Reads:
    let result = {
        let store = state.store.read().unwrap_or_else(|p| p.into_inner());
        /* do read-only work, build an owned return value */
    }; // <- guard dropped here, before anything else (esp. before any .await)

    // Writes (Tutorial 07 — PUT/DELETE):
    {
        let mut store = state.store.write().unwrap_or_else(|p| p.into_inner());
        /* mutate */
    } // <- guard dropped here

    (StatusCode::OK, Json(result))
}
```
- Rule: never hold a `read()`/`write()` guard across an `.await` point, and never acquire a second lock on the same `RwLock` while already holding one (deadlock, not a compile error) — scope guards with `{ }` blocks
- `IndexStore` methods (`list()`, `create()`, etc.) still don't return `Result` — first real fallible call site against the store is Tutorial 07 (`PUT`/`DELETE` needing `AppError::IndexNotFound`/`ResourceAlreadyExists`)

**Sync primitives in use:**
- `std::sync::{Arc, RwLock}` — the only ones in the codebase as of Tutorial 06
- `tokio::sync::RwLock`/`Mutex` — NOT used, deliberately (no handler `.await`s while holding the store lock); mentioned in the tutorial as the thing to reach for if that ever changes (e.g. async on-disk I/O mid-lock in Phase 4+)
- `parking_lot` — NOT adopted; `std::sync::RwLock` has no extra dependency cost and poisoning is treated as a useful signal, not friction, at this scale

**`GET /_cat/indices` endpoint added:**
- Route: `GET /_cat/indices` → `cat_indices_handler`
- Body: `Vec<IndexInfo>` (from `es-index`, Tutorial 03), built via `store.list().into_iter().map(IndexInfo::from_metadata).collect()`
- Empty store → `200` with `[]` (not an error — empty cluster is valid, matches real ES `_cat` API behavior of returning an empty list rather than 404)
- First handler in the codebase that actually touches `state.store` — demonstrates the full read-lock-scope-drop pattern for Tutorial 07 to copy

**Suggestions for Tutorial 07 — Index Management Endpoints:**
- `PUT /{index}`, `DELETE /{index}`, `GET`/`HEAD /{index}`, `PUT`/`GET /{index}/_settings`, `GET /{index}/_mapping` all follow the write-lock/read-lock pattern established here
- This is the natural point for `IndexStore` methods to start returning `Result` (e.g. `create()` → `Err` if index already exists) — surfaced as `AppError::ResourceAlreadyExists`/`AppError::IndexNotFound` (Tutorial 05), which currently have zero call sites
- Reuse the `{ }`-scoped guard pattern from `cat_indices_handler` for every new handler; do not hold a guard across the handler's return statement or any `.await`
- `IndexInfo::from_metadata` (Tutorial 03) is already wired for `_cat/indices` — likely reusable as-is for `GET /{index}` responses, check before adding a new conversion

---

### Tutorial 07 — Index Management Endpoints

**Written:** Phase 1, Task 6

**Concepts covered:**
- `Path<T>` extractor: single-segment `Path<String>`, multi-segment `Path<(String, String)>` tuple (preview for Tutorial 08's `/{index}/_doc/{id}`), struct-based `Path` extraction mentioned as the 3+-segment idiom
- Axum 0.8 route syntax uses `{index}` curly-brace segments (not `:index` — that was 0.6/0.7)
- `Query<T>` extractor with `#[serde(default)]` — why absent query keys 400 without it (no `Default` fallback baked into deserialization otherwise)
- `head()` routing; returning a bare `StatusCode` (not `(StatusCode, Json<T>)`) is what produces an empty body; contrasted with axum's auto-generated HEAD-from-GET (runs the GET handler, discards body) — explicit HEAD handler is cheaper, avoids serializing a body just to drop it
- Method chaining on one path: `.route("/{index}", get(a).put(b).delete(c).head(d))`
- `Option<Json<Value>>` extractor pattern — accepts either a JSON body or no body at all (needed for bodyless `PUT /{index}`)

**Library vs binary error separation (the core new pattern this tutorial establishes):**
- `IndexStoreError` defined in `es-index/src/error.rs`, `thiserror`-based, **zero axum/HTTP knowledge**:
  ```rust
  #[derive(Debug, Error, PartialEq, Eq)]
  pub enum IndexStoreError {
      #[error("index already exists: {0}")]
      AlreadyExists(String),
      #[error("index not found: {0}")]
      NotFound(String),
  }
  ```
  Exported via `es-index/src/lib.rs`: `mod error; pub use error::IndexStoreError;`
- `IndexStore::create/get/delete` now return `Result<_, IndexStoreError>`. `IndexStore::list()` deliberately stays `Vec<&IndexMetadata>` (not `Result`) — iterating a `HashMap` into a `Vec` cannot fail; wrapping it in `Result` would be a signature lie, same as a Python fn declaring it might raise when no branch does.
- Bridge in `es-mini/src/error.rs`: `impl From<es_index::IndexStoreError> for AppError` — maps `AlreadyExists` → `AppError::ResourceAlreadyExists`, `NotFound` → `AppError::IndexNotFound`. This is what makes `store.create(...)?` work directly inside a `Result<_, AppError>`-returning handler (Rust auto-applies `From` on `?`).
- Full chain: `IndexStoreError` (domain fact) → `AppError` (via `From`, adds HTTP status + ES error-type string, Tutorial 05) → `IntoResponse` (Tutorial 05, final JSON envelope). Three types, three concerns.
- `AppError::IndexNotFound` / `AppError::ResourceAlreadyExists` (defined Tutorial 05, zero call sites since) now have their first real call sites.

**Routes added (all in `es-mini`, wired to `IndexStore` through `Arc<RwLock<_>>`, Tutorial 06 pattern):**
- `PUT /{index}` → `create_index` → `{"acknowledged": true, "shards_acknowledged": true, "index": "<name>"}`
- `GET /{index}` → `get_index` → `{"<name>": {"aliases": {}, "mappings": {}, "settings": {"index": {...}}}}`
- `DELETE /{index}` → `delete_index` → `{"acknowledged": true}`
- `HEAD /{index}` → `head_index` → 200/404, empty body
- `GET /{index}/_mapping` → `get_mapping` → `{"<name>": {"mappings": {}}}`
- `GET /{index}/_settings` → `get_settings` → `{"<name>": {"settings": {"index": {...}}}}`
- `PUT /{index}/_settings` → `update_settings` → `{"acknowledged": true}` (index-exists check only; no mutable settings field exists on `IndexMetadata` yet to actually apply changes to — flagged as a deliberate stub, not wired to a real mutation)
- `PrettyQuery { #[serde(default)] pretty: bool }` struct defined (establishes the `Query<T>` pattern) but **not threaded through any handler body** — no test in this tutorial asserts on pretty-printed output; wire it into a handler the day one does

**Integration test setup established (new precedent for Tutorial 08+ to reuse):**
- `es-mini` gains `src/lib.rs` exposing `pub fn build_app(cluster_name: String) -> Router` — refactored out of `main.rs` so `tests/` (which can only see a crate's public *library* API, never `main.rs`) can build the real router
- `es-mini/tests/common/mod.rs` — shared test helper (`mod.rs` naming tells cargo "shared module, not a standalone test file")
- `es-mini/tests/index_management.rs` — first integration test file; `mod common;` at the top pulls in the shared helper
- Unit tests (`#[cfg(test)]` in-crate, same compilation unit, can see private items) vs. integration tests (`tests/` dir, separate crate per file, public-API-only view — same vantage point as an external HTTP client) — explained as the pytest-hitting-a-test-client vs. bare-`unittest`-calling-a-view-function distinction
- `#[tokio::test]` — wraps an async test body in its own tokio runtime; defaults to `flavor = "current_thread"` (differs from `#[tokio::main]`'s multi-thread default); plain `#[test]` cannot take an `async fn` at all (compile error)
- **`axum-test = "21"`** chosen over `reqwest` for HTTP-shaped integration tests — verified `axum-test` 21.0.0's own `Cargo.toml` pins `axum = "^0.8"` (0.8.9 exactly), matching this project's axum version. `axum-test::TestServer::new(router)` simulates requests against the `Router` in-process, no real socket/port — avoids parallel-test port collisions `reqwest` + a bound `TcpListener` would risk. `reqwest` deliberately NOT added; reserved for later phases needing a genuinely separate OS process (e.g. Phase 4/5 cross-engine compatibility tests against a real Java ES instance)
- `axum-test` dev-dependency added to `es-mini/Cargo.toml` under `[dev-dependencies]` along with `tokio = { workspace = true, features = ["macros"] }`
- Test assertion helpers used: `.assert_status_ok()`, `.assert_status_not_found()`, `.assert_status_bad_request()`, `.assert_json_contains(&json!({...}))` — partial-match JSON assertion, avoids brittle full-body equality checks
- Four integration tests written: `create_get_delete_happy_path`, `head_reflects_existence`, `duplicate_create_is_400`, `missing_index_operations_are_404`

**Versions verified in this tutorial:**
- `axum-test = "21"` → resolves to **21.0.0**, confirmed compatible with `axum 0.8.9` (already pinned since Tutorial 04) via its own manifest
- No new production dependencies added — `axum-test` and `tokio`'s `macros` feature are both `[dev-dependencies]` only

**Files created/modified in this tutorial:**
- `es-index/src/error.rs` — new: `IndexStoreError` enum, 2 variants
- `es-index/src/lib.rs` — `mod error; pub use error::IndexStoreError;` added; `IndexStore::create/get/delete` signatures changed to return `Result`
- `es-mini/src/error.rs` — `impl From<es_index::IndexStoreError> for AppError` added
- `es-mini/src/lib.rs` — new: `pub fn build_app(cluster_name: String) -> Router`, extracted from `main.rs`
- `es-mini/src/main.rs` — slimmed to call `build_app(...)` + `axum::serve(...)`
- `es-mini/src/handlers/index.rs` (or inline, per existing layout) — new: `create_index`, `get_index`, `delete_index`, `head_index`, `get_mapping`, `get_settings`, `update_settings`, `PrettyQuery`
- `es-mini/Cargo.toml` — `[dev-dependencies]` section added: `axum-test = "21"`, `tokio` with `macros` feature
- `es-mini/tests/common/mod.rs` — new: `test_server()` helper
- `es-mini/tests/index_management.rs` — new: 4 integration tests

**Suggestions for Tutorial 08 — Document CRUD Endpoints:**
- `build_app` / `tests/common/mod.rs` scaffolding from this tutorial is ready to reuse as-is — new test file `es-mini/tests/document_crud.rs` with `mod common;` pointing at the same helper, no changes needed to the helper itself
- Document endpoints will need a second in-memory store (documents per index) — decide whether that lives inside `IndexStore` (an index "owns" its documents) or as a sibling `DocumentStore` in `AppState`; either way it needs its own `thiserror` error enum in `es-index` following the exact `IndexStoreError` precedent (domain-only, no HTTP knowledge), with its own `From<...> for AppError` bridge
- Multi-segment `Path<(String, String)>` for `/{index}/_doc/{id}` — pattern already previewed in this tutorial's §1, ready to use directly
- Auto-ID generation (`POST /{index}/_doc` with no `{id}`) will need a UUID-like generator — not yet in the dependency tree; evaluate `uuid` crate vs. a simpler internal counter when that task starts
- `_version` counters per document — likely a `u64` field bumped on every write; decide storage shape (part of the document record vs. separate map) before wiring handlers

---

### Tutorial 08 — Document CRUD Endpoints

**Written:** Phase 1, Task 7

**`Document` struct (in `es-index`, do not redefine — extend if a new field is needed):**
```rust
pub struct Document {
    pub id: String,
    pub source: serde_json::Value,
    pub version: u64,
    pub seq_no: u64,
    pub primary_term: u64,
}
```
- `documents: HashMap<String, Document>` added directly to `IndexMetadata` (Option A chosen over a separate `DocumentStore` sibling struct — no independent lifecycle need justified the extra type/lookup)
- `IndexMetadata` also gained a private `next_seq_no: u64` counter (not `pub` — internal only)
- `IndexResult` enum: `Created`, `Updated`, `Deleted`, `NotFound`, `NoOp` — `.as_es_str()` maps to ES's `result` field strings (`"created"`, `"updated"`, `"deleted"`, `"not_found"`, `"noop"`)
- `DocumentOpResult { document: Document, result: IndexResult }` — the common return payload for every mutating doc op

**Methods added to `IndexMetadata`:**
- `document_put(id: String, source: Value) -> DocumentOpResult` — create-or-overwrite, infallible (always succeeds)
- `document_create(id: String, source: Value) -> Result<DocumentOpResult, IndexStoreError>` — fails `VersionConflict` if `id` exists
- `document_get(id: &str) -> Result<Document, IndexStoreError>`
- `document_delete(id: &str) -> Result<DocumentOpResult, IndexStoreError>`
- `document_update(id: &str, partial: Value) -> Result<DocumentOpResult, IndexStoreError>` — merges via `merge_json`, returns `IndexResult::NoOp` if source unchanged after merge

**`merge_json` helper (in `es-index`, top-level function, exported via `lib.rs`):**
```rust
pub fn merge_json(base: serde_json::Value, patch: serde_json::Value) -> serde_json::Value
```
- Shallow per-key overwrite, recurses only when both sides are `Value::Object` at a given key; arrays/scalars in `patch` fully replace `base`'s value (matches real ES `_update` semantics — no array merging)
- 5 unit tests: shallow overwrite, new-key addition, nested-object recursion, scalar/array replacement, no-op equality check

**`IndexStoreError` new variants (extends Tutorial 07's enum):**
- `DocumentNotFound(String)` — maps to existing `AppError::DocumentNotFound { index, id }` (404, `document_missing_exception`, defined Tutorial 05, first real call site here)
- `VersionConflict(String)` — maps to new `AppError::VersionConflict { index, id }` (409, `version_conflict_engine_exception`)

**Routes added (all in `es-mini`):**
- `PUT /{index}/_doc/{id}` → `put_doc` → create-or-overwrite, 201 on create / 200 on update
- `POST /{index}/_doc` → `post_doc_auto_id` → always creates, `_id` generated via `uuid::Uuid::new_v4().to_string()`
- `PUT /{index}/_create/{id}` → `create_doc` → 409 `version_conflict_engine_exception` if `id` already exists
- `GET /{index}/_doc/{id}` → `get_doc` → `{"_index","_id","_version","_seq_no","_primary_term","found","_source"}`
- `DELETE /{index}/_doc/{id}` → `delete_doc` → `{"..., "result":"deleted", ...}`
- `POST /{index}/_update/{id}` → `update_doc` → body `{"doc": {...}}`, merges into `_source`, `"result":"noop"` if unchanged

**uuid crate version:** `uuid = { version = "1", features = ["v4"] }` in root `[workspace.dependencies]`; `es-index` depends via `{ workspace = true }`. Only the `v4` feature is enabled (pure-random generation, no clock/node-id/hasher inputs needed — v1/v3/v5/v6/v7 need those and stay disabled). Marked with a `// ponytail:` comment noting real ES uses 20-char Base64-encoded random bytes, not UUIDv4 — different format, same purpose, acceptable for this MVP since no test asserts on ID shape.

**Response envelope structs (`es-mini`, local to handlers module):**
- `DocWriteResponse` — used by PUT/POST/_create/DELETE responses; fields `#[serde(rename = "_index")]` etc.
- `DocGetResponse` — used by GET; adds `found: bool` and `#[serde(rename = "_source")]`
- `Shards { total, successful, failed }` — `Shards::single_node()` → `{2, 1, 0}`, reused across all write responses (single-node MVP, always the same shard counts)
- Established pattern: `#[serde(rename = "...")]` per-field (not `rename_all`) because ES's underscore-prefixed metadata fields (`_index`, `_id`, etc.) don't follow one uniform transform relative to Rust naming — mixed with unprefixed fields (`found`, `result`) in the same struct

**`From<IndexStoreError> for AppError` gap noted:** `IndexStoreError::DocumentNotFound`/`VersionConflict` don't carry an index name (by design — zero HTTP/caller coupling per Tutorial 07's precedent). Handlers that need the index name in the error body construct `AppError::DocumentNotFound { index: index_name.clone(), id }` directly via `.map_err(|_| ...)` instead of relying on bare `?` + blanket `From`. The blanket `From` impl still exists and fills `index: String::new()` for any call site that doesn't override it — flagged as an accepted gap, not fixed, since every real call site in this tutorial does override it.

**Integration tests (`es-mini/tests/document_crud.rs`, reused `tests/common/mod.rs` from Tutorial 07 unchanged):**
- `create_put_get_update_delete_happy_path`
- `create_on_existing_id_is_409`
- `get_missing_document_is_404`
- `auto_id_post_then_get_roundtrip`
- `partial_update_merges_and_bumps_version_and_seq_no`

**Suggestions for Tutorial 09 — Bulk Ingest and NDJSON Parsing:**
- `document_put`/`document_create`/`document_delete`/`document_update` are ready to call per parsed bulk action as-is — no new per-document domain logic needed, only NDJSON parsing + per-item response aggregation
- `doc_write_response`-style envelope construction should be factored so both single-doc handlers and the new bulk handler can share it, rather than duplicating field mapping a third time
- `generate_id()` (uuid-based, defined this tutorial) is the same generator bulk `index`/`create` actions without an explicit `_id` need
- Mixed-action bulk body needs per-item error handling that does NOT short-circuit the whole batch — first place in the codebase where a `Result::Err` needs to become a data value in a response array instead of an early return via `?`; flag this contrast explicitly in Tutorial 09 since every prior tutorial has used `?`-and-bail as the only error-handling shape

---

### Tutorial 09 — Bulk Ingest and NDJSON Parsing

**Written:** Phase 1, Task 8

**Concepts covered:**
- NDJSON (newline-delimited JSON) wire format: alternating action/source lines, `delete` has no source line, trailing `\n` produces a phantom empty "line" that must be filtered
- `axum::extract::FromRequest<S>` trait — signature confirmed in axum 0.8.9: `type Rejection: IntoResponse;` + `fn from_request(req: Request<Body>, state: &S) -> impl Future<Output = Result<Self, Self::Rejection>> + Send`. Implementations write plain `async fn` — axum's RPITIT desugaring handles the `impl Future` return.
- `FromRequestParts<S>` (parts-only, e.g. `Path`, `Query` — Tutorial 07) vs `FromRequest<S>` (body-consuming, e.g. `Json`, `Bytes`, new `Ndjson`) — only one `FromRequest` extractor allowed per handler, must be last argument, since the body can only be read once
- `axum::body::Bytes` (re-export of `bytes::Bytes`) already implements `FromRequest<S>` — `Ndjson` delegates to it and reuses its `Rejection` type rather than inventing a new error type
- `body.split(|&b| b == b'\n')` — slice `split` via `Bytes`'s `Deref<Target = [u8]>`; `Bytes::copy_from_slice` per segment to detach from the original buffer's borrow (cheap, refcounted, not a memcpy)
- `serde_json::from_slice` vs `from_str` — parses directly from `&[u8]`, avoids a redundant UTF-8 validation pass that constructing a `String` first would require
- Action-line parsing via `serde_json::Value` (not a tagged enum) — the discriminant is the *key itself* (`"index"`/`"create"`/`"update"`/`"delete"`), which doesn't fit serde's internally/adjacently/externally-tagged enum representations (all assume a fixed field holding the tag, not the key varying)
- `.chunks(2)` vs `.windows(2)` — chunks = non-overlapping fixed-size groups (right for "every record is N lines" *when true*); windows = overlapping (right for "look at every adjacent pair"). Bulk's actual shape (delete = 1 line, everything else = 2) breaks the `.chunks(2)` assumption — the tutorial deliberately shows the naive `.chunks(2)` draft, explains why it misaligns after a `delete`, then replaces it with a manual `while cursor < lines.len()` loop that peeks the action type before deciding whether to consume a source line
- `.enumerate()` — used in the naive `.chunks(2)` draft to tag each group with its position for error messages/logging
- `.collect::<Vec<_>>()` — turbofish + `_` element-type inference, used in the `Ndjson` extractor's line-building pipeline
- `impl Trait` in argument position (`reason: impl Into<String>`) — sugar for a generic parameter; flagged as "recognize on sight, don't reach for reflexively" rather than a pattern to imitate everywhere
- Partial failure semantics: bulk always returns `200`, never `207`; failures live entirely inside `items[]` (`status`, `error`) and the top-level `errors: bool` flag — first place in the codebase where a handler returns a bare `Json<T>` with no `Result<_, AppError>` wrapper despite operations failing, because there is no whole-request failure mode left once NDJSON splitting itself succeeds

**Types/functions defined in `es-mini` (do not redefine in later tutorials):**
- `Ndjson(pub Vec<Bytes>)` — custom `FromRequest` extractor, line-splitter only, no JSON parsing inside it
- `BulkActionType` enum: `Index`, `Create`, `Update`, `Delete` + `as_es_str()` 
- `BulkActionMeta { index: Option<String>, id: Option<String> }` — `#[serde(rename = "_index"/"_id")]`
- `parse_action_line(bytes: &[u8]) -> Result<(BulkActionType, BulkActionMeta), String>`
- `process_bulk(store: &mut IndexStore, lines: &[Bytes], default_index: Option<&str>) -> BulkResponse` — the cursor-loop core, one write-lock acquisition per whole batch
- `apply_bulk_action(...) -> BulkItemOutcome` — dispatches to the exact same `document_put`/`document_create`/`document_update`/`document_delete` methods from Tutorial 08; bulk introduces zero new storage logic
- `BulkItemBody`, `BulkItemError`, `BulkItemOutcome`, `BulkResponse` — response envelope types; `BulkItemOutcome::into_value()` wraps a body under its action-type key (`{"index": {...}}` etc.) via `json!({...})`, not a typed enum-keyed wrapper struct

**Routes added:**
- `POST /_bulk` → `bulk_no_index` — every action line must carry its own `_index`
- `POST /{index}/_bulk` → `bulk_with_index` — `{index}` is the default; per-line `_index` still overrides

**Versions verified in this tutorial:**
- `bytes` resolves to **1.12.1** (`cargo search bytes`) — arrives transitively via axum, no explicit dependency line added anywhere
- `axum = "0.8.9"` confirmed still current (matches Tutorials 04–08's pinned version)
- Confirmed `FromRequest<S, M = ViaRequest>` trait signature and `Rejection: IntoResponse` bound directly from docs.rs for axum 0.8.9 — no `BoxError`/`Infallible`-mandated rejection type; each extractor picks its own, `Ndjson` reuses `Bytes`'s

**Iterator patterns to reference in later tutorials:**
- The "naive iterator adapter (`.chunks`) → breaks on non-uniform data → manual cursor loop with lookahead" narrative (§9 of Tutorial 09) is the template for Tutorial 10's tokenizer, which will face the same "peek before deciding how much to consume" shape at grapheme/word boundaries
- "Go through `serde_json::Value` first when the JSON shape is keyed by a variable field name" (§8) — reusable any time a discriminant is a JSON *key* rather than a fixed tag *value*

**Suggestions for Tutorial 10 — Text Analysis: Iterators, Traits, and Unicode:**
- Reference Tutorial 09's cursor-loop-over-naive-chunks narrative directly rather than re-deriving the "iterator adapter looked right but the data wasn't uniform" lesson from scratch
- No bulk-related types need touching — `_bulk` doesn't route through analysis in the MVP; the standard/keyword analyzers are a clean, independent addition
- This is the first tutorial in the series to need manually-implemented traits (not just `#[derive(...)]`) — a good moment to also revisit `FromRequest`'s manual-impl shape (Tutorial 09 §5–6) as a "you've already done this once" reference point before introducing a from-scratch domain trait

---

### Tutorial 10 — Text Analysis: Iterators, Traits, and Unicode

**Written:** Phase 2, Task 9

**Concepts covered:**
- First manually-implemented trait in the codebase (everything prior was `#[derive(...)]`) —
  trait vs TypeScript `interface` (structural) vs Python `ABC` (nominal, runtime-enforced)
  comparison table; Rust traits are nominal + compile-time-enforced + can be implemented for
  foreign types (orphan rule permitting)
- `dyn Trait` / trait objects, `Box<dyn Analyzer>`, object safety (briefly: no `Self`-by-value,
  no `Self` return, no generic methods)
- Generics vs `dyn Trait` decision table — generics for static dispatch/perf, `dyn Trait` for
  heterogeneous runtime-varying collections (`AnalyzerRegistry`'s `HashMap` is the motivating case)
- `impl Trait` return position vs `Box<dyn Trait>` return position — `impl Trait` requires a
  single concrete return type across all paths; `make_analyzer`-style branching factories need
  `Box<dyn Trait>`
- Default trait methods (`fn name(&self) -> &str { "unnamed" }`)
- `Iterator` trait from first principles: `type Item` (associated type) + `fn next(&mut self) ->
  Option<Self::Item>`; custom `TokenStream` implementing it directly (teaching example, not used
  in the real analyzer)
- Iterator adapter chain (`.map`, `.filter`, `.flat_map`, `.chain`, `.take`, `.skip`,
  `.enumerate`, `.peekable`) — lazy, no intermediate `Vec` until `.collect()`
- `Peekable<I>` / `.peek()` — explicitly tied back to Tutorial 09's `.chunks(2)` failure mode
  (needed to look ahead before deciding how much to consume); flagged as the general tokenizer/
  parser look-ahead tool, relevant again for Tutorial 11/12 parsers
- `char` vs bytes: Rust `&str`/`String` are UTF-8 byte buffers; `str` has no `Index<usize>` at
  all (compile error, not a runtime footgun); `.chars()` iterates Unicode scalar values (`char` is
  4 bytes in memory); `.char_indices()` gives `(byte_offset, char)`; byte offsets (not char
  counts) are required for slicing back into a `&str`

**`Analyzer` trait definition (in `es-index/src/analysis/mod.rs`, do not redefine — extend with
new analyzers by adding new `impl` blocks, never by editing this trait or existing impls unless
the interface itself needs to change):**
```rust
pub trait Analyzer {
    fn analyze(&self, text: &str) -> Vec<String>;

    fn name(&self) -> &str {
        "unnamed"
    }
}
```

**Implementations (both in `es-index/src/analysis/`):**
- `KeywordAnalyzer` — identity, `vec![text.to_lowercase()]`, `name() == "keyword"`. Empty input →
  one token `""` (matches real ES keyword-analyzer behavior, no filtering).
- `StandardAnalyzer` — `text.unicode_words().map(str::to_lowercase).collect()`, `name() ==
  "standard"`. Built on `unicode_words()` (not `split_word_bounds()` — the latter includes
  whitespace/punctuation segments; `unicode_words()` is pre-filtered to alphanumeric-containing
  segments only, matching what feeds Lucene's lowercase filter).

**unicode-segmentation version:** `1.13.3` (current on crates.io at authoring time). Added as
`unicode-segmentation = "1.13"` in root `[workspace.dependencies]`, `{ workspace = true }` in
`es-index/Cargo.toml`.

**Known divergences from Lucene's `StandardTokenizer` (documented via `// ponytail:` comment in
`es-index/src/analysis/mod.rs`, not fixed):**
1. Lucene's `StandardTokenizer` (JFlex-grammar-based) special-cases URL/email tokens as single
   units in some configurations; `unicode-segmentation` has no such special-casing and will split
   on embedded punctuation (`@`, `.`, `/`).
2. Numeric tokens with embedded commas/periods (e.g. `"3,000.50"`) may segment differently at
   locale-sensitive punctuation depending on which side's Unicode table version is pinned (ICU
   behind Lucene vs `unicode-segmentation`'s own tables).
- No actual byte-mismatch has been observed/tested against a real Lucene reference corpus yet —
  these are the commonly-cited limitation categories for UAX#29-only tokenizers relative to a
  grammar-based one. **Tutorial 11 or a later YAML-test pass should verify against real Lucene
  output and update this list if concrete mismatches are found** — treat as an open verification
  item, not a closed one.
- CJK note (not a divergence, a shared limitation): `unicode_words()` treats an unbroken run of
  CJK ideographs as one segment — no dictionary-based word breaking on either side by default;
  both defer that to dedicated tokenizers (`icu_tokenizer`/`kuromoji`) out of scope for this MVP.

**`AnalyzerRegistry` (in `es-index/src/analysis/registry.rs`, do not redefine — add new analyzers
inside `new()`'s insertions only):**
```rust
pub struct AnalyzerRegistry {
    analyzers: HashMap<String, Box<dyn Analyzer + Send + Sync>>,
}
impl AnalyzerRegistry {
    pub fn new() -> Self { /* inserts "keyword" -> KeywordAnalyzer, "standard" -> StandardAnalyzer */ }
    pub fn get(&self, name: &str) -> Option<&(dyn Analyzer + Send + Sync)> { ... }
}
impl Default for AnalyzerRegistry { fn default() -> Self { Self::new() } }
```
- `+ Send + Sync` bound required because this goes into `AppState` behind `Arc` and axum runs
  handlers across worker threads — flagged explicitly since it's the first `dyn Trait` field in
  shared state; the compiler error if omitted is "`dyn Analyzer` cannot be sent between threads
  safely."
- `AppState` gains `analyzers: Arc<AnalyzerRegistry>` (no `RwLock` — registry never mutates post-
  construction, same reasoning as any other read-only shared config since Tutorial 06).

**Trait object pattern established, for Tutorial 11 to build on directly:**
- `Box<dyn Trait + Send + Sync>` stored in a `HashMap`, wrapped in `Arc` at the `AppState` level,
  is now the established shape for "a registry of pluggable, runtime-selected behavior." Tutorial
  11's query execution (different `Query` variants: `match`, `term`, `bool`, `range`) is a
  candidate for the same shape if it needs open extensibility — evaluate whether a closed `enum
  Query` with exhaustive `match` (cheaper, no `dyn` needed since the DSL variants are fixed by the
  ES spec, not user-extensible) is preferable; unlike analyzers, query types are unlikely to grow
  from third parties, so an enum may be the better fit there. Don't reflexively reuse `dyn Trait`
  just because it was introduced here.
- `filter_tokens()` iterator-adapter-chain helper (`map` → `filter` empty → `filter`
  non-alphanumeric-only → `collect`) exists in `es-index` for future analyzers built on
  `split_word_bounds()` (finer splitting than `unicode_words()`); `StandardAnalyzer` itself doesn't
  call it (`unicode_words()` already excludes punctuation-only segments).

**Files created/modified in this tutorial:**
- `es-index/Cargo.toml` — `unicode-segmentation = { workspace = true }` added
- `Cargo.toml` (root) — `unicode-segmentation = "1.13"` added to `[workspace.dependencies]`
- `es-index/src/analysis/mod.rs` — new: `Analyzer` trait, `KeywordAnalyzer`, `StandardAnalyzer`,
  `filter_tokens()`, table-driven tests (`standard_analyzer_corpus`,
  `keyword_analyzer_is_identity_lowercased`)
- `es-index/src/analysis/registry.rs` — new: `AnalyzerRegistry`
- `es-index/src/lib.rs` — `mod analysis; pub use analysis::{Analyzer, AnalyzerRegistry, KeywordAnalyzer, StandardAnalyzer};` added
- `es-mini/src/main.rs` — `AppState` gains `analyzers: Arc<AnalyzerRegistry>`, constructed once in
  `main()` alongside `store`

**Suggestions for Tutorial 11 — In-Memory Inverted Index and JSON Query DSL:**
- `AnalyzerRegistry` is ready to call from document-indexing code as-is — when a `text`-typed
  field (per `FieldType::Text`, Tutorial 03) is indexed, look up its analyzer (default
  `"standard"`, or `"keyword"` for `FieldType::Keyword`) via `state.analyzers.get(name)` and run
  `.analyze()` to produce terms for the inverted index
- No analysis integration exists yet in document PUT/POST handlers (Tutorial 07/08) — Tutorial 11
  is the first place `analyze()` gets called outside a unit test; decide there whether analysis
  happens eagerly at document-write time (building the inverted index incrementally) or lazily at
  query time (re-analyzing on every search) — eager is almost certainly correct for an inverted
  index's whole purpose, call this out explicitly rather than assuming it's obvious
- Consider whether `enum Query` (closed, exhaustive `match`) is a better fit than `dyn Trait` for
  query-type dispatch — unlike analyzers, the ES JSON DSL query types are fixed by spec, not
  user-extensible, so the trait-object pattern from this tutorial may not be the right reflex to
  reach for there
- The corpus test in this tutorial is a starting set, not exhaustive — extend
  `standard_analyzer_corpus` with real Lucene reference output if/when available, and resolve the
  "open verification item" flagged above about actual byte-exact divergence from Java ES

---

### Tutorial 11 — In-Memory Inverted Index and JSON Query DSL

**Written:** Phase 2, Task 10

**`InvertedIndex` (in `es-query/src/index.rs`, do not redefine — extend methods only):**
```rust
pub type DocId = String;

pub struct InvertedIndex {
    postings: HashMap<String, Vec<DocId>>, // term -> doc IDs, one push per occurrence (enables term_frequency)
}
impl InvertedIndex {
    pub fn new() -> Self;
    pub fn index_document(&mut self, doc_id: &str, _field: &str, terms: &[String]);
    pub fn lookup_term(&self, term: &str) -> &[DocId];
    pub fn matching_docs(&self, term: &str) -> HashSet<DocId>;
    pub fn term_frequency(&self, term: &str, doc_id: &str) -> usize;
}
```
- Single flat postings table, no per-field key — `// ponytail:` flagged as a known gap (cross-field
  term bleed-through not yet exercised by any test; fix would be a `(field, term)` composite key).
- Lives on `IndexMetadata` as `pub inverted_index: InvertedIndex` (Tutorial 08's `documents`
  precedent — no separate `DocumentStore`-style sibling struct, no independent lifecycle).
- Populated **eagerly at write time** via `IndexMetadata::reindex_document(&AnalyzerRegistry, doc_id, &Value)`
  — called from every existing write path (`document_put`/`document_create`/`document_update`,
  Tutorial 08; every bulk action, Tutorial 09). Text fields → `"standard"` analyzer, keyword
  fields → `"keyword"` analyzer (Tutorial 10's `AnalyzerRegistry`, unchanged). Numeric/boolean/date
  fields are not term-indexed in this tutorial's scope.
- **Known gap, not fixed:** `document_delete` does not remove postings — deleted docs are filtered
  out at query time by checking `documents.contains_key(doc_id)`... actually current `execute()`
  filters against `index.documents.values()` directly so deleted docs are naturally excluded from
  results even though stale postings remain; the postings table itself just grows unboundedly.
  `// ponytail:` flagged, compacting removal deferred until it's a measured problem.

**`Query` enum (in `es-query/src/query.rs`, do not redefine — this is now the closed, spec-fixed DSL surface):**
```rust
pub enum Query {
    MatchAll,
    Match { field: String, query: String },
    Term { field: String, value: serde_json::Value },
    Range { field: String, gte: Option<Value>, lte: Option<Value>, gt: Option<Value>, lt: Option<Value> },
    Bool { must: Vec<Query>, should: Vec<Query>, must_not: Vec<Query>, filter: Vec<Query> },
}
```
- Deliberately an enum, not a trait — contrasted explicitly with Tutorial 10's `Analyzer` trait
  decision: query types are spec-fixed (not third-party-extensible), so exhaustive `match` beats
  `dyn Trait` here. Tutorial 10's own "suggestions" section had already flagged this as the
  question to resolve; resolved in favor of the enum.
- `Bool { must: Vec<Query>, ... }` recurses with **no `Box<Query>`** — `Vec<T>`'s own size never
  depends on `T`, so the indirection needed to break the infinite-size cycle is already present.
  Only a bare (non-`Vec`/`Box`) recursive field would need explicit `Box<Query>`.

**Parsing (`es-query/src/parse.rs`):**
- `parse_query(value: &serde_json::Value) -> Result<Query, QueryError>` — manual recursive-descent
  over `serde_json::Value`, NOT `#[derive(Deserialize)]` with a tag attribute. Same "discriminant
  is a JSON *key*, not a fixed tag field" shape Tutorial 09 hit for bulk actions — noted as the
  same lesson recurring.
- Rejects empty query objects and multi-key query objects (`QueryError::Malformed`); rejects
  unknown query-type keys (`QueryError::UnsupportedQueryType`).
- `QueryError` — new `thiserror` enum in `es-query`, zero HTTP/axum knowledge (Tutorial 07's
  `IndexStoreError` precedent). Bridged via `impl From<es_query::QueryError> for AppError` in
  `es-mini/src/error.rs`, mapping to `AppError::InvalidRequest` (400,
  `action_request_validation_exception`).

**Execution (`es-query/src/execute.rs`):**
- `execute(query: &Query, index: &IndexMetadata) -> Vec<ScoredDoc>` where
  `ScoredDoc<'a> { doc: &'a Document, score: f64 }`.
- `Match`: analyzes query text with the field's analyzer, unions doc IDs across all resulting
  terms, sums term-frequency-based scores.
- `Term`: exact lookup, no analysis, always scores `1.0` (matches real ES's filter-context default
  for term queries).
- `Bool`: `must`/`filter` intersect (via `HashSet::intersection`); `should` unions and — only when
  `must`/`filter` are both empty — becomes a required clause (mirrors real ES's should-becomes-
  required rule); `must_not` subtracts. Score = sum of `must`+`should` clause scores, floored at `1.0`.
- `Range`: full document scan + `partial_cmp`-based numeric comparison — `// ponytail:` flagged as
  O(n), no BKD/range index yet (that's Lucene's job in Phase 4).
- `MatchAll`: returns every document, score `1.0`.

**Scoring approach chosen:** raw term frequency (`InvertedIndex::term_frequency`), no IDF, no
length normalization. `// ponytail:` comment names BM25 as the correct upgrade and the concrete
trigger ("once a test asserts on relative `_score` ordering across differently-sized corpora").

**Routes added (all in `es-mini`):**
- `GET`/`POST /{index}/_search` → same handler (`search`) for both verbs; `GET` takes an optional
  JSON body (`Option<Json<SearchRequest>>`, Tutorial 07's bodyless-`PUT` pattern reused), defaults
  to `{"query": {"match_all": {}}}` when absent.
- `POST /{index}/_count` → reuses `execute()` verbatim, returns `{"count": N, "_shards": {...}}`.

**`SearchRequest`/`SearchResponse` shapes (`es-mini/src/search.rs`):**
- `SearchRequest { query: Value, from: usize (default 0), size: usize (default 10), sort: Vec<Value> }`
- `SearchResponse { took, timed_out, #[serde(rename="_shards")] shards: Shards, hits: HitsEnvelope }`
- `HitsEnvelope { total: HitsTotal { value: usize, relation: String }, max_score: Option<f64>, hits: Vec<Hit> }`
- `Hit { #[serde(rename="_index")] index, #[serde(rename="_id")] id, #[serde(rename="_score")] score: Option<f64>, #[serde(rename="_source")] source: Value }`
- Verified against real ES: `hits.total` is the modern `{value, relation}` object form (not a bare
  integer — that's the pre-7.x legacy shape), `relation` is always `"eq"` here (no approximate
  counting), `_count` response is `{"count": N, "_shards": {...}}`.
- `Shards` struct reused as-is from Tutorial 08 (`Shards::single_node()`).

**Pagination:** `from`/`size` applied via `Vec::drain(0..from)` + `truncate(size)` after
sort — `// ponytail:` flagged as O(n) per page, same documented deep-pagination cost real ES's
own `from`/`size` API has (not an es-mini-specific regression); `search_after` noted as the real
ES fix, out of scope.

**Sort:** default (`_score` descending) uses `sort_unstable_by` — no secondary key to preserve, so
the faster/no-extra-allocation variant is preferred. Multi-key explicit `sort: [...]` requests use
`sort_by` (stable) for every clause, applied in reverse order so the first clause in the request
ends up primary — explicitly contrasted: `sort_unstable_by` would silently corrupt secondary-key
ordering on ties. `partial_cmp(...).unwrap_or(Ordering::Equal)` used throughout for `f64` compares
(floats aren't `Ord` — no total order because of `NaN`).

**Rust concepts newly introduced this tutorial:**
- `HashSet<T>` + `.intersection()`/`.union()`/`.difference()` (return iterators, need
  `.copied()`/`.cloned()` + `.collect()`) — the `Bool` query's actual implementation mechanism.
- `Vec::dedup()` — mentioned and explicitly NOT used (`dedup()` only removes *consecutive*
  duplicates; building a `HashSet` from the start is simpler and doesn't require a prior sort).
- `f64`/`PartialOrd` vs `Ord`, `partial_cmp`, `NaN` handling — floats can't implement `Ord` (no
  total order); `.unwrap_or(Ordering::Equal)` is the standard fallback.
- `sort_unstable_by` vs `sort_by` — confirmed current guidance: prefer `sort_unstable_by` (faster,
  no extra allocation, pattern-defeating quicksort family) whenever tie order doesn't matter; drop
  to stable `sort_by`/`sort` only when a secondary sort key (or any other tie-breaking guarantee)
  needs preserving.
- Recursive enums without `Box`: `Vec<T>`'s size never depends on `T`, so `Query::Bool { must:
  Vec<Query>, ... }` compiles directly — no `Box<Query>` needed, unlike a bare recursive field.

**Files created/modified in this tutorial:**
- `es-query/src/index.rs` — new: `InvertedIndex`, `DocId` type alias
- `es-query/src/query.rs` — new: `Query` enum
- `es-query/src/parse.rs` — new: `parse_query`, per-variant parse helpers
- `es-query/src/execute.rs` — new: `execute`, `ScoredDoc`, per-variant execution helpers
- `es-query/src/error.rs` — new: `QueryError` (`thiserror`)
- `es-index/src/index_metadata.rs` — `IndexMetadata` gains `pub inverted_index: InvertedIndex`;
  new `reindex_document` method; existing write paths call it
- `es-mini/src/search.rs` — new: `SearchRequest`, `SearchResponse`, `HitsEnvelope`, `HitsTotal`,
  `Hit`, `CountResponse`
- `es-mini/src/handlers/search.rs` — new: `search`, `count` handlers, `apply_pagination`,
  `apply_sort`, `parse_sort_clause`, `compare_json`
- `es-mini/src/error.rs` — `impl From<es_query::QueryError> for AppError` added
- `es-mini/tests/search.rs` — new: 7 integration tests including `_count` vs `_search` parity

**Known performance limitations (ponytail items) carried forward:**
1. Single flat postings table, no per-field separation — possible cross-field match bleed-through
   (untested, no current corpus exercises it).
2. Deleted documents' postings are never removed — filtered at read time via `documents.values()`,
   but the postings table itself grows unboundedly.
3. `Range` query is a full O(n) document scan — no BKD/range index; deferred to the Lucene codec
   (Phase 4).
4. Scoring is raw term frequency only — no IDF, no length normalization (not BM25).
5. `from`/`size` pagination is O(n) per request (drain+truncate over an already-materialized
   `Vec`) — same class of cost real ES's own `from`/`size` has at deep pages; `search_after` is
   the documented ES-side fix, not implemented here.

**Suggestions for Tutorial 12 — Parser Combinators with nom: ES|QL Subset:**
- `parse_query`'s manual `serde_json::Value` walk is JSON-tree-specific — do not reuse as a
  template for ES|QL, which is a genuine text grammar needing `nom`.
- `Query` enum's "closed enum over `dyn Trait`" reasoning (§7) applies again to an ES|QL AST.
- `InvertedIndex::lookup_term`/`matching_docs` and the `Range` scan-and-compare logic are directly
  reusable for ES|QL's `WHERE` clause — no new storage-layer code needed.

---

### Tutorial 12 — Parser Combinators with nom: ES|QL Subset

**Written:** Phase 2, Task 11

**Library choice:** `nom` over `chumsky` — more widely used, battle-tested, "just Rust functions"
character matches project style. Trade-off accepted: nom's stock errors need `nom-language`'s
`VerboseError`/`convert_error` for anything HTTP-400-worthy; chumsky would have had this built in.

**nom version: `nom = "8"` → resolves to 8.0.0.** This is a real, verified breaking change from
nom 7 that any internet example/tutorial predates:
- nom 8 combinators return a value implementing the `Parser` trait; call `.parse(input)`, not
  `parser(input)` directly. `use nom::Parser;` required in scope.
- `nom::sequence::tuple(...)` is deprecated in nom 8 — tuples of parsers implement `Parser`
  directly: `(p1, p2).parse(input)` replaces `tuple((p1, p2))(input)`.
- `VerboseError`/`convert_error` **moved out of `nom` core** into a separate crate,
  `nom-language = "0.1"`, as of nom 8. `use nom::error::VerboseError` fails to compile against
  nom 8 with `error[E0432]: unresolved imports`.
- All three confirmed by actually building a throwaway crate against real `nom = "8"` /
  `nom-language = "0.1"` dependencies (not just doc-reading) — see tutorial §2/§13 for the exact
  compiler output.
- Everything else (`tag`, `tag_no_case`, `alpha1`, `digit1`, `alphanumeric1`, `multispace0/1`,
  `preceded`, `delimited`, `separated_list1`, `alt`, `opt`, `many0`, `all_consuming`, `recognize`)
  works the same shape in nom 8 as nom 7 — only the calling convention and the error-type location
  changed.

**ES|QL AST types defined (`es-query/src/esql/ast.rs`, do not redefine in later tutorials):**
```rust
pub struct EsqlQuery { pub source: SourceCommand, pub commands: Vec<EsqlCommand> }
pub struct SourceCommand { pub index: String }
pub enum EsqlCommand { Where(WhereExpr), Limit(u64), Keep(Vec<String>), Drop(Vec<String>), Sort(Vec<SortField>) }
pub struct WhereExpr { pub field: String, pub op: CompareOp, pub value: EsqlValue }
pub enum CompareOp { Eq, Ne, Gt, Lt, Gte, Lte }
pub enum EsqlValue { Str(String), Num(f64) }
pub struct SortField { pub field: String, pub descending: bool }
```
All `#[derive(Debug, Clone, PartialEq)]`. `EsqlCommand` is a closed enum, not `dyn Trait` — same
"spec-fixed vocabulary, not third-party-extensible" reasoning as Tutorial 11's `Query` enum.

**Parser functions (`es-query/src/esql/parser.rs`):** `parse_source`, `parse_where`,
`parse_limit`, `parse_keep`, `parse_drop`, `parse_sort`, `parse_command` (the `alt((...))` over
all five), `parse_esql` (full pipeline via `many0(parse_pipe_stage)`), `parse_esql_verbose`
(wraps `parse_esql` with `all_consuming` + `VerboseError` + `convert_error` → `Result<EsqlQuery,
String>`). `tag_no_case` used for every keyword (`FROM`/`WHERE`/`LIMIT`/`KEEP`/`DROP`/`SORT`/
`ASC`/`DESC`) — case-insensitive per real ES|QL; field names and string values stay
case-sensitive.

**Executor (`es-query/src/esql/execute.rs`):** `execute_esql(query: &EsqlQuery, index:
&IndexMetadata) -> Result<EsqlResponse, QueryError>`. Reuses Tutorial 11's `Document`/
`IndexMetadata` storage as-is — no new storage layer. `Where`/`Limit`/`Sort` filter the row set
in a loop over `query.commands`; `Keep`/`Drop` handled once afterward as a column-projection step
(deliberately decoupled from row filtering — flagged as an MVP simplification, real ES|QL lets
column changes affect later `WHERE` references, out of scope here). `compare()` for `WHERE`
reuses Tutorial 11's `partial_cmp`/NaN-handling numeric-compare shape verbatim.

**Response types:** `EsqlResponse`/`EsqlColumn` (in `es-query`) → `EsqlResponseBody`/
`EsqlColumnBody` (in `es-mini/src/esql.rs`, `#[serde(rename = "type")]` on the column-type field)
— columnar `{"columns": [{"name","type"}], "values": [[...]]}` shape, verified distinct from
`_search`'s row-per-hit envelope (Tutorial 11).

**Routes added:** `POST /_query` → `esql_query` handler in `es-mini/src/handlers/esql.rs`.
Request body `{"query": "<esql text>"}`. Parse failure → `AppError::InvalidRequest` (via a new
narrow `impl From<String> for AppError`, scoped only to the ES|QL parse-error call site — not a
blanket bridge). Unknown source index → `AppError::IndexNotFound` (existing variant, new call
site). Both reuse Tutorial 05's `IntoResponse` machinery unchanged.

**Unsupported-clause handling (EXECUTION.md hard requirement):** No explicit "reject these
keywords" list needed — `alt((...))` over exactly the five supported clause parsers fails on
anything else (e.g. `EVAL`), and `all_consuming` (`nom::combinator::all_consuming`) turns any
leftover/trailing input into a hard parse failure rather than a silent partial parse. Both
together give "fail loudly on anything unsupported" for free from the grammar structure itself.

**Lifetime concepts introduced (for Tutorial 14+ binary format work — read this before starting
Lucene codec reading):**
- `fn parse_x<'a>(input: &'a str) -> IResult<&'a str, &'a str>` — nom parsers over `&str` return
  slices *of that same input*; `'a` is the compiler-checked promise that the returned slice
  doesn't outlive the buffer it was sliced from.
- The AST-boundary rule established here: convert to an owned `String` (`.to_string()`) at the
  exact moment a parser produces its *final* value, and nowhere else. Storing a borrowed `&'a
  str` in an AST type (`EsqlQuery<'a>`) would force that lifetime parameter through every
  downstream function signature that touches the AST — avoided entirely by taking ownership once,
  at the parse boundary.
- Explicitly flagged as the same shape Tutorial 14 will hit reading Lucene's on-disk byte format:
  borrow while parsing/decoding a segment file's byte slice, own (copy into a `Vec<u8>`/`String`/
  typed struct) at the point a decoded value needs to outlive the buffer/mmap it came from.

**Files created/modified in this tutorial:**
- `Cargo.toml` (root) — `nom = "8"`, `nom-language = "0.1"` added to `[workspace.dependencies]`
- `es-query/Cargo.toml` — both added via `{ workspace = true }`
- `es-query/src/esql/ast.rs` — new: AST types listed above
- `es-query/src/esql/parser.rs` — new: all parser functions + unit tests (one success + one
  failure case per clause, plus full-pipeline and case-insensitivity tests)
- `es-query/src/esql/execute.rs` — new: `execute_esql`, `EsqlResponse`, `EsqlColumn`
- `es-query/src/lib.rs` — `pub mod esql;` added
- `es-mini/src/esql.rs` — new: `EsqlResponseBody`, `EsqlColumnBody`, `From<EsqlResponse>` impl
- `es-mini/src/handlers/esql.rs` — new: `EsqlRequest`, `esql_query` handler
- `es-mini/src/error.rs` — narrow `impl From<String> for AppError` added (ES|QL parse-error
  bridge only)
- `es-mini/tests/esql.rs` — new: happy-path pipeline test, parse-error (400) test,
  unknown-index (404) test

**Suggestions for Tutorial 13 — Integration Testing and YAML REST Test Harness:**
- `POST /_query` is a fourth "parse untrusted text/JSON → dispatch → respond" endpoint family
  (after index management, document CRUD, `_search`) — needs its own YAML `do:` stanza mapping
  distinct from `search`'s.
- The documented out-of-scope grammar list (`EVAL`, `STATS`, `RENAME`, `DISSECT`/`GROK`,
  multi-source `FROM`, boolean `AND`/`OR` in `WHERE`, decimal/negative number literals, string
  ordering comparisons) should seed the first entries in whatever YAML-test exclusion manifest
  Tutorial 13 builds — exclude by construction (grammar doesn't support it) rather than by a
  runtime skip flag.
- `all_consuming` + `VerboseError`/`convert_error` (nom-language) is now a reusable pattern for
  any future hand-written grammar in this codebase.

---

### Tutorial 13 — Integration Testing and YAML REST Test Harness

**Written:** Phase 3, Tasks 12-13

**Versions verified in this tutorial (via `cargo search`/`cargo info` against live crates.io):**
- `serde_yaml` confirmed **deprecated**: publishes as literal version string
  `0.9.34+deprecated`. Chosen replacement: **`serde_norway = "0.9"`** (resolves to 0.9.42) —
  maintained community fork, same API surface (`from_str`, `Value`, `from_slice`), drop-in
  rename. `serde_yaml_ng`, `yaml_serde`, `serde_yaml_bw` also exist as alternative forks — not
  chosen; `serde_norway` picked as the most commonly cited/actively-versioned successor.
- `reqwest = "0.13"` → resolves to **0.13.4**. Async by default (no `blocking` feature needed);
  `json` feature required explicitly (not in `default`) for `.json::<T>()` and `client.get(...)
  .json(&body)`. `rust-version: 1.85.0` per its own manifest — confirmed compatible with this
  project's toolchain (already past 1.85 since Tutorial 01/edition-2024 baseline).
- `xtask` confirmed **pure convention, no cargo-native support** — no `[xtask]` Cargo.toml
  section exists; crates named `xtask`/`xtask-wasm`/`tracel-xtask`/etc. on crates.io are all
  *libraries of reusable xtask-building helpers*, not cargo plugins. The `cargo xtask <name>`
  invocation is made to work entirely via a two-line `.cargo/config.toml` alias
  (`xtask = "run --package xtask --"`) plus an ordinary workspace member binary crate that
  dispatches on `env::args().nth(1)`.

**xtask structure established:**
- New workspace member: `xtask/` — binary crate, `publish = false`, depends on `serde_norway`,
  `serde_json`, `reqwest`, `tokio` (all `{ workspace = true }`, real deps not dev-deps — xtask IS
  the test tool, not a thing being tested)
- `xtask/src/main.rs` — plain sync `fn main()`, dispatches `env::args().nth(1)` to task
  functions (`"test-yaml" => yaml_runner::run()`); unknown/missing task → usage message + exit 1
- `.cargo/config.toml` (new file, root) — `[alias] xtask = "run --package xtask --"`
- `xtask/src/yaml_runner/{mod.rs, model.rs, path.rs, stash.rs, api_client.rs}` — the runner
  internals (§10 of the tutorial)
- Only one task implemented so far: `test-yaml`. Future tasks (e.g. a fixture-generation task
  for Phase 4/Tutorial 14) add new `match` arms in `main.rs`, not new crates.

**`Drop` trait introduced — first manual `Drop` impl in the codebase (for Tutorial 14 to
reference, since Phase 4's fixture/process-lifecycle work will likely need the same shape again,
e.g. tearing down a Docker container running real Java ES):**
```rust
struct ServerGuard { child: std::process::Child }
impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
```
- Analogy used: Python `with`/`__exit__` or `try/finally` — NOT `__del__` (explicitly contrasted;
  `__del__` timing/guarantee is weaker than Rust's scope-tied, panic-safe `Drop`)
- Key discipline: never call `.kill()` manually AND rely on `Drop` — pick one. This tutorial
  relies on `Drop` exclusively; the guard being in scope for the whole test run is the only
  cleanup mechanism.
- `.ok()`/`let _ =` on both `kill()` and `wait()` inside `drop()` — panicking inside a `Drop` impl
  during an existing unwind aborts the process instead of surfacing the original failure; this is
  the reason to swallow (not propagate) errors specifically inside `drop()`, contrasted with every
  other place in the codebase where swallowing an error would be flagged as wrong.
- `std::process::Child` — `.kill()`, `.wait()` (reaps the process, prevents a zombie/defunct
  entry), `.stdout`/`.stderr` (set via `Stdio::null()`/`Stdio::inherit()` at spawn time, not read
  from directly in this tutorial's runner).

**Process lifecycle pattern (for Tutorial 14+ if any future tutorial needs to spawn Docker/Java
ES or another external process):**
- Spawn via `std::process::Command::new(...).args([...]).env(...).stdout/stderr(...).spawn()`
- Wrap the returned `Child` in a `Drop`-implementing guard immediately — do not let a bare
  `Child` live unwrapped across any fallible code
- Poll a cheap, stateless readiness endpoint (`GET /` here) with exponential backoff
  (`tokio::time::sleep`, doubling, capped) rather than a fixed sleep or a tight busy-loop

**YAML test format summary (ES `rest-api-spec` shape, do not re-derive — reference this
section directly in any future tutorial touching `tests/yaml/`):**
- File = multiple `---`-delimited YAML documents; first doc keyed `setup` (optional) runs before
  every case in the file; optional `teardown` doc runs after every case; every other doc is
  `"case name": [steps...]`
- Step shapes: `do: { <action.name>: {...params...} }` (action name is a dotted string like
  `indices.create`, params include both path/query fields and a `body` sub-key mirroring the real
  REST body), `match`, `length`, `is_true`, `is_false` (dot-path assertions against the most
  recent `do:`'s JSON response), `set: { path: stash_key }` (capture), `skip: { version, reason }`
  (this single-ES-version project either always-skips or always-runs a given skip stanza, no
  actual range negotiation needed), `catch: <short-name>|/regex/` (expect the preceding `do:` to
  fail in a specific way; short names: `missing`=404, `conflict`=409, `bad_request`=400,
  `unauthorized`=401, `forbidden`=403, `request_timeout`=408)
- `$stash_key` string values anywhere in a later `do:`'s params get substituted from the stash
  before dispatch — recursive walk over the params `Value` tree, exact-match `"$key"` strings only
- YAML 1.1 gotcha carried over into `serde_norway::Value` unchanged from `serde_yaml`: bare
  `yes`/`no`/`on`/`off` parse as booleans, not strings

**Runner architecture (in `xtask`, not `es-mini`):**
- Parse each `.yml` to `serde_norway::Value` → convert to `serde_json::Value` once at the parse
  boundary (same "own at the parse boundary" discipline as Tutorial 12's ES|QL AST) → everything
  downstream (`resolve_path`, `Stash`, assertion comparisons) is plain `serde_json::Value`
- `resolve_path(value: &Value, path: &str) -> Option<&Value>` — dot-split + `.try_fold`,
  traverses `Value::Object`/`Value::Array` (numeric segment = array index), `None` on any
  mismatch/missing segment
- `Stash(HashMap<String, Value>)` — `.set()`, `.substitute()` (recursive tree walk replacing
  exact `"$key"` string leaves)
- `ApiClient` — one method per supported action name (`indices.create`, `index`, `get`, `delete`,
  `bulk`, `search`, …), each reads specific fields out of the untyped params `Value` and issues
  the matching `reqwest` call; unmatched action name → `panic!` (closed, exhaustive-by-
  construction mapping — same "spec-fixed vocabulary" reasoning as Tutorial 11's `Query` enum and
  Tutorial 12's `EsqlCommand` enum, applied to a match statement instead of an enum since the
  dispatch key is a runtime string, not a compile-time variant)
- Case execution loop: iterate `setup` steps then case's own steps in one combined sequence;
  track `last_response: Option<Value>` (most recent `do:`'s body) and a pending `expect_catch:
  Option<String>` (consumed by the very next `do:`); `Step::Skip` short-circuits the whole case
  immediately

**Xfail mechanism:**
- `tests/yaml/XFAIL.md` — plain text, one `<file>::<case name>` per line, `#` comments allowed
- Runner cross-references failures against this list: known-failure + actually fails → `XFAIL`
  (doesn't fail CI); known-failure + actually passes → `UNEXPECTED PASS` (**does** fail CI — the
  allowlist itself must be edited in the same change); not-listed + fails → plain `FAIL`
- Exit code: `0` iff `fail == 0 && unexpected_pass == 0` — CI gates on process exit code, not
  parsed console text

**Vendoring:**
- `tests/yaml/` mirrors upstream `rest-api-spec` relative paths (grouped by action name
  directory, e.g. `indices.create/10_basic.yml`), copied byte-verbatim from the pinned
  `COMPATIBILITY.md` ref
- `tests/yaml/INCLUDED.md` — manifest table of included files (+ which Phase/Task motivated
  inclusion) and excluded files (+ why); file-level granularity only — individual excluded *cases*
  within an included file are never deleted/edited, they go on `XFAIL.md` instead
- Selection heuristic: include a file if its action name(s) map onto an implemented `es-mini`
  endpoint family (`indices.*`, bare document CRUD actions, `bulk`, `search`, a hand-picked `esql`
  subset matching Tutorial 12's supported grammar); whole ES|QL upstream directory excluded since
  it exercises `EVAL`/`STATS`/multi-source `FROM` etc. that Tutorial 12's parser doesn't support —
  Tutorial 12's own documented out-of-scope grammar list seeded this exclusion directly

**Test organization clarified (extends Tutorial 07's precedent):**
- `tests/common/mod.rs` (Tutorial 07, in-process `axum-test` helper) and the new
  `tests/yaml_common/mod.rs` (this tutorial's subprocess/`ServerGuard` helper) are kept as two
  separate modules, not merged — different server-startup shape (in-process `Router` vs. real
  subprocess) doesn't share meaningful setup code
- Reaffirmed the `mod.rs` naming rule from Tutorial 07: any file directly under `tests/` (not in
  a subdirectory, or not named `mod.rs` within one) is compiled as its own independent test
  binary by cargo's auto-discovery — a shared-helper file must live at `tests/<name>/mod.rs`
  (or, equivalently since Rust 2018, `tests/<name>.rs` with no subdirectory) specifically to be
  *excluded* from that auto-discovery

**Files created/modified in this tutorial:**
- `Cargo.toml` (root) — `xtask` added to `[workspace] members`; `serde_norway = "0.9"` and
  `reqwest = { version = "0.13", features = ["json"] }` added to `[workspace.dependencies]`
- `.cargo/config.toml` — new: `[alias] xtask = "run --package xtask --"`
- `xtask/Cargo.toml` — new: `publish = false`, deps on `serde_norway`, `serde_json`, `reqwest`,
  `tokio` (workspace, real deps)
- `xtask/src/main.rs` — new: task dispatcher
- `xtask/src/yaml_runner/mod.rs` — new: `run()`, top-level suite loop, summary/exit-code logic
- `xtask/src/yaml_runner/model.rs` — new: `TestFile`, `TestCase`, `Step` enum
- `xtask/src/yaml_runner/path.rs` — new: `resolve_path`
- `xtask/src/yaml_runner/stash.rs` — new: `Stash`
- `xtask/src/yaml_runner/api_client.rs` — new: `ApiClient`, one method per action name
- `es-mini/tests/yaml_common/mod.rs` — new: `ServerGuard`, `wait_for_ready`, `spawn_server`
- `tests/yaml/` — new: vendored `.yml` files, `INCLUDED.md`, `XFAIL.md`
- `.github/workflows/ci.yml` — new step/job: `cargo xtask test-yaml`, gated on exit code

**Suggestions for Tutorial 14 — Reading Binary Formats: Lucene Codec:**
- `Drop`/`ServerGuard` pattern (this tutorial) is directly reusable if Tutorial 14/15's fixture
  generator needs to spawn and tear down a real Java ES Docker container (Phase 4 Task 15) —
  same "guarantee cleanup even on panic" requirement, same shape (wrap the `Child`/container
  handle, implement `Drop`, never call teardown manually alongside it)
- Tutorial 12's "borrow while parsing, own at the parse boundary" lifetime discipline (flagged
  there as directly relevant to Lucene byte-format reading) is the next concept in sequence —
  this tutorial didn't need new lifetime concepts (everything here is owned `String`/`Value`,
  short-lived subprocess/HTTP round trips) but Tutorial 14 will need it for real, reading
  `&[u8]` slices out of an mmap'd segment file
- `xtask` crate now exists and is a natural home for Phase 4's fixture-generation script
  (`EXECUTION.md` Task 15 mentions "script (in `fixtures/` or `xtask`)") — add a new
  `"gen-fixtures"` task alongside `"test-yaml"` in `xtask/src/main.rs` rather than inventing a
  second task-runner mechanism
- No `es-index`/`es-query`/`es-mini` production code changed in this tutorial — Tutorial 14 is
  free to assume the HTTP contract (Tutorials 04-12) is unchanged and now has a compatibility
  test harness watching it

---

### Tutorial 14 — Reading Binary Formats: Lucene 10 Codec

**Written:** Phase 4, Tasks 15-17

**All Lucene format facts verified against live source** (network access was available in this
session — `curl` against `raw.githubusercontent.com/apache/lucene`, tag
`releases/lucene/10.1.0`), not just recalled from general knowledge. Cite this tag if any future
tutorial needs to re-verify or extend format details.

**Critical correction to the original task brief:** Lucene is NOT big-endian throughout. Verified
split:
- `CodecUtil.writeBEInt`/`readBEInt` (big-endian) — used **only** for codec header magic/version
  and footer magic/algorithm-id/checksum.
- `DataOutput.writeInt`/`DataInput.readInt` (the ordinary, far-more-frequently-used one) — **little-
  endian**. Applies to `.si`'s doc count and version triple, `.fnm`'s `dvGen`, everything else.
- Tutorial 14 documents this explicitly as a "verified against source, common assumption is
  wrong" correction. Any later tutorial reading more Lucene formats (postings, doc values) should
  assume LE by default and only expect BE at a header/footer boundary.

**Binary I/O helpers defined (`es-codec/src/io.rs`, do not redefine):**
```rust
pub const CODEC_MAGIC: i32 = 0x3fd76c17;
pub fn check_header(r: &mut impl Read, expected_codec: &str, min_version: i32, max_version: i32) -> io::Result<i32>;
pub fn check_index_header(r: &mut impl Read, expected_codec: &str, min_version: i32, max_version: i32, expected_id: &[u8; 16], expected_suffix: &str) -> io::Result<i32>;
pub fn read_vint(r: &mut impl Read) -> io::Result<u32>;   // 5-byte max, u32
pub fn read_vlong(r: &mut impl Read) -> io::Result<u64>;  // 10-byte max, u64
pub fn read_string(r: &mut impl Read) -> io::Result<String>; // VInt len + UTF-8 bytes
// plus private read_i32_be/read_i32_le/read_i64_le, read_map_of_strings/read_set_of_strings
```
- VInt/VLong: low 7 bits per byte, high bit = continuation, little-endian group order (verified
  against `DataOutput.writeVInt`/`writeVLong`). Worked example in the tutorial: `300` encodes as
  `[0xAC, 0x02]`.
- Codec header: `BE magic (0x3fd76c17)` + `VInt-length-prefixed UTF-8 codec name` + `BE version
  int`. Index header adds: 16-byte object ID + 1-byte suffix length + ASCII suffix bytes. Footer:
  `BE ~magic` + `BE algorithm-id (always 0)` + `BE i64 CRC-32 checksum` (16 bytes total,
  `footerLength() == 16`).

**Lucene 10.1.0 codec identity confirmed (from `Lucene101Codec.java`):**
- Default codec: `Lucene101Codec` (postings format changed vs Lucene 9, `Lucene101PostingsFormat`
  — NOT read by this tutorial, deferred to Task 16/18)
- `SegmentInfoFormat`: `Lucene99SegmentInfoFormat`, `.si` file, codec name in header is literally
  `"Lucene90SegmentInfo"` (naming didn't change when the format got a new class name)
- `FieldInfosFormat`: `Lucene94FieldInfosFormat`, `.fnm` file, codec name `"Lucene94FieldInfos"`
- `StoredFieldsFormat`: `Lucene90StoredFieldsFormat`, default mode `BEST_SPEED` = LZ4 (8KB
  sub-blocks, ~80KB chunk flush threshold), codec name for BEST_SPEED's `.fdt` is
  `"Lucene90StoredFieldsFastData"`. `BEST_COMPRESSION` mode (DEFLATE, 48KB blocks) exists but is
  NOT implemented by this tutorial's reader — only LZ4/BEST_SPEED.
- `CompoundFormat`: `Lucene90CompoundFormat`. **Confirmed Lucene DOES use compound files
  (`.cfs`/`.cfe`) by default** — `MergePolicy.DEFAULT_NO_CFS_RATIO = 1.0` means small/fresh
  segments are compound by default. This tutorial's reader does NOT transparently unpack `.cfs` —
  fixture generation works around it by setting `index.merge.policy.no_cfs_ratio: 0` at
  index-creation time so the fixture's segments are plain per-file, not compounded. Real
  `.cfs` unpacking is an explicit Task 18 item.

**`.si` format (fully implemented, verified against `Lucene99SegmentInfoFormat.parseSegmentInfo`):**
`IndexHeader, SegVersion(3x LE i32), HasMinVersion(byte)[+3x LE i32 if 1], DocCount(LE i32),
IsCompoundFile(byte), HasBlocks(byte), Diagnostics(Map<String,String>), Files(Set<String>),
Attributes(Map<String,String>), NumSortFields(VInt)[unsupported if != 0], Footer`.

**`segments_N` format (partially implemented — enough to enumerate segment name+id, NOT the full
commit-point shape):** `IndexHeader(codec "segments"), LuceneVersion(3x VInt), IndexCreatedVersionMajor(VInt),
Counter(VLong), NumSegments(VInt), per-segment{Name(String), ID(16 bytes), Codec(String), ...more
fields deliberately skipped: del-gen/field-infos-gen/doc-values-gen/soft-deletes-field/sort-fields},
CommitUserData(Map), Footer`. Note VInt (not plain LE i32) is used for the Lucene version triple
here — different from `.si`'s version triple, which uses plain LE i32. Both verified independently
against source; don't assume one implies the other.

**`.fnm` format (partially implemented — enough for name→field-number lookup only):**
`IndexHeader(codec "Lucene94FieldInfos"), NumFields(VInt), per-field{Name(String),
FieldNumber(VInt), Bits(byte), IndexOptions(byte), DocValuesType(byte), DvGen(LE i64),
Attributes(Map), PointDataDimensionCount(VInt)[+2 more VInts if !=0], VectorDimension(VInt),
VectorEncoding(byte), VectorSimilarity(byte)}, Footer`. Byte cursor correctly walks past every
field even though only name+number are retained — important since misreading a field's length
misaligns every subsequent field.

**`.fdt`/`.fdx` stored fields (partially implemented, real LZ4 decode, several explicit scope
limits):**
- Chunk header (in `.fdt`): `docBase(VInt), token(VInt)` where `chunkDocs = token >> 2`,
  `sliced = token & 1`. If `chunkDocs == 1`: `numStoredFields[0](VInt), length[0](VInt)`. Else:
  two "packed ints" blocks (`StoredFieldsInts` format) for per-doc field-counts and raw lengths,
  prefix-summed into offsets.
- "Packed ints" sub-format: 1 byte `bpv` (0/8/16/32), then either one repeated VInt (`bpv==0`) or
  `count` fixed-width LE values. **Only implemented for `count < 128`** — the real format has an
  additional 128-value interleaved bit-transpose block path for larger chunks, not implemented
  (`// ponytail:` flagged, add if a fixture ever buffers >=128 docs into one chunk).
- Per-document field record (after LZ4 decompress + slicing to the doc's byte range):
  `infoAndBits(VLong)` where `fieldNumber = infoAndBits >> 3`, `type = infoAndBits & 0b111`
  (`0=STRING, 1=BYTE_ARR, 2=NUMERIC_INT, 3=NUMERIC_FLOAT, 4=NUMERIC_LONG, 5=NUMERIC_DOUBLE`,
  verified constants from `Lucene90CompressingStoredFieldsWriter`). ES's `_source` is stored as
  type `1` (`BYTE_ARR`) — raw UTF-8 JSON bytes, VInt length prefix, **no additional per-field
  compression** (the old ES per-field LZF layer was removed years ago; block-level LZ4 is the
  only compression).
- LZ4 block format: verified `org.apache.lucene.util.compress.LZ4`'s decompress algorithm is the
  **standard LZ4 block sequence format** (token byte, literal-length + escape-byte extension,
  2-byte LE match offset, `MIN_MATCH=4`) — no custom Lucene-specific LZ4 variant. This means the
  `lz4_flex` crate's `block::decompress` (given the known decompressed length) can decode it
  directly. New dependency: `lz4_flex = "0.11"` added to `es-codec/Cargo.toml` only (not a
  workspace dependency — no other crate needs LZ4).
- **Explicit scope limits (each has a `// ponytail:` comment at its call site in the tutorial's
  code, not just prose):** no `.fdx` binary-search index used (linear-scans chunk headers from
  file start instead — correctness doesn't depend on `.fdx`, it's a pure perf optimization); no
  `sliced` (>80KB single-doc, multi-LZ4-block) chunk support; no multi-chunk preset-dictionary
  carry-over (LZ4WithPresetDict — only a chunk's first block, with no preceding dictionary, is
  supported); no BEST_COMPRESSION/DEFLATE mode; no footer CRC-32 checksum verification anywhere
  in this tutorial's readers (footer position/shape trusted, not checksummed).

**`StorageBackend` trait definition (`es-index/src/storage.rs`, do not redefine):**
```rust
pub trait StorageBackend: Send + Sync {
    fn get_document(&self, index: &str, id: &str) -> Result<Option<Document>, AppError>;
    fn search(&self, index: &str, query: &Query) -> Result<Vec<Document>, AppError>;
}
```
- `InMemoryBackend` — wraps Tutorial 06's existing `IndexStore` behind the trait, behavior-
  preserving (every prior integration test must still pass unchanged).
- `DiskBackend` — wraps a `PathBuf` data dir, reads via `es-codec`. **Honest limitation stated
  explicitly in the tutorial: `_id` → internal Lucene doc-ID resolution is NOT implemented** —
  that lookup lives in the terms dictionary (`.tim`/`.tip`), a format this tutorial doesn't read
  (Task 16/18 scope). `DiskBackend::get_document`'s body is illustrative/wiring-only, not a
  working end-to-end disk read of `GET /{index}/_doc/{id}` by real `_id` string yet.
  `DiskBackend::search` is an explicit `Err(...)`/unimplemented stub.
- `AppState` gains `backend: Arc<dyn StorageBackend>` (same `Arc<dyn Trait>` shape as Tutorial
  10's `AnalyzerRegistry`, but `Arc` not `Box` here since `AppState` itself is `Clone`d per-request
  and `Box<dyn Trait>` isn't `Clone`). Selected at startup via `ES_MINI_DATA_DIR` env var
  (present → `DiskBackend`, absent → `InMemoryBackend`).

**`AppError::Io` variant added (extends Tutorial 05's enum):**
```rust
#[error("storage I/O error: {0}")]
Io(#[from] std::io::Error),
```
Maps to 500 / `internal_server_exception`. `es-codec` itself keeps every signature as plain
`io::Result<T>` — zero HTTP knowledge, same "library owns domain errors" split as
`IndexStoreError`. The `#[from]` wrap happens once, at the `es-mini` handler boundary that calls
into `DiskBackend`.

**`PathBuf`/`Path` pattern introduced:** `Path::new(...)`, `.join(...)`, `.with_extension(...)`,
`Path` (borrowed, like `&str`) vs `PathBuf` (owned, like `String`) — used throughout the fixture
generator and `DiskBackend` for constructing segment file paths (`data_dir.join(format!("{seg}.si"))`
style, mentioned but not spelled out in every snippet since the pattern is self-explanatory once
named).

**`xtask` extended (not a new crate — new task alongside Tutorial 13's `test-yaml`):**
- `xtask/src/fixtures.rs` — new: `run()`, `wait_for_healthy()`, `ingest_fixture_documents()`,
  `force_merge_and_flush()`
- `xtask/src/main.rs` — new match arm: `Some("gen-fixtures") => fixtures::run()`
- `reqwest` gains the `blocking` feature (was async-only per Tutorial 13) — `xtask` fixture
  generation is a linear, sync-shaped CLI script with no reason to run under `tokio`; async
  `reqwest` stays in use for `yaml_runner` unchanged, both features coexist on one `reqwest`
  workspace dependency line.
- Deterministic fixtures: `docker rm -f` + `remove_dir_all` unconditionally before every
  `gen-fixtures` run (errors from "didn't exist yet" discarded via `.ok()`/`let _ =`, same
  discipline as Tutorial 13's `Drop`-impl error swallowing, applied here via an explicit
  delete-first step instead of a `Drop` guard since there's no long-lived guard object in this
  linear function)
- `fixtures/data/` — committed to the repo (small, known corpus); `fixtures/data/README.md`
  records the exact `docker run` invocation + ES version for reproducibility
- Force-merge to 1 segment (`_forcemerge?max_num_segments=1`) + `no_cfs_ratio: 0` index setting
  at creation — keeps every fixture segment as plain per-file (not `.cfs` compound), and keeps
  `segments_N` walks in tests trivial (one segment per index)

**Files created/modified in this tutorial:**
- `es-codec/src/io.rs`, `segment_info.rs`, `segment_infos.rs`, `field_infos.rs`,
  `stored_fields.rs` — new, as detailed above
- `es-codec/src/lib.rs` — `mod` declarations + `pub use` re-exports for the above
- `es-codec/Cargo.toml` — `lz4_flex = "0.11"` added (crate-local, not workspace-level)
- `es-codec/tests/stored_fields.rs` — new: fixture-gated decode test (file-exists check + loud
  `eprintln!` skip, not `#[ignore]`)
- `xtask/src/fixtures.rs` — new
- `xtask/src/main.rs` — `gen-fixtures` match arm added
- `xtask/Cargo.toml` / root `Cargo.toml` — `reqwest`'s `blocking` feature added
- `es-index/src/storage.rs` — new: `StorageBackend` trait, `InMemoryBackend`, `DiskBackend`
- `es-mini/src/main.rs` — `AppState` gains `backend: Arc<dyn StorageBackend>`; `ES_MINI_DATA_DIR`
  env var read at startup to select backend
- `es-mini/src/error.rs` — `AppError::Io(#[from] std::io::Error)` added

**Suggestions for Tutorial 15 — Writing Binary Formats: Lucene Segment Writer (Phase 5, Tasks 19-21):**
- All the low-level I/O helpers (`check_header`/VInt/VLong/string) are read-only in this tutorial
  — Tutorial 15 needs the *write* side (`write_vint`, `write_header`, etc.), which is the exact
  mirror image byte-for-byte; reuse the same magic numbers/codec name constants, don't
  re-derive them
- `.tim`/`.tip` (postings) and `.dvd`/`.dvm` (doc values) were explicitly out of scope for both
  reading (this tutorial) and are NOT yet covered on the write side either — if Tutorial 15 needs
  a minimal round-trip (`EXECUTION.md` Task 19's DoD: "Rust writes, Rust reads back identical
  data"), it may be simplest to design the writer's minimum viable segment to only need stored
  fields + segment info + field infos (skip an inverted index on write entirely for the MVP
  round-trip, since search still runs against the in-memory `InvertedIndex` regardless of storage
  backend) — flag this scope question explicitly rather than assuming postings must be written
  from day one
- The compound-file (`.cfs`/`.cfe`) gap cuts both ways: Tutorial 15's writer should also default
  to **not** writing compound files (mirrors this tutorial's fixture-generation workaround) unless
  `.cfs` packing is explicitly built — simplest way to keep the reader (this tutorial) and writer
  (next tutorial) trivially compatible with each other without either side handling compound files
- `lz4_flex` (added this tutorial, read-only `block::decompress`) has a matching
  `block::compress`/`compress_prepend_size` — reuse the same crate for the writer's stored-fields
  compression rather than adding a second LZ4 dependency
- The `StorageBackend`/`DiskBackend` scaffold (this tutorial) is where Tutorial 15's writer
  ultimately plugs in — once segments can be written, `DiskBackend` gains actual write paths
  (`put_document` etc.), turning it from read-only into the real storage engine; no new trait
  needed, `StorageBackend` as defined here would grow write methods
- Real `.fdx` binary search (skipped this tutorial in favor of a linear scan) becomes worth
  building once the writer needs to *produce* a `.fdx` file too — natural moment to implement both
  read and write sides of `.fdx` together rather than adding read-side complexity in isolation

---

### Tutorial 15 — Writing Binary Formats: Lucene Segment Writer

**Written:** Phase 5, Tasks 19-21. **Final tutorial in the series — series marked complete below.**

**Corrections to the original task brief, both verified against Lucene source at
`releases/lucene/10.1.0`:**
1. **Footer magic is not `0x2DF046E`.** `CodecUtil.FOOTER_MAGIC = ~CODEC_MAGIC`, i.e.
   `~0x3fd76c17 = 0xc02893e8` (signed `i32`: `-1071082520`). Derive it as `!CODEC_MAGIC` in Rust
   (bitwise NOT), never hardcode a separate literal — it can't drift out of sync that way.
2. **`segments_N`'s `N` is base 36, not hex.** `Long.toString(gen, Character.MAX_RADIX)`,
   `MAX_RADIX == 36` (digits `0-9a-z`). First real commit is generation `1` (`0` is Lucene's
   internal "no commit yet" sentinel, never written as an actual `segments_0` file in practice).
3. **Stored fields chunk size**: task brief's `16384` isn't Lucene's real `BEST_SPEED` threshold
   — verified `BEST_SPEED_BLOCK_LENGTH = 10 * 8 * 1024 = 81920` bytes, `maxDocsPerChunk = 1024`.
   Sidestepped by design: the MVP writer always produces exactly one chunk per segment, so the
   exact threshold value doesn't matter for correctness (see below).
4. **Stored fields is 3 files, not 2.** `.fdt` (data) + `.fdm` (meta: codec header + chunkSize
   VInt + footer) + `.fdx` (skip index, real format is `DirectMonotonicWriter`-based). Tutorial
   14's reader skipped `.fdx` because correctness never depended on it (linear scan instead) —
   but a real Java `StoredFieldsReader` **requires** valid `.fdm`/`.fdx` to open a segment at all,
   whether or not random access is ever used. This tutorial's writer therefore writes a
   deliberately non-standard, simplified `.fdx` (fixed single-entry format: header + doc count +
   start offset + footer) valid only because the MVP writer never produces more than one chunk
   per segment — flagged as the single sharpest limitation in the tutorial, NOT wire-compatible
   with a real multi-chunk Lucene `.fdx`, only readable by this project's own (updated) reader.
5. **BEST_SPEED's real compressor is `LZ4WithPresetDictCompressionMode`**, not plain LZ4 — chunk
   N+1 uses chunk N as a dictionary. The single-chunk-per-segment MVP design sidesteps this
   entirely (empty dict on a segment's only chunk == plain LZ4), which is exactly why
   `lz4_flex::block::compress` (not `compress_prepend_size` — that adds a 4-byte length prefix
   Lucene's format has no room for, the length is already carried by an adjacent VInt) is correct
   for this MVP without implementing dictionary chaining.
6. **`segments_N` uses BE (`CodecUtil.writeBEInt`/`writeBELong`) for more fields than just
   header/footer** — verified from `SegmentInfos.write()`: `version` (commit counter), segment
   count, and per-segment `delGen`/`delCount`/`fieldInfosGen`/`docValuesGen`/`softDelCount` are
   all BE. This is a genuine, source-verified exception to Tutorial 14's "only header/footer are
   BE" rule — don't carry that simpler rule forward into new format work without checking.

**Versions verified (crates.io, live lookup at authoring time):**
- `lz4_flex = "0.11"` → current max stable is **0.14.0**. Reused the same crate Tutorial 14 added
  (crate-local to `es-codec`, not a workspace dependency) — `block::compress` (write side) sits
  alongside the already-in-use `block::decompress` (read side, Tutorial 14).
- `crc32fast = "1"` → resolves to **1.5.0**. New dependency, `es-codec` only.
- `tempfile = "3"` → resolves to **3.27.0**. New dependency, `es-codec` only (also usable in
  `xtask` for `verify-writes`'s temp data directory, via `tempfile::tempdir()`).
- No new workspace-level dependencies — both new crates (`crc32fast`, `tempfile`) are `es-codec`-
  local, same "don't promote to workspace until ≥2 crates need it" discipline as `lz4_flex`
  (Tutorial 14).

**Write-side helpers defined (`es-codec/src/io.rs`, extending Tutorial 14's read-only file — do
not redefine, add write functions alongside the existing read functions):**
```rust
pub fn write_i32_be/write_i32_le/write_i64_be/write_u32_be(w: &mut impl Write, v: _) -> io::Result<()>;
pub fn write_vint(w: &mut impl Write, value: u32) -> io::Result<()>;
pub fn write_vlong(w: &mut impl Write, value: u64) -> io::Result<()>;
pub fn write_string(w: &mut impl Write, s: &str) -> io::Result<()>;
pub fn write_index_header(w: &mut impl Write, codec: &str, version: i32, object_id: &[u8; 16], suffix: &str) -> io::Result<()>;
pub const FOOTER_MAGIC: i32 = !CODEC_MAGIC; // derived, not a separate literal
pub fn write_footer(w: &mut impl Write, crc: u32) -> io::Result<()>;
pub fn write_map_of_strings/write_set_of_strings(w: &mut impl Write, _: &BTreeMap<..>/&BTreeSet<..>) -> io::Result<()>;
```

**`ChecksumWriter<W: Write>` newtype (`es-codec/src/io.rs`, do not redefine):**
```rust
pub struct ChecksumWriter<W: Write> { inner: W, hasher: crc32fast::Hasher }
impl<W: Write> ChecksumWriter<W> {
    pub fn new(inner: W) -> Self;
    pub fn finish(self) -> (W, u32); // consumes self, returns inner writer + finalized CRC-32
}
impl<W: Write> Write for ChecksumWriter<W> { /* forwards write()/flush(), hashes only bytes actually written */ }
```
The newtype-wrapper-implementing-`Write` pattern — every existing `write_*` helper (all generic
over `impl Write`/`W: Write`) works unmodified underneath it, accumulating a running checksum as
a side effect. This is the tutorial's flagship Rust-idiom example (per the task brief's explicit
requirement) — analogous to Python's decorator pattern / a hashlib-updating file-object proxy.

**Atomic write helper (`es-codec/src/write_atomic.rs`, do not redefine):**
```rust
pub fn write_atomic(dir: &Path, final_name: &str, contents: impl FnOnce(&mut dyn Write) -> io::Result<()>) -> io::Result<()>;
```
Built on `tempfile::NamedTempFile::new_in(dir)` (same filesystem as `dir` — required for the
final `rename` to be atomic) + `.persist(dir.join(final_name))`. Every file write in `es-codec`
(`.si`, `.fdt`/`.fdm`/`.fdx`, `segments_N`) goes through this — no direct `File::create` on a
final filename anywhere in the write path (Definition of Done item 3 gates on a grep for this).

**Format functions defined (`es-codec`, do not redefine — mirror the existing read-side files
1:1, same module names with a `write_` prefix on the public function):**
- `write_si_file(dir, name, id, version, doc_count, diagnostics, files) -> io::Result<()>` — in
  `segment_info.rs`, alongside Tutorial 14's `read_segment_info`.
- `write_stored_fields(dir, name, id, sources: &[Vec<u8>]) -> io::Result<()>` — in
  `stored_fields.rs`, writes `.fdt`+`.fdm`+`.fdx` together (single-chunk MVP, §10/§11 of the
  tutorial). `sources` is one raw `_source` JSON blob per document; field number `0`, type `1`
  (`BYTE_ARR`) always — mirrors Tutorial 14's read side exactly (same type constants).
- `write_segments_file(dir, generation, counter, segments: &[SegmentCommitInfo], user_data) ->
  io::Result<()>` — in `segment_infos.rs`.
- `next_generation(dir: &Path) -> io::Result<u64>` — scans `segments_*` files, returns `1` if
  none exist, `max+1` otherwise; parses/produces the base-36 suffix via `u64::from_str_radix(s,
  36)` / a hand-rolled `to_base36(n)` (no crate needed for base-36 — `core`'s own integer
  formatting doesn't support arbitrary radixes above what `{:x}`/`{:o}`/`{:b}` give, so this is
  one of the rare places a few lines of manual code is the correct rung of the ladder rather than
  reaching for a crate).

**Scope decisions carried as `// ponytail:` comments in `es-codec` (see Part 5 of Tutorial 15
for the full numbered list; summarized here for the series-wide inventory below):**
1. No postings write (`.tim`/`.tip`) — disk-backed `_search` still not possible; in-memory
   `InvertedIndex` (Tutorial 11) remains the only query execution path regardless of backend.
2. No doc values write (`.dvd`/`.dvm`).
3. No points/BKD tree write — range queries against disk-only data would still need a full scan.
4. No compound file write (`.cfs`/`.cfe`) — mirrors Tutorial 14's fixture-generation workaround
   (`no_cfs_ratio: 0`) so reader and writer stay symmetric.
5. Single chunk per segment — no `LZ4WithPresetDict` chaining across chunks (`lz4_flex` supports
   `compress_with_dict`, not wired up). Upgrade trigger: ingest batches regularly exceeding
   ~1000 docs or ~80KB of `_source` per segment.
6. Simplified, non-standard `.fdx` (§11) — valid only for the single-chunk case; NOT the real
   `DirectMonotonicWriter` format, NOT readable by a real Java Lucene beyond this project's own
   reader. Sharpest limitation in the tutorial.
7. No merges — one segment per commit, forever, for the lifetime of an index.
8. No soft deletes / update-in-place on disk.
9. `.fnm` write is name/number only (matches Tutorial 14's read-side retention, nothing more).
10. `cargo xtask verify-writes` covers `GET /{index}/_doc/{id}` only — no Java-side `_search`
    against Rust-written segments (would need the postings write item #1 defers).

**`cargo xtask verify-writes` (`xtask/src/verify_writes.rs`, new match arm in `xtask/src/main.rs`
alongside Tutorial 13's `test-yaml` and Tutorial 14's `gen-fixtures` — third task, same
dispatcher, no new mechanism):**
- Sequence: temp data dir → spawn Rust `es-mini` server (Tutorial 13's `Drop`-guard subprocess
  pattern) → create index + ingest N docs via `reqwest::blocking` (Tutorial 14's fixture
  generator precedent for sync/blocking `reqwest` in `xtask`) → drop the Rust server guard →
  spawn Java ES via Docker on the *same* data directory (new `Drop`-guarded container handle,
  same pattern, second independent instance of it in the codebase) → HTTP query against Java ES
  → `anyhow::ensure!` assertions → both guards drop, tearing down process + container
  independently → print `PASS`/`FAIL`, non-zero exit on failure.
- Gated CI job (slower, per `EXECUTION.md` Task 20/21's explicit allowance), same precedent as
  Tutorial 14's YAML harness job.

**Task 21 (Ralph loop for write-compatibility) status:** explicitly scoped as future work, not
attempted by this tutorial. `verify-writes`'s single pass/fail assertion is the exact red/green
signal such a loop needs to iterate against — ready to hand off as-is, no further scaffolding
required before that loop could be built.

**Files created/modified in this tutorial:**
- `es-codec/src/io.rs` — write-side helpers added (VInt/VLong/string/BE/LE ints, header, footer,
  map/set-of-strings), `ChecksumWriter<W>` added
- `es-codec/src/write_atomic.rs` — new: `write_atomic`
- `es-codec/src/segment_info.rs` — `write_si_file` added alongside existing `read_segment_info`
- `es-codec/src/segment_infos.rs` — `write_segments_file`, `next_generation`, `to_base36` added
- `es-codec/src/stored_fields.rs` — `write_stored_fields` added (writes `.fdt`+`.fdm`+`.fdx`)
- `es-codec/Cargo.toml` — `crc32fast = "1"`, `tempfile = "3"` added (crate-local)
- `es-codec/tests/write_roundtrip.rs` — new: `si_roundtrip`, `stored_fields_roundtrip`,
  `full_segment_write_then_read_via_segments_file`
- `xtask/src/verify_writes.rs` — new
- `xtask/src/main.rs` — new match arm: `Some("verify-writes") => verify_writes::run()`
- `.github/workflows/ci.yml` — new gated job: `cargo xtask verify-writes`
- `COMPATIBILITY.md` — updated with a one-sentence definition of "write compatible" for this
  project (single-segment-per-commit, no merges, no compound files, stored fields + segment
  metadata only)

---

## Series status: COMPLETE

All 15 tutorials written. `EXECUTION.md` Tasks 1–20 have a corresponding tutorial; Task 21
(write-compatibility Ralph loop) is explicitly documented as future work at the end of Tutorial
15, not implemented by any tutorial in this series.

### Final crate inventory

| Crate | Kind | Role |
|---|---|---|
| `es-mini` | binary | HTTP server entrypoint, `AppState`, handlers, `AppError`/`IntoResponse` |
| `es-codec` | library | Lucene 10 on-disk read (Tutorial 14) + write (Tutorial 15): `io.rs` (VInt/VLong/string/header/footer/`ChecksumWriter`, both directions), `segment_info.rs` (`.si`), `segment_infos.rs` (`segments_N`), `field_infos.rs` (`.fnm`), `stored_fields.rs` (`.fdt`/`.fdm`/`.fdx`), `write_atomic.rs` |
| `es-query` | library | JSON DSL (`Query` enum, parser, executor), ES\|QL (`esql/` — AST, `nom` parser, executor), `InvertedIndex` |
| `es-index` | library | `IndexMetadata`/`Document`/`IndexStore`, mappings (`FieldType`), analysis (`Analyzer` trait, `KeywordAnalyzer`/`StandardAnalyzer`, `AnalyzerRegistry`), `StorageBackend` trait + `InMemoryBackend`/`DiskBackend` |
| `xtask` | binary (workspace tool) | `test-yaml` (Tutorial 13), `gen-fixtures` (Tutorial 14), `verify-writes` (Tutorial 15) |

### Complete dependency list (versions as verified at each tutorial's authoring time — re-check
before any further work, several of these will have moved on by the time this is read)

| Crate | Version pinned | Added in | Scope |
|---|---|---|---|
| `serde` | `1` (derive feature) | Tutorial 03 | workspace |
| `serde_json` | `1` | Tutorial 03 | workspace |
| `thiserror` | `2` → 2.0.19 | Tutorial 05 | workspace |
| `axum` | `0.8` → 0.8.9 | Tutorial 04 | workspace |
| `tokio` | `1` (full) → 1.53.1 | Tutorial 04 | workspace |
| `tracing` | `0.1` → 0.1.44 | Tutorial 04 | workspace |
| `tracing-subscriber` | `0.3` (env-filter, json) → 0.3.23 | Tutorial 04 | workspace |
| `uuid` | `1` (v4) | Tutorial 08 | workspace |
| `unicode-segmentation` | `1.13` → 1.13.3 | Tutorial 10 | `es-index` |
| `nom` | `8` → 8.0.0 | Tutorial 12 | `es-query` |
| `nom-language` | `0.1` | Tutorial 12 | `es-query` |
| `serde_norway` | `0.9` → 0.9.42 | Tutorial 13 | `xtask` |
| `reqwest` | `0.13` (json, blocking) → 0.13.4 | Tutorial 13 (async), Tutorial 14 (blocking) | `xtask` |
| `lz4_flex` | `0.11` → 0.14.0 current | Tutorial 14 (decompress), Tutorial 15 (compress) | `es-codec` |
| `crc32fast` | `1` → 1.5.0 | Tutorial 15 | `es-codec` |
| `tempfile` | `3` → 3.27.0 | Tutorial 15 | `es-codec` |
| `axum-test` | `21` → 21.0.0 | Tutorial 07 | dev-dep, `es-mini` |
| `anyhow` | mentioned (Tutorial 05), not added anywhere; `verify-writes` (Tutorial 15) is the first real call site | Tutorial 05 / 15 | `xtask` |

Dev/CI-only tooling (not Cargo dependencies): `dtolnay/rust-toolchain@stable`,
`Swatinem/rust-cache@v2`, `EmbarkStudios/cargo-deny-action@v2` (Tutorial 02).

### All `// ponytail:` deferred items across the series (consolidated)

- **Tutorial 08:** `uuid` v4 for `_id` generation instead of ES's real 20-char Base64 random-byte
  IDs — different format, same purpose, no test asserts on ID shape.
- **Tutorial 10:** `unicode-segmentation`'s UAX#29-only tokenizer vs Lucene's JFlex-grammar
  `StandardTokenizer` — documented divergence categories (URL/email special-casing, locale-
  sensitive numeric punctuation), not yet verified against a real byte-mismatch corpus.
- **Tutorial 11:** (1) flat postings table, no per-field key — cross-field term bleed-through
  untested; (2) deleted documents' postings never removed, table grows unboundedly; (3) `Range`
  query is O(n) full scan, no BKD/range index; (4) scoring is raw term frequency, no BM25/IDF;
  (5) `from`/`size` pagination is O(n) drain+truncate, no `search_after`.
- **Tutorial 14:** (1) no `.fdx` binary-search index on read (linear scan instead); (2) no
  `sliced` (>80KB single-doc) chunk support; (3) no multi-chunk LZ4 preset-dictionary carry-over;
  (4) no `BEST_COMPRESSION`/DEFLATE mode; (5) no footer CRC-32 verification on read; (6) `.cfs`
  compound files not transparently unpacked (fixture generation works around this); (7) packed-
  ints fast path only for `count < 128`; (8) `DiskBackend::get_document`'s `_id` → Lucene-doc-ID
  resolution not implemented (needs `.tim`/`.tip`, out of scope); (9) `DiskBackend::search` is an
  unimplemented stub.
- **Tutorial 15:** the ten items in its own Part 5 (postings write, doc values write, BKD write,
  compound file write, single-chunk-per-segment/no preset-dict chaining, simplified non-standard
  `.fdx`, no merges, no soft deletes/update-in-place, `.fnm` write is name/number only,
  `verify-writes` covers only `GET .../_doc/{id}` not `_search`).

### Where the series leaves off

A working, tested, HTTP-compatible, YAML-compatible, disk-read-and-write-compatible (at the
stored-fields + segment-metadata level) Elasticsearch clone. The largest remaining body of work —
postings, doc values, BKD trees, compound files, merges, and a Ralph loop closing out Task 21 —
is enumerated in Tutorial 15's "Where to go from here" section. No further tutorials are planned
in this series; any of those items would be a new, appropriately-scoped tutorial in its own right
if the project continues.
