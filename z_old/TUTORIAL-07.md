# Tutorial 07 — Index Management Endpoints

**Series:** es-mini — Building Elasticsearch in Rust
**Phase:** 1, Task 6
**Prerequisites:** Tutorials 01–06 complete — workspace builds, `es-index::IndexStore` exists (Tutorial 03), axum server with `AppState { cluster_name, store: Arc<RwLock<IndexStore>> }` (Tutorial 06), `AppError`/`IntoResponse` exists (Tutorial 05), `GET /_cat/indices` already reads through the lock (Tutorial 06).

---

By the end of this tutorial you will have:

- `PUT /{index}`, `GET /{index}`, `DELETE /{index}`, `HEAD /{index}`, `GET /{index}/_mapping`, `GET`/`PUT /{index}/_settings` — all wired to `IndexStore`
- `IndexStore` methods returning `Result` with a new library-level `IndexStoreError` in `es-index`, converted to `AppError` in `es-mini`
- Axum's `Path<T>` extractor, `Query<T>` extractor, `head()` routing, and method-chained routing on one path
- An integration test suite in `es-mini/tests/` hitting the real router over HTTP via `axum-test`

---

## 1. Route Parameters: `Path<T>`

Every index management endpoint is scoped to `{index}` — same shape as ES itself. Axum extracts path segments positionally via the `Path` extractor:

```rust
use axum::extract::Path;

async fn get_index(Path(index_name): Path<String>) -> impl IntoResponse {
    // index_name is an owned String, already URL-decoded
}
```

For a single `{param}` segment, `Path<String>` destructures directly. Multiple segments extract as a tuple:

```rust
// route: "/{index}/_doc/{id}"   (Tutorial 08 preview)
async fn get_doc(Path((index_name, id)): Path<(String, String)>) -> impl IntoResponse { }
```

Or, more readably for 3+ segments, a struct with `#[derive(Deserialize)]` — axum deserializes path segments into named fields the same way it deserializes JSON bodies. Not needed yet with only one segment; mentioned so the pattern isn't a surprise in Tutorial 08.

Analogy: this is Express's `req.params.index` or Flask's `<index>` URL rule — except the extraction is a type in the function signature instead of a runtime dict lookup, so a typo in the type (`Path<u32>` against a route matching `/my-index`) is a **routing-time 400**, not a silent `undefined`.

---

## 2. `Query<T>` — URL Query Parameters

ES endpoints accept `?pretty`, `?refresh`, etc. Axum's `Query<T>` extractor deserializes the query string into a struct via `serde`:

```rust
use axum::extract::Query;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct PrettyQuery {
    #[serde(default)]
    pretty: bool,
}

async fn get_index(
    Path(index_name): Path<String>,
    Query(params): Query<PrettyQuery>,
) -> impl IntoResponse {
    if params.pretty {
        // serialize with Json's pretty variant, or a manually indented body
    }
    // ...
}
```

`#[serde(default)]` is doing real work here: `?pretty` with no value, or the query string entirely absent, both need to deserialize successfully. Without `#[serde(default)]`, a missing `pretty` key is a **deserialization error** (`Query` extraction failure → axum returns `400 Bad Request` before your handler even runs) because `bool` has no `Default` fallback baked into `Query`'s deserializer. `#[serde(default)]` tells serde "use `bool::default()` (`false`) if the key is absent," which is exactly ES's semantics for boolean query flags — presence, with or without `=true`, means true; absence means false.

Extractor order matters: `Path` then `Query` then `Json` (body) — axum requires extractors that don't consume the request body (`Path`, `Query`, headers) before the one that does (`Json`, `Bytes`, `String`). This mirrors Tutorial 04's ordering rule for `State`; the compiler error if violated is the same `FromRequest` trait-bound message.

`pretty` isn't implemented end-to-end below (real ES's pretty-printing is a `serde_json::to_string_pretty` swap, orthogonal to everything this tutorial covers) — struct is defined and extracted to establish the pattern; wire the actual pretty-print branch when/if a test demands it.

