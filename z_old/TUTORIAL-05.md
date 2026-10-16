# Tutorial 05 — Error Handling the Rust Way

**Series:** es-mini — Building Elasticsearch in Rust
**Phase:** Cross-cutting (needed before Task 6 — index management endpoints)
**Prerequisites:** Tutorials 01–04 complete — workspace builds, axum server serves `GET /` and `GET /_cluster/health`.

---

By the end of this tutorial you will have:

- The mental model for why Rust has no exceptions
- Fluency with `?`, `unwrap()`, `expect()`, and `Box<dyn Error>`
- `thiserror` for library-style error enums
- `anyhow` for application-level error handling with context
- A working `AppError` enum in `es-mini/src/error.rs` with `IntoResponse` — the pattern every future handler uses
- The ES error response shape (`{"error": {"type": "...", "reason": "..."}, "status": N}`) implemented for real

---

## 1. The Mental Model Shift: Errors Are Values

JS and Python make errors a parallel control-flow channel — `throw`/`raise` unwinds the stack until something catches it. Nothing in a function's signature tells you it can fail:

```python
def get_index(name: str) -> dict:
    # Can this raise? Read the whole function (and everything it calls) to find out.
    ...
```

```js
async function getIndex(name) {
  // Can this reject? Same problem — signature doesn't say.
}
```

Rust has no exceptions for recoverable errors (`panic!` exists and unwinds, but it's for *unrecoverable* bugs, not expected failure modes — see §3). Every fallible function returns `Result<T, E>` — an enum with exactly two variants:

```rust
enum Result<T, E> {
    Ok(T),
    Err(E),
}
```

The return type documents fallibility. `fn get_index(name: &str) -> Result<IndexMetadata, AppError>` tells you, at the signature, that this can fail and exactly what failure looks like. The compiler will not let you use a `Result<T, E>` as if it were a `T` — you must explicitly unwrap, match, or propagate it.

**The closest analogy you already know:** Go's `(value, error)` return convention.

```go
val, err := getIndex(name)
if err != nil {
    return nil, err
}
```

Rust's `Result<T, E>` is the same idea made into an actual type instead of a two-value convention — which means the compiler enforces handling it (Go lets you silently ignore `err`; Rust does not let you silently ignore an unused `Result`, it's a warning by default and easy to deny).

If you know functional programming: `Result<T, E>` is the `Either` monad, specialized — `Ok` is `Right`, `Err` is `Left`. `.map()`, `.and_then()`, `?` all exist and behave the way you'd expect from `Either`.

**Compiler says / fix — ignoring a `Result`:**

```rust
fn write_file() -> Result<(), std::io::Error> { /* ... */ }

fn main() {
    write_file();   // warning, not an error, by default
}
```

```
warning: unused `Result` that must be used
  --> src/main.rs:5:5
   |
 5 |     write_file();
   |     ^^^^^^^^^^^^^
   |
   = note: this `Result` may be an `Err` variant, which should be handled
   = note: `#[warn(unused_must_use)]` on by default
```

**Fix:** handle it explicitly — `let _ = write_file();` (deliberately discard), `write_file().unwrap();` (crash on error), or `write_file()?;` (propagate, see §2). The `#[must_use]` attribute on `Result` is why this warns at all; it's what makes "silently swallowed error" a lint instead of a silent bug.

---

## 2. The `?` Operator

Writing out error propagation by hand is what you'd do in Go:

```rust
fn load_index(name: &str) -> Result<IndexMetadata, AppError> {
    let raw = match std::fs::read_to_string(name) {
        Ok(s) => s,
        Err(e) => return Err(AppError::from(e)),
    };
    let meta = match serde_json::from_str(&raw) {
        Ok(m) => m,
        Err(e) => return Err(AppError::from(e)),
    };
    Ok(meta)
}
```

`?` is sugar for exactly that match:

```rust
fn load_index(name: &str) -> Result<IndexMetadata, AppError> {
    let raw = std::fs::read_to_string(name)?;
    let meta = serde_json::from_str(&raw)?;
    Ok(meta)
}
```