---

## 3. `HEAD` Requests

`HEAD /{index}` checks index existence — same route as `GET`, but the HTTP spec says: identical headers, **zero body**. Axum's `head()` function routes it:

```rust
use axum::routing::{get, head};

Router::new().route("/{index}", get(get_index).head(head_index))
```

Handler:

```rust
async fn head_index(
    State(state): State<AppState>,
    Path(index_name): Path<String>,
) -> StatusCode {
    let store = state.store.read().unwrap_or_else(|p| p.into_inner());
    if store.get(&index_name).is_ok() {
        StatusCode::OK
    } else {
        StatusCode::NOT_FOUND
    }
}
```

Returning a bare `StatusCode` (rather than `(StatusCode, Json<T>)`) is what makes the response bodyless — `StatusCode` alone implements `IntoResponse` by producing a response with an empty body. Axum's `MethodRouter` also auto-generates a `HEAD` response for any route with a `GET` handler by running the `GET` handler and discarding the body — but that would still allocate and serialize the full `IndexMetadata` JSON just to throw it away. Handling `HEAD` explicitly, cheaply, with a plain existence check is the correct lazy version here: less work per request, not more code.

---

## 4. Method Routing on One Path

ES has one URL, `/{index}`, answering four different verbs. Axum chains methods on a single `route()` call — each verb is independent, same path pattern:

```rust
Router::new().route(
    "/{index}",
    get(get_index).put(create_index).delete(delete_index).head(head_index),
)
```

This reads left-to-right as "GET → this fn, PUT → this fn, DELETE → this fn, HEAD → this fn," all against `/{index}`. An unmatched verb (e.g. `POST /{index}`) still 405s automatically — same "unmatched routes just work" guarantee from Tutorial 04, now extended to unmatched *methods* on a matched path.

Axum 0.8's path syntax uses `{index}` (curly braces), **not** the old `:index` colon syntax from axum 0.6/0.7 — confirmed already current in this codebase per Tutorial 04/06's routes; called out again here since Task 6's own spec text in `EXECUTION.md` still writes `{index}`, which matches.

---

## 5. `IndexStoreError` — Library-Level Errors in `es-index`

Tutorial 05 deferred this: `IndexStore` methods (`create`, `get`, `delete`) don't return `Result` yet, because nothing failed yet. `PUT /{index}` against an existing index is the first real domain failure. Time to add it — but **not** as `AppError`.

`AppError` (Tutorial 05) is an HTTP/presentation type: it knows about `StatusCode`, `IntoResponse`, and ES's `{"error": {"type", "reason"}, "status"}` JSON envelope. `es-index` is a library crate with zero axum dependency and zero business reason to gain one — a document-store engine, a CLI import tool, or a future gRPC frontend could all depend on `es-index` and none of them want to link `axum` just to get a `Result` out of `IndexStore::get()`.

The rule: **a library crate's error type describes what went wrong in domain terms; only the binary that turns errors into an HTTP response needs to know what status code or JSON shape that implies.** This is the same separation as a Python library raising its own `class IndexNotFoundError(Exception)` instead of an HTTP framework's `abort(404)` — the library has no idea it's being called from a web server.

`es-index/src/error.rs`:

```rust
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum IndexStoreError {
    #[error("index already exists: {0}")]
    AlreadyExists(String),

    #[error("index not found: {0}")]
    NotFound(String),
}
```

No `#[from]`, no `IntoResponse`, no `StatusCode` — this type only ever describes "which index, which failure." `es-index/src/lib.rs` gains `mod error; pub use error::IndexStoreError;`.

### `IndexStore` methods updated

```rust
impl IndexStore {
    pub fn create(&mut self, metadata: IndexMetadata) -> Result<(), IndexStoreError> {
        if self.indices.contains_key(&metadata.name) {
            return Err(IndexStoreError::AlreadyExists(metadata.name.clone()));
        }
        self.indices.insert(metadata.name.clone(), metadata);
        Ok(())
    }

    pub fn get(&self, name: &str) -> Result<&IndexMetadata, IndexStoreError> {
        self.indices
            .get(name)
            .ok_or_else(|| IndexStoreError::NotFound(name.to_string()))
    }

    pub fn delete(&mut self, name: &str) -> Result<IndexMetadata, IndexStoreError> {
        self.indices
            .remove(name)
            .ok_or_else(|| IndexStoreError::NotFound(name.to_string()))
    }

    // Unchanged — see below for why.
    pub fn list(&self) -> Vec<&IndexMetadata> {
        let mut v: Vec<&IndexMetadata> = self.indices.values().collect();
        v.sort_by(|a, b| a.name().cmp(b.name()));
        v
    }
}
```

`list()` stays `Vec<&IndexMetadata>`, not `Result<Vec<&IndexMetadata>, IndexStoreError>`. Iterating a `HashMap` and collecting into a `Vec` cannot fail — there's no missing-key case, no precondition to violate, nothing to report. Wrapping an infallible operation in `Result` is the Rust equivalent of a Python function `return None` in every branch just to satisfy a signature that promised it might raise — it doesn't communicate anything, and every caller pays a `.unwrap()` or `?` tax for a failure mode that structurally cannot occur. `Result` is for operations with a real failure branch; `list()` doesn't have one.

### Converting `IndexStoreError` → `AppError` in `es-mini`

`AppError` (Tutorial 05) gains two things: existing variants get reused (`IndexNotFound`, `ResourceAlreadyExists` already exist, zero call sites until now), and a `From<IndexStoreError>` impl bridges the two error types so `?` works across the crate boundary:

```rust
// es-mini/src/error.rs, added to the existing AppError impl block area

impl From<es_index::IndexStoreError> for AppError {
    fn from(err: es_index::IndexStoreError) -> Self {
        match err {
            es_index::IndexStoreError::AlreadyExists(name) => AppError::ResourceAlreadyExists(name),
            es_index::IndexStoreError::NotFound(name) => AppError::IndexNotFound(name),
        }
    }
}
```

With `From` in place, handlers use `?` directly — no manual `match` per call site:

```rust
async fn create_index_handler(
    State(state): State<AppState>,
    Path(index_name): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    let metadata = IndexMetadata::new(&index_name, 1, 0);
    {
        let mut store = state.store.write().unwrap_or_else(|p| p.into_inner());
        store.create(metadata)?;   // IndexStoreError -> AppError via From, then AppError propagates
    }
    Ok(Json(json!({
        "acknowledged": true,
        "shards_acknowledged": true,
        "index": index_name,
    })))
}
```

If `From` weren't defined, the alternative is `.map_err(AppError::from)?` or `.map_err(|e| match e { ... })?` spelled out at every call site — `From` exists precisely so that's written once, in one place, and every handler that touches the store gets it for free through `?`'s automatic conversion.

The chain end to end: **`IndexStoreError` (domain fact, no HTTP knowledge) → `AppError` (via `From`, adds HTTP status + ES error-type string) → `IntoResponse` (Tutorial 05, turns it into the actual `{"error": {...}, "status": N}` JSON response).** Three types, three concerns, one `?` per handler.

---

## 6. The Endpoints

All handlers follow Tutorial 06's scoped-guard pattern: acquire the lock, do the work, drop the guard before returning.