`expr?` means: if `expr` is `Ok(v)`, evaluate to `v`. If it's `Err(e)`, return `Err(From::from(e))` immediately from the enclosing function. That `From::from` conversion is the important part — the error type of `expr` doesn't have to match the function's declared error type exactly, it just has to have a `From` impl converting one into the other (§5 builds these).

This is the same shape as JS's `await` unwrapping a rejected promise into a thrown exception that propagates up — except here it's explicit at every call site (`?` is visible in the source) rather than an implicit stack unwind.

**Common mistake — using `?` in a function that doesn't return `Result`:**

```rust
fn main() {
    let raw = std::fs::read_to_string("config.json")?;   // compile error
    println!("{}", raw);
}
```

**Compiler says:**

```
error[E0277]: the `?` operator can only be used in a function that returns
              `Result` or `Option` (or another type that implements `FromResidual`)
 --> src/main.rs:2:44
  |
1 | fn main() {
  | ---------- this function should return `Result` or `Option` to accept `?`
2 |     let raw = std::fs::read_to_string("config.json")?;
  |               ---------------------------------------^ cannot use the `?` operator
  |               |                                        in a function that returns `()`
```

**Fix:** either handle the `Result` inline (`.unwrap()`, `match`, `if let Err(e) = ...`), or change the function's return type. `main()` is allowed to return `Result` — this is idiomatic for small binaries/tools:

```rust
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let raw = std::fs::read_to_string("config.json")?;
    println!("{}", raw);
    Ok(())
}
```

If `main()` returns `Err`, the process exits with status 1 and prints the error via `Debug`.

---

## 3. `unwrap()`, `expect()`, and `unreachable!()`

`.unwrap()` on a `Result<T, E>` or `Option<T>` extracts the `Ok`/`Some` value or **panics** — crashes the current thread with a message and (in a server) generally kills the request handling task. It is the closest thing Rust has to letting an exception fly uncaught, and it should be treated with the same caution.

**When it's fine:**

- **Tests** — a panic in a test is exactly a failed assertion. `.unwrap()` is the normal way to assert "this operation succeeded."
- **Prototyping** — sketching logic before deciding on real error handling. Comes with an implicit TODO to replace it.
- **Genuinely unreachable branches** — where the type system can't express "this Option is always Some" but you know it from invariants elsewhere:

```rust
let first_shard = shards.first().unwrap();   // shards is non-empty by construction, always
```

Prefer `expect("message")` over bare `.unwrap()` — it's the same crash, but the message is what a human reads at 3am in the panic output:

```rust
let first_shard = shards
    .first()
    .expect("IndexMetadata::new always creates at least one shard");
```

`unreachable!()` documents a `match` arm that the type system requires you to write but that logically can never execute:

```rust
match http_method {
    Method::GET | Method::POST | Method::PUT | Method::DELETE => handle(m),
    _ => unreachable!("router only dispatches GET/POST/PUT/DELETE"),
}
```

If it ever does execute, that's a bug worth a panic with a clear message, not a silently wrong result.

**In `es-mini` HTTP handlers, `.unwrap()`/`.expect()` are NOT acceptable** — an untrusted request should never be able to crash a handler task. Every fallible operation in request-handling code returns a `Result` that flows through `AppError` (§9).

---

## 4. `Box<dyn Error>` — Quick and Dirty

Before building a real error enum, the fastest way to make disparate error types (`std::io::Error`, `serde_json::Error`, etc.) compose is `Box<dyn std::error::Error>` — a trait object, roughly "any type implementing the `Error` trait, boxed on the heap because its size isn't known at compile time."

```rust
fn load_config(path: &str) -> Result<Config, Box<dyn std::error::Error>> {
    let raw = std::fs::read_to_string(path)?;    // io::Error -> Box<dyn Error> via From
    let cfg = serde_json::from_str(&raw)?;       // serde_json::Error -> Box<dyn Error> via From
    Ok(cfg)
}
```

`?` works here because `Box<dyn Error>` has a blanket `From<E> for Box<dyn Error>` impl covering any concrete error type. This is genuinely the fastest way to get something compiling, and it's fine for `main()` and small tools.