```rust
// es-mini/src/handlers/index.rs (or inline in main.rs, per existing project layout)

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use es_index::IndexMetadata;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{AppError, AppState};

#[derive(Debug, Deserialize)]
pub struct PrettyQuery {
    #[serde(default)]
    pub pretty: bool,
}

/// PUT /{index}
pub async fn create_index(
    State(state): State<AppState>,
    Path(index_name): Path<String>,
    body: Option<Json<Value>>,
) -> Result<impl IntoResponse, AppError> {
    let _settings_body = body.map(|Json(v)| v); // parsed, not yet applied — Tutorial 08+ territory
    let metadata = IndexMetadata::new(&index_name, 1, 0);

    {
        let mut store = state.store.write().unwrap_or_else(|p| p.into_inner());
        store.create(metadata)?;
    }

    Ok(Json(json!({
        "acknowledged": true,
        "shards_acknowledged": true,
        "index": index_name,
    })))
}

/// GET /{index}
pub async fn get_index(
    State(state): State<AppState>,
    Path(index_name): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    let body = {
        let store = state.store.read().unwrap_or_else(|p| p.into_inner());
        let meta = store.get(&index_name)?;
        json!({
            index_name.clone(): {
                "aliases": {},
                "mappings": {},
                "settings": {
                    "index": {
                        "number_of_shards": meta.number_of_shards.to_string(),
                        "number_of_replicas": meta.number_of_replicas.to_string(),
                    }
                }
            }
        })
    };
    Ok(Json(body))
}

/// HEAD /{index}
pub async fn head_index(
    State(state): State<AppState>,
    Path(index_name): Path<String>,
) -> StatusCode {
    let store = state.store.read().unwrap_or_else(|p| p.into_inner());
    if store.get(&index_name).is_ok() {
        StatusCode::OK
    } else {
        StatusCode::NOT_FOUND
    }
}

/// DELETE /{index}
pub async fn delete_index(
    State(state): State<AppState>,
    Path(index_name): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    {
        let mut store = state.store.write().unwrap_or_else(|p| p.into_inner());
        store.delete(&index_name)?;
    }
    Ok(Json(json!({ "acknowledged": true })))
}

/// GET /{index}/_mapping
pub async fn get_mapping(
    State(state): State<AppState>,
    Path(index_name): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    let store = state.store.read().unwrap_or_else(|p| p.into_inner());
    store.get(&index_name)?;
    Ok(Json(json!({ index_name: { "mappings": {} } })))
}

/// GET /{index}/_settings
pub async fn get_settings(
    State(state): State<AppState>,
    Path(index_name): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    let body = {
        let store = state.store.read().unwrap_or_else(|p| p.into_inner());
        let meta = store.get(&index_name)?;
        json!({
            index_name.clone(): {
                "settings": {
                    "index": {
                        "number_of_shards": meta.number_of_shards.to_string(),
                        "number_of_replicas": meta.number_of_replicas.to_string(),
                    }
                }
            }
        })
    };
    Ok(Json(body))
}

/// PUT /{index}/_settings
pub async fn update_settings(
    State(state): State<AppState>,
    Path(index_name): Path<String>,
    Json(_settings): Json<Value>,
) -> Result<impl IntoResponse, AppError> {
    // Confirms the index exists; applying settings mutation deferred — no mutable
    // settings field exists on IndexMetadata yet (number_of_replicas is currently
    // fixed at creation). Wire real mutation when a setting actually needs to change.
    let store = state.store.read().unwrap_or_else(|p| p.into_inner());
    store.get(&index_name)?;
    Ok(Json(json!({ "acknowledged": true })))
}
```

`PrettyQuery` is defined and importable but not threaded through every handler above — every handler this tutorial adds is a small, fixed JSON shape; wiring `?pretty` into each is repeated boilerplate for a cosmetic feature no test in this tutorial exercises. Add the `Query<PrettyQuery>` parameter to a handler the day a test asserts on indentation.

`create_index`'s `body: Option<Json<Value>>` — `Option` around an extractor makes it accept **either** a JSON body **or** no body at all (empty `PUT /my-index` is valid ES usage) without a 415/400 on the empty case. Contrast with a bare `Json<Value>`, which would reject a bodyless `PUT`.

### Router wiring

```rust
// es-mini/src/main.rs

use axum::routing::get;

let app = Router::new()
    .route("/", get(root_handler))
    .route("/_cluster/health", get(cluster_health_handler))
    .route("/_cat/indices", get(cat_indices_handler))
    .route(
        "/{index}",
        get(get_index)
            .put(create_index)
            .delete(delete_index)
            .head(head_index),
    )
    .route("/{index}/_mapping", get(get_mapping))
    .route(
        "/{index}/_settings",
        get(get_settings).put(update_settings),
    )
    .with_state(state);
```

All six index-management routes visible together, mirroring how the routes read in the ES REST API spec.

---

## 7. Integration Tests

### Unit tests vs. integration tests

`#[cfg(test)] mod tests { ... }` inside `es-index/src/lib.rs` (used since Tutorial 03) is a **unit test** — same crate, same compilation unit, can reach private items. An **integration test** lives in a top-level `tests/` directory next to `src/`; each file in `tests/` compiles as its own separate crate that only sees the public API — exactly the view an external HTTP client has. For `es-mini` specifically, "external HTTP client" is the point: these tests build the real `Router`, with the real `AppState`, and drive it over an actual (in-process) HTTP request/response cycle, catching anything a handler-level unit test can't — wrong route, wrong method, extractor mismatches, header/status code correctness.

Analogy: pytest's `tests/` directory hitting a running Flask app via its test client, vs. a bare `unittest.TestCase` calling a view function directly in-process.

### `axum-test` over `reqwest`

Two ways to drive the router over HTTP in a test: spin up a real `TcpListener` and hit it with `reqwest` (a full HTTP client, real sockets, real port), or use `axum-test`, a crate purpose-built for this exact job — it wraps an axum `Router` and simulates requests without binding a real socket. `axum-test = "21"` targets axum `0.8.x`, compatible with this project's pinned `axum = { version = "0.8" }` (Tutorial 04). Less setup than `reqwest` (no listener, no port, no address-in-use flakiness in parallel test runs) for the same assertions — the lazier and more reliable option, so it's the one used below. Reach for `reqwest` instead only if a test needs to be a *real* separate OS process talking real sockets (e.g. later cross-engine compatibility tests in Phase 4/5) — not needed here.

`es-mini/Cargo.toml`:

```toml
[dev-dependencies]
axum-test = "21"
tokio = { workspace = true, features = ["macros"] }
```

`[dev-dependencies]` — only compiled for `cargo test`, never shipped in the release binary. Same reasoning as `#[cfg(test)]`, one level up: test-only tooling shouldn't cost anything in `cargo build --release`.

### `#[tokio::test]`

Integration tests need an async runtime to `.await` the server calls, but `cargo test`'s default test harness has no idea what `async fn` even means — it expects `fn test_name()`, synchronous, full stop. `#[tokio::test]` is a macro that wraps the async test body in its own single-threaded tokio runtime and blocks on it, same relationship as `#[tokio::main]` (Tutorial 04 §3) has to `fn main()`:

```rust
#[tokio::test]
async fn my_test() {
    // full .await access here
}
```