**Why it's bad at library API boundaries:** the caller only sees `Box<dyn Error>` — no way to `match` on what specifically went wrong without downcasting (`err.downcast_ref::<SpecificError>()`), which is clunky and easy to get wrong. A library function returning `Box<dyn Error>` is like a Python function that only ever raises bare `Exception` — technically an error occurred, no structured way to handle specific cases differently.

es-mini's HTTP layer needs to distinguish "index not found" (404) from "bad request body" (400) from "internal bug" (500) — `Box<dyn Error>` can't drive that dispatch cleanly. That's what `thiserror` is for.

---

## 5. `thiserror` — Typed Errors for Libraries

**Version at time of writing: `thiserror = "2.0.19"`.** The derive syntax below is the current 2.x API — v2 kept the same `#[derive(Error)]` / `#[error("...")]` / `#[from]` surface as v1; the breaking changes in 2.0 were internal (MSRV, `#[source]` inference edge cases), not this API.

`thiserror` generates the boilerplate `impl std::error::Error` and `impl Display` for a hand-written enum. Analogy: defining a custom `Exception` subclass hierarchy in Python (`class IndexNotFoundError(Exception): ...`) instead of raising bare strings — each variant is a distinct, matchable type, but here it's a compile-time-checked enum instead of a class hierarchy.

```toml
# Cargo.toml [workspace.dependencies]
thiserror = { version = "2" }
```

```rust
use thiserror::Error;

#[derive(Debug, Error)]
enum StoreError {
    #[error("index '{0}' not found")]
    NotFound(String),

    #[error("index '{0}' already exists")]
    AlreadyExists(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),   // automatic `impl From<std::io::Error> for StoreError`
}
```

- `#[error("...")]` — the `Display` format string. `{0}` refers to the variant's first unnamed field (tuple variant); named fields use `{field_name}`.
- `#[from]` — generates `impl From<std::io::Error> for StoreError`, which is exactly what makes `?` work when a function returning `std::io::Error` is called from a function returning `StoreError`:

```rust
fn read_index_file(path: &str) -> Result<String, StoreError> {
    let contents = std::fs::read_to_string(path)?;   // io::Error auto-converts via #[from]
    Ok(contents)
}
```

Without `#[from]`, that `?` would fail to compile — there'd be no conversion path from `std::io::Error` to `StoreError`, same error shape as the mismatched-`?` case in §2 but pointing at a missing `From` impl instead of a missing `Result` return type.

`thiserror` is the right tool for **library-facing** error types — types other code will `match` on to make decisions. `es-index` and `es-query` would use `thiserror` enums if/when they need their own error types (deferred — see §9's note on scope).

---

## 6. `anyhow` — Application-Level Error Handling

**Version at time of writing: `anyhow = "1.0.104"`.**

`anyhow::Error` is the opposite tradeoff from `thiserror`: one flexible error type that erases the specific variant, in exchange for effortless composition and `.context()` — attaching a human-readable breadcrumb at each layer without hand-rolling a new enum variant per failure site.

```toml
anyhow = { version = "1" }
```

```rust
use anyhow::{Context, Result};   // anyhow::Result<T> = Result<T, anyhow::Error>

fn load_settings(path: &str) -> Result<Settings> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading settings file at {path}"))?;
    let settings: Settings = serde_json::from_str(&raw)
        .context("parsing settings JSON")?;
    Ok(settings)
}
```

If `read_to_string` fails, the error printed is:

```
Error: reading settings file at /etc/es-mini/settings.json

Caused by:
    No such file or directory (os error 2)
```

This is the direct analogue of Python's `raise SettingsError("reading settings file") from original_exception` — wrapping an exception with added context while preserving the original as the cause chain. `.context()` takes a fixed string (cheap, use when there's no interpolation); `.with_context(|| ...)` takes a closure (lazy — only formats the string if the error path is actually hit, useful when the message itself does work like a `format!`).

**When to use which:**