Difference from plain `#[test]`: `#[test]` requires a synchronous `fn` (no `async`, compiler rejects `async fn` under bare `#[test]` with an error about `Output` not implementing a required trait); `#[tokio::test]` defaults to `flavor = "current_thread"` (a single-threaded runtime per test — fine, since each test's server is independent), whereas `#[tokio::main]` in `main.rs` defaults to the multi-thread flavor. Override with `#[tokio::test(flavor = "multi_thread")]` if a test specifically needs multiple worker threads; not needed here.

### Shared test helper

Every test needs the same `Router` built with a fresh `IndexStore`. Rather than repeat that in every `#[tokio::test]` function, factor it into a helper module. Files in `tests/` don't share code with each other automatically (each is its own crate) — the convention is a `tests/common/mod.rs` (the `mod.rs` name, not `common.rs`, tells cargo this is a shared module, not a standalone test file to run on its own):

```rust
// es-mini/tests/common/mod.rs
use axum_test::TestServer;
use es_mini::build_app; // requires main.rs's app-building logic factored into a `pub fn build_app(...) -> Router` in lib.rs / a small lib target

pub fn test_server() -> TestServer {
    let app = build_app("es-mini-test".to_string());
    TestServer::new(app).unwrap()
}
```

This requires `es-mini` to expose the router-building code as a library function, not buried inline in `fn main()` — a small refactor: extract everything currently between "construct `AppState`" and "`axum::serve(...)`" into `pub fn build_app(cluster_name: String) -> Router` in a new `es-mini/src/lib.rs`, and have `main.rs` call it. `es-mini` becomes a binary crate with a thin `src/main.rs` plus a small library part — a common Rust pattern once a binary needs to be testable from `tests/`, since `tests/` can only depend on a crate's public *library* API, never on `main.rs` directly.

### Integration tests

```rust
// es-mini/tests/index_management.rs
mod common;

use common::test_server;
use serde_json::json;

#[tokio::test]
async fn create_get_delete_happy_path() {
    let server = test_server();

    let resp = server.put("/my-index").await;
    resp.assert_status_ok();
    resp.assert_json_contains(&json!({ "acknowledged": true, "index": "my-index" }));

    let resp = server.get("/my-index").await;
    resp.assert_status_ok();
    resp.assert_json_contains(&json!({ "my-index": {} }));

    let resp = server.delete("/my-index").await;
    resp.assert_status_ok();
    resp.assert_json_contains(&json!({ "acknowledged": true }));

    // now gone
    let resp = server.get("/my-index").await;
    resp.assert_status_not_found();
}

#[tokio::test]
async fn head_reflects_existence() {
    let server = test_server();

    server.head("/my-index").await.assert_status_not_found();

    server.put("/my-index").await.assert_status_ok();
    server.head("/my-index").await.assert_status_ok();
}

#[tokio::test]
async fn duplicate_create_is_400() {
    let server = test_server();

    server.put("/my-index").await.assert_status_ok();
    let resp = server.put("/my-index").await;

    resp.assert_status_bad_request();
    resp.assert_json_contains(&json!({
        "error": { "type": "resource_already_exists_exception" }
    }));
}

#[tokio::test]
async fn missing_index_operations_are_404() {
    let server = test_server();

    let resp = server.get("/does-not-exist").await;
    resp.assert_status_not_found();
    resp.assert_json_contains(&json!({
        "error": { "type": "index_not_found_exception" }
    }));

    let resp = server.delete("/does-not-exist").await;
    resp.assert_status_not_found();
}
```

Run with:

```bash
cargo test -p es-mini --test index_management
```

`--test index_management` runs only that integration test binary; omit it to run everything (unit + all integration files) with `cargo test -p es-mini`.

---

## 8. Definition of Done

1. `cargo build --workspace` succeeds with `IndexStoreError` added to `es-index` and `From<IndexStoreError> for AppError` added to `es-mini`.
2. `cargo clippy --workspace --all-targets -- -D warnings` passes.
3. `Router` includes all six routes: `PUT`/`GET`/`DELETE`/`HEAD /{index}`, `GET /{index}/_mapping`, `GET`/`PUT /{index}/_settings`.
4. `curl -X PUT localhost:9200/my-index` returns `200` (bare `Json(...)` defaults to `200 OK`, matching real ES's `PUT /{index}` response) with `{"acknowledged":true,"shards_acknowledged":true,"index":"my-index"}`.
5. `curl -I localhost:9200/my-index` (HEAD) returns `200` with an empty body (verify with `curl -I -v` and check `Content-Length` / absent body); `curl -I localhost:9200/nope` returns `404`.
6. `curl -X PUT localhost:9200/my-index` a second time returns `400` with `error.type == "resource_already_exists_exception"`.
7. `curl localhost:9200/does-not-exist` returns `404` with `error.type == "index_not_found_exception"`.
8. `cargo test -p es-mini --test index_management` passes all four integration tests (happy path, HEAD existence, duplicate create, missing-index 404s).
9. `cargo test --workspace` passes (unit tests in `es-index` for `IndexStore::create`/`get`/`delete` error variants, plus the new integration suite).
10. `es-mini/src/lib.rs` exists exposing `build_app(...)`, and `tests/common/mod.rs` is reused (not duplicated) across every file under `es-mini/tests/`.