| | `thiserror` | `anyhow` |
|---|---|---|
| Where | Library crates (`es-index`, `es-query`, `es-codec`) | Application/binary code (`es-mini`'s `main.rs`, CLI tools, `xtask`) |
| Callers need to `match` on specific failure | Yes | No |
| Error type is part of the public API | Yes | No — `anyhow::Error` is deliberately opaque |
| Goal | Structured, typed failure modes | Fast, ergonomic, "just tell me what broke and why" |

es-mini's HTTP layer is a middle case: it needs typed variants (to map to specific HTTP status codes) but lives in the binary crate. That's `thiserror`, not `anyhow` — §9 builds it.

---

## 7. Mapping Errors to HTTP in Axum: `IntoResponse`

The key axum pattern: implement `axum::response::IntoResponse` for your error type, and every handler can return `Result<T, AppError>` and use `?` freely. Axum calls `.into_response()` on whichever variant comes back — `Ok(T)` uses `T`'s `IntoResponse` impl, `Err(AppError)` uses `AppError`'s.

```rust
use axum::response::{IntoResponse, Response};
use axum::http::StatusCode;
use axum::Json;

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = self.status_code();
        let body = self.to_es_error_body(); // §9 implements this on AppError
        (status, Json(body)).into_response()
    }
}
```

`axum::response::IntoResponse` (and `Response`) live in `axum::response` in 0.8.x — same module path as every axum version since 0.6, unchanged here. Once this impl exists, a handler looks like:

```rust
async fn get_index_handler(
    // Simplified for this example — Tutorial 06 wraps `store` inside `AppState`,
    // so the real extractor becomes `State(state): State<AppState>`.
    State(store): State<Arc<RwLock<IndexStore>>>,
    Path(name): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    // Recover from a poisoned lock rather than crash the handler task (§3: no bare
    // `.unwrap()` in request-handling code) — same pattern Tutorial 06 establishes.
    let store = store.read().unwrap_or_else(|poisoned| poisoned.into_inner());
    let meta = store.get(&name).ok_or_else(|| AppError::IndexNotFound(name.clone()))?;
    Ok((StatusCode::OK, Json(meta.clone())))
}
```

No manual `match` on success/failure in the handler body — `?` on the `Option::ok_or_else` result does the work, and `IntoResponse` on `AppError` handles turning it into the right status code and JSON body automatically. Every handler from Tutorial 06 onward follows this shape.

---

## 8. The ES Error Response Shape

Real Elasticsearch error responses look like this:

```json
{
  "error": {
    "root_cause": [
      { "type": "index_not_found_exception", "reason": "no such index [foo]" }
    ],
    "type": "index_not_found_exception",
    "reason": "no such index [foo]"
  },
  "status": 404
}
```

es-mini implements the simplified two-level shape (flat `type`/`reason`, no `root_cause` array — that's a nested-cause feature real ES uses for multi-shard failures, out of scope for a single-node in-memory MVP):

```json
{ "error": { "type": "index_not_found_exception", "reason": "no such index [foo]" }, "status": 404 }
```

```rust
use serde::Serialize;

#[derive(Debug, Serialize)]
struct EsErrorBody {
    #[serde(rename = "type")]
    error_type: String,
    reason: String,
}

#[derive(Debug, Serialize)]
struct EsError {
    error: EsErrorBody,
    status: u16,
}
```

`#[serde(rename = "type")]` is needed because `type` is a reserved keyword in Rust — the field is named `error_type` in the struct and renamed to `"type"` on the wire (same trick used for `pri`/`rep` in Tutorial 03's `IndexInfo`).

---

## 9. `AppError` — The Full Implementation

### Why `AppError` lives in `es-mini`, not `es-index`

The HTTP status code and JSON error shape are a **presentation concern** — how a failure is communicated to an HTTP client — not a property of the index/document domain model itself. `es-index` doesn't know it's being driven over HTTP; it could equally be embedded in a CLI tool or a different transport. Coupling its error type to `StatusCode` and `IntoResponse` would leak an axum dependency into a library crate that has no other reason to depend on axum.

`es-index` can (and later will, if warranted) grow its own library-level error enum — e.g., `IndexStoreError` with variants like `NotFound`, `AlreadyExists` — built with `thiserror`, with no knowledge of HTTP at all. `AppError` in `es-mini` would then have a variant wrapping it (`#[error(transparent)] Store(#[from] IndexStoreError)`) and map it to the right status code. That step is **deferred** — Tutorial 05's in-memory `IndexStore` (Tutorial 03) doesn't yet return `Result` from its methods, so there's nothing to wrap yet. Tutorial 06/07 revisit this if the store starts returning fallible results.

### `es-mini/Cargo.toml`

```toml
[package]
name = "es-mini"
version.workspace = true
edition.workspace = true

[dependencies]
es-query  = { workspace = true }
es-index  = { workspace = true }
axum      = { workspace = true }
tokio     = { workspace = true }
tracing   = { workspace = true }
tracing-subscriber = { workspace = true }
serde     = { workspace = true }
serde_json = { workspace = true }
thiserror = { workspace = true }
```

Add to root `Cargo.toml` `[workspace.dependencies]`:

```toml
thiserror = { version = "2" }
```

(`anyhow` is not added yet — nothing in `es-mini` needs it until a later tutorial introduces multi-step setup code, e.g., loading config at startup, where `.context()` earns its keep. Adding it now with no call site would be dead weight.)

### `es-mini/src/error.rs`

```rust
//! `AppError` — the single error type used across all HTTP handlers.
//!
//! Maps domain-level failures to Elasticsearch-shaped error responses.
//! Lives in the binary crate: HTTP status codes and response shape are a
//! presentation concern, not part of the `es-index` domain model.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;
use thiserror::Error;

/// All error conditions that can occur while handling an HTTP request.
#[derive(Debug, Error)]
pub enum AppError {
    #[error("no such index [{0}]")]
    IndexNotFound(String),

    #[error("index [{0}] already exists")]
    ResourceAlreadyExists(String),

    #[error("document not found: index [{index}], id [{id}]")]
    DocumentNotFound { index: String, id: String },

    #[error("validation failed: {0}")]
    InvalidRequest(String),

    #[error("internal error: {0}")]
    Internal(String),
}

impl AppError {
    /// HTTP status code for this error variant.
    fn status_code(&self) -> StatusCode {
        match self {
            AppError::IndexNotFound(_) => StatusCode::NOT_FOUND,
            AppError::ResourceAlreadyExists(_) => StatusCode::BAD_REQUEST,
            AppError::DocumentNotFound { .. } => StatusCode::NOT_FOUND,
            AppError::InvalidRequest(_) => StatusCode::BAD_REQUEST,
            AppError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// The ES-style exception type string, e.g. `"index_not_found_exception"`.
    ///
    /// Note: real Elasticsearch's actual exception class for a missing
    /// document is `document_missing_exception` (not `document_not_found_exception`,
    /// despite the Rust variant name here matching es-mini's own vocabulary) —
    /// kept faithful to the real API for client compatibility.
    fn error_type(&self) -> &'static str {
        match self {
            AppError::IndexNotFound(_) => "index_not_found_exception",
            AppError::ResourceAlreadyExists(_) => "resource_already_exists_exception",
            AppError::DocumentNotFound { .. } => "document_missing_exception",
            AppError::InvalidRequest(_) => "action_request_validation_exception",
            AppError::Internal(_) => "internal_server_exception",
        }
    }

    /// Build the ES-shaped error envelope (§8) for this variant.
    fn to_es_error_body(&self) -> EsError {
        EsError {
            error: EsErrorBody {
                error_type: self.error_type().to_string(),
                reason: self.to_string(), // uses the #[error("...")] Display impl
            },
            status: self.status_code().as_u16(),
        }
    }
}

/// Nested `error` object in an Elasticsearch error response.
#[derive(Debug, Serialize, PartialEq, Eq)]
struct EsErrorBody {
    #[serde(rename = "type")]
    error_type: String,
    reason: String,
}

/// Top-level Elasticsearch error response envelope.
#[derive(Debug, Serialize, PartialEq, Eq)]
struct EsError {
    error: EsErrorBody,
    status: u16,
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = self.status_code();
        let body = self.to_es_error_body();
        (status, Json(body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    #[test]
    fn index_not_found_maps_to_404() {
        let err = AppError::IndexNotFound("foo".to_string());
        assert_eq!(err.status_code(), StatusCode::NOT_FOUND);
        assert_eq!(err.error_type(), "index_not_found_exception");
        assert_eq!(err.to_string(), "no such index [foo]");
    }

    #[test]
    fn resource_already_exists_maps_to_400() {
        let err = AppError::ResourceAlreadyExists("foo".to_string());
        assert_eq!(err.status_code(), StatusCode::BAD_REQUEST);
        assert_eq!(err.error_type(), "resource_already_exists_exception");
    }

    #[test]
    fn document_not_found_maps_to_404_with_missing_exception_type() {
        let err = AppError::DocumentNotFound {
            index: "foo".to_string(),
            id: "1".to_string(),
        };
        assert_eq!(err.status_code(), StatusCode::NOT_FOUND);
        assert_eq!(err.error_type(), "document_missing_exception");
        assert_eq!(err.to_string(), "document not found: index [foo], id [1]");
    }

    #[test]
    fn invalid_request_maps_to_400() {
        let err = AppError::InvalidRequest("missing field 'mappings'".to_string());
        assert_eq!(err.status_code(), StatusCode::BAD_REQUEST);
        assert_eq!(err.error_type(), "action_request_validation_exception");
    }

    #[test]
    fn internal_maps_to_500() {
        let err = AppError::Internal("lock poisoned".to_string());
        assert_eq!(err.status_code(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(err.error_type(), "internal_server_exception");
    }

    #[tokio::test]
    async fn into_response_produces_correct_status_and_body() {
        let err = AppError::IndexNotFound("foo".to_string());
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["status"], 404);
        assert_eq!(json["error"]["type"], "index_not_found_exception");
        assert_eq!(json["error"]["reason"], "no such index [foo]");
    }
}
```

`#[cfg(test)]` gates the whole module out of the release binary (established in Tutorial 02). The `axum::body::to_bytes` helper reads a `Response` body back out in a test — useful pattern to keep for Tutorial 07's handler integration tests.

### `es-mini/src/main.rs` — wiring it in

Add the module declaration near the top:

```rust
mod error;
```

`AppError` is used within the crate (`error::AppError` or, with a re-export, `crate::AppError`). Add a re-export so call sites elsewhere in the binary don't need the `error::` prefix:

```rust
mod error;
pub use error::AppError;
```

No existing handler in `main.rs` currently returns a `Result` — `root_handler` and `cluster_health_handler` from Tutorial 04 cannot fail. Nothing to change there yet; `AppError` has no call sites in `main.rs` until Tutorial 06 wires up `PUT /{index}` and friends. This tutorial's Definition of Done validates `AppError` in isolation via its unit tests — that's sufficient; wiring a handler that has nothing to fail on would be an unrequested abstraction.

---

## 10. Compiler Says / Fix — Two More Examples

### Forgetting a `match` arm covering a new `AppError` variant

Adding `DocumentNotFound { index, id }` to the enum but forgetting to add it to `status_code()`:

```
error[E0004]: non-exhaustive patterns: `AppError::DocumentNotFound { .. }` not covered
  --> src/error.rs:24:15
   |
24 |         match self {
   |               ^^^^ pattern `AppError::DocumentNotFound { .. }` not covered
```

**Fix:** add the missing arm. This is the compiler doing exactly the job a code reviewer would otherwise have to catch by hand — every new error variant is forced to have an HTTP mapping, at compile time, forever.

### Mismatched error type in `?` with no `From` impl

```rust
fn parse_settings(body: &str) -> Result<Settings, AppError> {
    let value: serde_json::Value = serde_json::from_str(body)?;   // no From<serde_json::Error> for AppError
    // ...
}
```

```
error[E0277]: `?` couldn't convert the error to `AppError`
  --> src/error.rs:40:53
   |
40 |     let value: serde_json::Value = serde_json::from_str(body)?;
   |                                                                ^ the trait `From<serde_json::Error>`
   |                                                                  is not implemented for `AppError`
```

**Fix:** either map explicitly (`.map_err(|e| AppError::InvalidRequest(e.to_string()))?`) or add `#[from]` on a variant that wraps `serde_json::Error` if this conversion is common enough to be worth automating. Explicit mapping is the better default here — `serde_json::Error` on a malformed request body should become `InvalidRequest` (400), but the same error type in a different context might mean something else, so an automatic blanket `#[from]` would be too eager.

---

## 11. What Was Skipped

- **`es-index`-level error type** — deferred until `IndexStore` methods actually need to return `Result` (Tutorial 06/07, once concurrent access via `RwLock` introduces real failure modes like lock poisoning).
- **`root_cause` array in `EsError`** — real ES nests the top-level type/reason inside a `root_cause: [...]` array for multi-shard failure aggregation. Single-node in-memory MVP has no multiple shards to aggregate causes from; add if/when sharding is simulated.
- **`anyhow` dependency** — not added; no call site needs it yet. Add when `main()` grows multi-step fallible setup (config loading, etc.) that benefits from `.context()`.
- **Wiring `AppError` into an actual handler** — no handler returns `Result` yet because none can fail yet. Tutorial 06 is where `AppError::IndexNotFound` and `ResourceAlreadyExists` get their first real call sites.

---

## 12. Definition of Done

1. **`thiserror` declared in workspace and crate dependencies:**
   ```bash
   grep "thiserror" Cargo.toml es-mini/Cargo.toml
   # Cargo.toml:thiserror = { version = "2" }
   # es-mini/Cargo.toml:thiserror = { workspace = true }
   ```

2. **`es-mini/src/error.rs` exists and compiles:**
   ```bash
   cargo check -p es-mini
   # Finished ... without errors
   ```

3. **`mod error;` wired into `main.rs`:**
   ```bash
   grep -E "mod error|pub use error" es-mini/src/main.rs
   # mod error;
   # pub use error::AppError;
   ```

4. **All five `AppError` variants present with correct HTTP mappings (unit tests pass):**
   ```bash
   cargo test -p es-mini error::tests
   # test error::tests::index_not_found_maps_to_404 ... ok
   # test error::tests::resource_already_exists_maps_to_400 ... ok
   # test error::tests::document_not_found_maps_to_404_with_missing_exception_type ... ok
   # test error::tests::invalid_request_maps_to_400 ... ok
   # test error::tests::internal_maps_to_500 ... ok
   # test error::tests::into_response_produces_correct_status_and_body ... ok
   ```

5. **`IndexNotFound` produces the exact ES error shape:**
   ```bash
   cargo test -p es-mini into_response_produces_correct_status_and_body -- --nocapture
   # verifies: status == 404, error.type == "index_not_found_exception", error.reason set
   ```

6. **`DocumentNotFound` uses the real ES exception type name (`document_missing_exception`), not a literal translation of the Rust variant name:**
   ```bash
   grep "document_missing_exception" es-mini/src/error.rs
   ```

7. **Workspace still builds and lints clean:**
   ```bash
   cargo build --workspace
   cargo clippy --workspace --all-targets -- -D warnings
   cargo test --workspace
   # all exit 0
   ```

8. **`anyhow` is NOT yet a dependency (confirms scope discipline for this tutorial):**
   ```bash
   grep -c "^anyhow" Cargo.toml
   # 0
   ```

9. **`#[cfg(test)]` keeps the test module out of the release binary:**
   ```bash
   cargo build -p es-mini --release
   strings target/release/es-mini | grep -c "index_not_found_maps_to_404"
   # 0 — test function names are not compiled into the release binary
   ```

---

**Next:** Tutorial 06 — Shared State in Axum (`Arc`, `Mutex`, `RwLock`). The in-memory `IndexStore` from Tutorial 03 gets wrapped for concurrent access across request handlers — the first real call sites for `AppError::IndexNotFound` and `AppError::ResourceAlreadyExists` follow immediately after in Tutorial 07.
