# Tutorial 04 — Async Rust: Tokio and Axum Basics

**Series:** es-mini — Building Elasticsearch in Rust  
**Phase:** 1 — HTTP contract skeleton  
**Prerequisites:** Tutorials 01–03 complete — workspace builds, CI is green, types in `es-index` are defined and tested.

---

By the end of this tutorial you will have:

- The async mental model mapped to Node.js/Python concepts you already know
- `tokio` as the async runtime wired into `es-mini`
- `axum` serving two endpoints: `GET /` and `GET /_cluster/health`
- Structured logging via `tracing`
- Port configuration from environment variables
- Graceful shutdown on Ctrl+C (container-friendly)

The types defined in Tutorial 03 (`ClusterHealthResponse`, etc.) become the actual HTTP response bodies here. No mocking — these are real, curl-testable JSON responses.

---

## 1. The Async Mental Model

### Promises vs Futures — the conceptual parallel

If you've written async JavaScript, you've used Promises:

```js
// JS: a Promise is created and starts running immediately
const p = fetch("https://example.com");   // already in-flight
await p;                                    // suspend until done
```

Python's asyncio coroutines are similar — `asyncio.create_task(coro())` starts the coroutine immediately.

Rust's `Future` is different in one critical way: **Futures are lazy. They do nothing until polled.**

```rust
// Rust: this does NOT start any I/O yet
let f = some_async_function();    // f is a Future<Output=T>

// This is what actually drives it to completion:
let result = f.await;             // now it runs
```

A `Future` is essentially a state machine — the compiler transforms your `async fn` into a struct that implements the `Future` trait, with a `poll()` method that advances the state machine step by step. When `poll()` returns `Pending`, the runtime suspends the task and wakes it up when progress is possible (e.g., a socket becomes readable). When `poll()` returns `Ready(val)`, the future is done.

You will almost never implement `Future` manually. The `async`/`.await` syntax generates all of that for you. One sentence is enough: **the compiler turns your `async fn` into a state machine struct that the runtime drives by calling `poll()`.**

### The runtime gap

Neither Node.js nor Python make you wire up the event loop yourself — it's always there. Rust ships no built-in async runtime. This is intentional: embedded systems use tiny runtimes; servers use multithreaded runtimes; the language doesn't impose one.

For servers, the ecosystem standard is **Tokio**. Think of it as libuv (Node) or asyncio's event loop (Python), but multithreaded by default and without a GIL.

```
JS:     Node.js process → libuv event loop → V8 callbacks
Python: Python process → asyncio event loop (single thread, GIL)
Rust:   Binary → tokio runtime → N worker threads, no GIL, no GC pauses
```

The zero-cost part: Rust futures compile down to the same state machine code a skilled C developer would write by hand. There's no garbage collector pausing your handlers, no GIL serializing your requests, no VM overhead.

---

## 2. Adding Dependencies

### Workspace-level declarations

In `Cargo.toml` (root workspace file), add to `[workspace.dependencies]`:

```toml
[workspace.dependencies]
# — existing entries —
es-codec  = { path = "es-codec" }
es-query  = { path = "es-query" }
es-index  = { path = "es-index" }
serde     = { version = "1", features = ["derive"] }
serde_json = { version = "1" }

# — new entries for Tutorial 04 —
axum      = { version = "0.8" }
tokio     = { version = "1", features = ["full"] }
tracing   = { version = "0.1" }
tracing-subscriber = { version = "0.3", features = ["env-filter", "json"] }
```

**Feature flags explained:**

`tokio = { version = "1", features = ["full"] }` — Tokio ships a modular set of features: `rt` (the basic scheduler), `rt-multi-thread` (thread pool), `net` (TCP/UDP), `fs` (async file I/O), `io-util`, `macros` (the `#[tokio::main]` macro), `signal` (Ctrl+C handling), and more. `"full"` enables all of them. It's the equivalent of `npm install tokio` and then using everything.

This is like npm optional dependencies or Python's `pip install package[extra]`. The difference: unused Tokio features genuinely don't compile into your binary — feature flags are compile-time switches, not runtime toggles.

For production you'd trim this to `["rt-multi-thread", "net", "macros", "signal"]`, but `"full"` is fine while building.

`tracing-subscriber = { ..., features = ["env-filter", "json"] }` — `env-filter` enables `RUST_LOG=info cargo run` style filtering; `json` enables structured JSON log output.

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
```

Verify the dependency graph builds cleanly before writing any code:

```bash
cargo check -p es-mini
```

---

## 3. `#[tokio::main]` — The Runtime Entry Point

Replace `es-mini/src/main.rs` with the minimal working skeleton:

```rust
#[tokio::main]
async fn main() {
    println!("es-mini starting");
}
```

Run it:

```bash
cargo run -p es-mini
# es-mini starting
```

**What `#[tokio::main]` does:** This is a procedural macro. It transforms:

```rust
#[tokio::main]
async fn main() { ... }
```

into roughly:

```rust
fn main() {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async { ... })   // your async main body runs here
}
```

The real `main()` is still synchronous (Rust's process entry point always is). Tokio builds a thread pool, then `block_on` drives the async main function to completion. Every `await` inside suspends the current task and yields to the scheduler — other tasks can run, I/O can complete, etc.

**Multi-thread vs current-thread:** The default `#[tokio::main]` spawns N worker threads (default: number of logical CPUs). For tests or tools that don't need parallelism:

```rust
#[tokio::main(flavor = "current_thread")]
async fn main() { ... }
```

For `es-mini`, the default multi-thread flavor is correct — you want parallel request handling.

---

## 4. `async fn` and `.await`

Writing an async function:

```rust
async fn fetch_something() -> String {
    // Anything inside an async fn can use .await
    String::from("result")
}
```

Calling it — `.await` is a postfix operator:

```rust
async fn main_logic() {
    let result = fetch_something().await;   // suspend until done
    println!("{}", result);
}
```

Chaining `.await` calls:

```rust
async fn process() -> Result<(), Box<dyn std::error::Error>> {
    let body = read_file("config.json").await?;   // await then ? for errors
    let parsed = parse_json(&body).await?;
    send_to_server(parsed).await?;
    Ok(())
}
```

The `?` operator works inside `async fn` the same as in synchronous code — it unwraps `Ok` or returns `Err` early.

### Spawning background tasks

`tokio::spawn` runs a future concurrently — like `asyncio.create_task()` or `Promise.race()`-style fire-and-forget:

```rust
tokio::spawn(async move {
    // This runs concurrently with whatever spawned it
    do_background_work().await;
});
```

**Why `async move`?** The `move` keyword captures variables from the outer scope by ownership (moves them into the closure), rather than by reference. This is necessary because the spawned task may outlive the scope where it was created.

**Why `Send + 'static`?** Tokio's multi-thread scheduler can move tasks between threads. This means anything the task captures must be safe to send across threads (`Send`) and must not hold references to stack-local data that could be freed before the task finishes (`'static`). The compiler enforces this:

```
error: future cannot be sent between threads safely
   | required by a bound introduced by this call
   | the trait `Send` is not implemented for `Rc<String>`
```

**Fix:** Replace `Rc<T>` with `Arc<T>` (atomic reference counting, thread-safe). For now, axum handles all of this for you — your handlers don't need to think about `Send + 'static` directly.

---

## 5. Adding Tracing

Tracing replaces `println!` for production server code. Think of it as `pino` or `winston` (Node) or Python's `structlog` — structured, leveled, filterable log output.

Update `main.rs`:

```rust
use tracing::info;

#[tokio::main]
async fn main() {
    // Initialize the subscriber (log sink) first, before any logging calls
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "es_mini=info".into()),
        )
        .init();

    info!("es-mini starting");
}
```

Run it:

```bash
cargo run -p es-mini
# 2025-01-15T10:30:00.000000Z  INFO es_mini: es-mini starting
```

Control log levels with `RUST_LOG`:

```bash
RUST_LOG=debug cargo run -p es-mini    # verbose
RUST_LOG=warn  cargo run -p es-mini    # only warnings and errors
RUST_LOG=es_mini=info,axum=warn cargo run -p es-mini   # per-crate levels
```

**Tracing macros:**

```rust
info!("server listening on {}", addr);
warn!("index {} already exists, skipping", name);
error!("failed to bind: {}", err);
debug!("request headers: {:?}", headers);  // debug only in dev
```

**Structured key-value fields:**

```rust
info!(port = 9200, cluster_name = "es-mini", "server ready");
// Output: ... INFO es_mini: server ready port=9200 cluster_name="es-mini"
```

**JSON output for production** (e.g., forwarding to Loki/Datadog):

```rust
tracing_subscriber::fmt()
    .json()   // emit newline-delimited JSON instead of human-readable text
    .with_env_filter(...)
    .init();
```

Toggle this with an env var later. For now, human-readable text is easier during development.

**`#[instrument]` — automatic span tracing for functions:**

```rust
use tracing::instrument;

#[instrument]
async fn my_handler(name: String) -> String {
    // Every call creates a span: function name, arguments, duration
    info!("handling request");
    name
}
```

This is the async equivalent of adding a manual `span` in OpenTelemetry. You'll use `#[instrument]` more in later tutorials when tracing request flows through multiple functions.

---

## 6. The Axum Router

Axum's API is built on three concepts:

1. **Handlers** — async functions that take typed extractors and return a response
2. **Router** — maps paths+methods to handlers
3. **Extractors** — pull data out of the request (path params, query params, JSON body, shared state)

Think of it as Express.js but type-safe: instead of `(req, res, next) => {}` where `req.params.id` could be anything, axum gives you `Path<String>` or `Path<u64>` and the type system guarantees it or rejects the request before your code runs.

Expand `main.rs` with the router:

```rust
use axum::{routing::get, Router};
use tracing::info;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "es_mini=info".into()),
        )
        .init();

    let app = Router::new()
        .route("/", get(root_handler))
        .route("/_cluster/health", get(cluster_health_handler));

    let addr = "127.0.0.1:9200";
    info!("listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn root_handler() -> &'static str {
    "hello"
}

async fn cluster_health_handler() -> &'static str {
    "health"
}
```

Test it:

```bash
cargo run -p es-mini &
curl http://localhost:9200/
# hello
curl http://localhost:9200/_cluster/health
# health
kill %1
```

**Routing API:**

```rust
Router::new()
    .route("/path", get(handler))           // GET /path
    .route("/path", post(handler))          // POST /path
    .route("/path", put(handler))           // PUT /path
    .route("/path", delete(handler))        // DELETE /path
    .route("/path", get(g).post(p))         // GET and POST on same path
```

Import the method functions:
```rust
use axum::routing::{delete, get, post, put};
```

---

## 7. Proper JSON Responses

Replace the placeholder handlers with real ES-shaped responses. First, add the serde import and define a response struct for `GET /`:

```rust
use axum::{http::StatusCode, response::IntoResponse, routing::get, Json, Router};
use serde::Serialize;
use tracing::info;

// ── Response types ────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct VersionInfo {
    number: String,
    build_flavor: String,
    build_type: String,
    lucene_version: String,
}

#[derive(Serialize)]
struct RootResponse {
    name: String,
    cluster_name: String,
    cluster_uuid: String,
    version: VersionInfo,
    tagline: String,
}
```

Axum's `Json<T>` extractor/responder is the key connector: as a return value it serializes `T` to JSON and sets `Content-Type: application/json`. As a function parameter it deserializes the request body. The type system ensures you can't accidentally return malformed JSON — if `T: Serialize`, `Json<T>` works; if it doesn't, compile error.

The return type `impl IntoResponse` is a trait bound that means "anything axum knows how to turn into an HTTP response." A tuple `(StatusCode, Json<T>)` implements it — status code plus body:

```rust
async fn root_handler() -> impl IntoResponse {
    let resp = RootResponse {
        name: String::from("es-mini"),
        cluster_name: String::from("es-mini"),
        cluster_uuid: String::from("_na_"),
        version: VersionInfo {
            number: String::from("9.0.0"),
            build_flavor: String::from("default"),
            build_type: String::from("tar"),
            lucene_version: String::from("10.1.0"),
        },
        tagline: String::from("You Know, for Search"),
    };
    (StatusCode::OK, Json(resp))
}
```

Test it:

```bash
curl -s http://localhost:9200/ | jq .
```

Expected output:

```json
{
  "name": "es-mini",
  "cluster_name": "es-mini",
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

This matches the real `GET /` response from Elasticsearch 9.x (minus some additional fields that we'll add as the project grows).

---

## 8. Shared State with `State<T>`

The cluster name is currently hardcoded. Any data that needs to be shared across handlers lives in axum's `State`. This is how you'd share a database connection pool, configuration, or (in Tutorial 06) the in-memory index store.

Define a state struct and wire it in:

```rust
use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use std::sync::Arc;

#[derive(Clone)]           // ← State<T> requires T: Clone
struct AppState {
    cluster_name: String,
}
```

**Why `Clone`?** Axum clones the state for each handler invocation. This is cheap when you wrap heap-allocated data in `Arc` — cloning an `Arc` just increments a reference count, it doesn't copy the underlying data. For `AppState` as written (a single `String`), the clone copies the string — which is fine at this scale. When you add `IndexStore` in Tutorial 06, you'll wrap it in `Arc<RwLock<IndexStore>>` so cloning stays cheap.

**The compiler error you'll see if you forget `Clone`:**

```
error[E0277]: the trait bound `AppState: Clone` is not satisfied
  --> src/main.rs:42:6
   |
42 |         .with_state(state)
   |          ^^^^^^^^^^ the trait `Clone` is not implemented for `AppState`
   |
   = note: required by `axum::routing::Router::<S>::with_state`
```

Fix: `#[derive(Clone)]` on `AppState`.

Wire state into the router:

```rust
let state = Arc::new(AppState {
    cluster_name: String::from("es-mini"),
});

let app = Router::new()
    .route("/", get(root_handler))
    .route("/_cluster/health", get(cluster_health_handler))
    .with_state(state);   // ← binds state to this router
```

Extract state in a handler:

```rust
async fn root_handler(
    State(state): State<Arc<AppState>>,    // destructure with pattern matching
) -> impl IntoResponse {
    let resp = RootResponse {
        name: String::from("es-mini"),
        cluster_name: state.cluster_name.clone(),
        cluster_uuid: String::from("_na_"),
        // ...
    };
    (StatusCode::OK, Json(resp))
}
```

**The extractor pattern:** `State(state): State<Arc<AppState>>` is Rust pattern matching in a function argument. `State<T>` is a newtype wrapper; `State(inner)` destructures it in place so you get `state: Arc<AppState>` directly without an extra `let state = raw_state.0;` line. You'll see this pattern with all axum extractors.

---

## 9. The `/_cluster/health` Endpoint

Now use the `ClusterHealthResponse` from Tutorial 03:

```rust
use es_index::ClusterHealthResponse;

async fn cluster_health_handler(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let health = ClusterHealthResponse::healthy_empty(&state.cluster_name);
    (StatusCode::OK, Json(health))
}
```

That's it. The types from Tutorial 03 are the response bodies — no translation layer, no manual JSON construction.

Test it:

```bash
curl -s http://localhost:9200/_cluster/health | jq .
```

Expected output:

```json
{
  "cluster_name": "es-mini",
  "status": "green",
  "timed_out": false,
  "number_of_nodes": 1,
  "number_of_data_nodes": 1,
  "active_primary_shards": 0,
  "active_shards": 0,
  "relocating_shards": 0,
  "initializing_shards": 0,
  "unassigned_shards": 0,
  "delayed_unassigned_shards": 0,
  "number_of_pending_tasks": 0,
  "number_of_in_flight_fetch": 0,
  "task_max_waiting_in_queue_millis": 0,
  "active_shards_percent_as_number": 100.0
}
```

Verify the status code:

```bash
curl -s -o /dev/null -w "%{http_code}" http://localhost:9200/_cluster/health
# 200
```

---

## 10. Environment-Based Configuration

`std::env::var` is the standard library's interface to environment variables — no external crate needed:

```rust
use std::env;

let port = env::var("ES_PORT").unwrap_or_else(|_| String::from("9200"));
let host = env::var("ES_HOST").unwrap_or_else(|_| String::from("127.0.0.1"));
let cluster_name = env::var("CLUSTER_NAME").unwrap_or_else(|_| String::from("es-mini"));

let addr = format!("{}:{}", host, port);
```

`unwrap_or_else(|_| ...)` is the lazy variant of `unwrap_or` — the closure only runs if the variable is absent. Slightly more efficient when the default requires allocation.

Test it:

```bash
ES_PORT=9201 cargo run -p es-mini &
curl http://localhost:9201/
# response from the new port
kill %1
```

---

## 11. Graceful Shutdown

Kubernetes sends `SIGTERM` before killing a pod. Docker Compose sends it on `Ctrl+C`. Without graceful shutdown, in-flight requests get cut off mid-response. This is the part you've dealt with at the infrastructure level (preStop hooks, terminationGracePeriodSeconds) — here's how to wire it at the application level in Rust.

Axum 0.8's `serve()` returns a builder that accepts a shutdown signal future:

```rust
async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("failed to install Ctrl+C handler");
    // ponytail: SIGTERM handling not added — add when running in Kubernetes
}
```

Wire it in:

```rust
axum::serve(listener, app)
    .with_graceful_shutdown(shutdown_signal())
    .await
    .unwrap();
```

When `shutdown_signal()` resolves (i.e., Ctrl+C is pressed), axum stops accepting new connections and waits for in-flight requests to complete before returning. Then `main()` exits cleanly.

Test it:

```bash
cargo run -p es-mini &
# (send a request in the background to show graceful drain)
curl http://localhost:9200/ &
# Press Ctrl+C — or send SIGINT to the background process (only SIGINT is wired up so far)
kill -INT %1
# The server completes in-flight requests and exits cleanly
```

For Kubernetes, add `SIGTERM` handling alongside `SIGINT`:

```rust
use tokio::signal::unix::{signal, SignalKind};

async fn shutdown_signal() {
    let ctrl_c = async { tokio::signal::ctrl_c().await.unwrap() };
    let terminate = async {
        signal(SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
```

`tokio::select!` races multiple futures — first one to complete wins. This is the Rust equivalent of `Promise.race()`.

---

## 12. The Complete `main.rs`

Put it all together. This is the full working file:

```rust
//! es-mini HTTP server — Phase 1 skeleton.

#![deny(clippy::all)]

use std::{env, sync::Arc};

use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use es_index::ClusterHealthResponse;
use serde::Serialize;
use tracing::info;

// ── Shared state ──────────────────────────────────────────────────────────────

/// Application-wide state shared across all request handlers.
///
/// Wrapped in `Arc` so cloning is cheap (increments a reference count,
/// does not copy the data). Tutorial 06 adds `Arc<RwLock<IndexStore>>` here.
#[derive(Clone)]
struct AppState {
    cluster_name: String,
}

// ── Response types ────────────────────────────────────────────────────────────

/// Version info nested in the root `GET /` response.
#[derive(Serialize)]
struct VersionInfo {
    number: String,
    build_flavor: String,
    build_type: String,
    lucene_version: String,
}

/// `GET /` response — matches Elasticsearch 9.x node info shape.
#[derive(Serialize)]
struct RootResponse {
    name: String,
    cluster_name: String,
    cluster_uuid: String,
    version: VersionInfo,
    tagline: String,
}

// ── Handlers ──────────────────────────────────────────────────────────────────

/// `GET /` — node info, analogous to Elasticsearch's root endpoint.
async fn root_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let resp = RootResponse {
        name: String::from("es-mini"),
        cluster_name: state.cluster_name.clone(),
        cluster_uuid: String::from("_na_"),
        version: VersionInfo {
            number: String::from("9.0.0"),
            build_flavor: String::from("default"),
            build_type: String::from("tar"),
            lucene_version: String::from("10.1.0"),
        },
        tagline: String::from("You Know, for Search"),
    };
    (StatusCode::OK, Json(resp))
}

/// `GET /_cluster/health` — cluster health snapshot.
async fn cluster_health_handler(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let health = ClusterHealthResponse::healthy_empty(&state.cluster_name);
    (StatusCode::OK, Json(health))
}

// ── Shutdown ──────────────────────────────────────────────────────────────────

/// Resolves when Ctrl+C is received. Passed to axum's graceful shutdown.
// ponytail: SIGTERM not handled — add signal(SignalKind::terminate()) when deploying to k8s
async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("failed to install Ctrl+C handler");
    info!("shutdown signal received");
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    // Logging — reads RUST_LOG env var; defaults to info for this crate
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "es_mini=info".into()),
        )
        .init();

    // Configuration from environment
    let host = env::var("ES_HOST").unwrap_or_else(|_| String::from("127.0.0.1"));
    let port = env::var("ES_PORT").unwrap_or_else(|_| String::from("9200"));
    let cluster_name =
        env::var("CLUSTER_NAME").unwrap_or_else(|_| String::from("es-mini"));

    let addr = format!("{}:{}", host, port);

    let state = Arc::new(AppState { cluster_name });

    let app = Router::new()
        .route("/", get(root_handler))
        .route("/_cluster/health", get(cluster_health_handler))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| panic!("failed to bind {}: {}", addr, e));

    info!("es-mini listening on http://{}", addr);

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("server error");
}
```

Build and run:

```bash
cargo build -p es-mini
cargo run -p es-mini
```

Expected startup output:

```
2025-01-15T10:30:00.000000Z  INFO es_mini: es-mini listening on http://127.0.0.1:9200
```

---

## 13. End-to-End Test

```bash
# Start the server
cargo run -p es-mini &
SERVER_PID=$!

# GET / — node info
curl -s http://localhost:9200/ | jq '{name, cluster_name, "version_number": .version.number}'
# {
#   "name": "es-mini",
#   "cluster_name": "es-mini",
#   "version_number": "9.0.0"
# }

# GET /_cluster/health
curl -s http://localhost:9200/_cluster/health | jq '{cluster_name, status, number_of_nodes}'
# {
#   "cluster_name": "es-mini",
#   "status": "green",
#   "number_of_nodes": 1
# }

# Verify status codes
curl -s -o /dev/null -w "GET /: %{http_code}\n" http://localhost:9200/
curl -s -o /dev/null -w "GET /_cluster/health: %{http_code}\n" http://localhost:9200/_cluster/health
# GET /: 200
# GET /_cluster/health: 200

# Unknown route — axum returns 404 automatically
curl -s -o /dev/null -w "GET /nonexistent: %{http_code}\n" http://localhost:9200/nonexistent
# GET /nonexistent: 404

# Custom port
ES_PORT=9201 cargo run -p es-mini &
curl -s http://localhost:9201/ | jq .name
# "es-mini"

# Shutdown
kill -INT $SERVER_PID  # plain `kill` sends SIGTERM, which isn't handled yet
# 2025-01-15T10:30:05.000000Z  INFO es_mini: shutdown signal received
```

---

## 14. Compiler Errors You'll Encounter

### Missing `Clone` on State

```
error[E0277]: the trait bound `AppState: Clone` is not satisfied
  --> src/main.rs:60:6
   |
60 |         .with_state(state)
   |          ^^^^^^^^^^ the trait `Clone` is not implemented for `AppState`
```

**Fix:** Add `#[derive(Clone)]` to `AppState`.

### Mismatched State type in handler

If the handler's `State<T>` type doesn't match what `.with_state()` provides:

```
error[E0277]: the trait bound `fn(State<AppState>) ... : Handler<_, _>` is not satisfied
note: `State<AppState>` can only be used as an extractor if the state is `AppState`
      but the state is `Arc<AppState>`
```

**Fix:** Make the handler's `State<Arc<AppState>>` match exactly what `.with_state(Arc::new(...))` provides. The types must match exactly — axum uses the type to thread state through.

### Missing `Send` on spawned task

```
error: future cannot be sent between threads safely
  --> src/main.rs:82:18
   |
   = help: within `impl Future<Output = ()>`, the trait `Send` is not implemented
             for `Rc<String>`
```

**Fix:** Replace `Rc<T>` with `Arc<T>`. `Rc` (reference counting) is single-threaded; `Arc` (atomic reference counting) is multi-thread safe. When you put data in `tokio::spawn`, it must be `Send`.

### Axum handler signature error

```
error[E0277]: the trait bound `fn() -> ... : Handler<_, _>` is not satisfied
```

This is axum telling you the function signature doesn't match what it expects for a handler. Common causes:
- A parameter type that isn't a valid extractor
- A return type that doesn't implement `IntoResponse`
- Taking `State<T>` without calling `.with_state()`

Add `#[axum::debug_handler]` above the function to get better diagnostics:

```rust
#[axum::debug_handler]
async fn broken_handler(State(s): State<Arc<AppState>>) -> impl IntoResponse {
    // ...
}
```

This attribute expands the error message significantly and usually points directly at the problem.

---

## 15. What Was Skipped

**Tower middleware** — axum is built on Tower, a service abstraction for Rust. You can layer in request logging, rate limiting, compression, auth via Tower middleware. Deferred until there's a concrete need.

**Error handling middleware** — right now, handler panics return a 500. Tutorial 05 covers building a proper `EsError` type and mapping it to correct HTTP status codes.

**Request body extraction** — `Json<T>` as an extractor (request body → typed struct) is shown conceptually but not exercised until Tutorial 07 (index management endpoints, which take `PUT /index-name` with a JSON settings body).

**Testing handlers directly** — axum handlers are just async functions; you can call them in tests without a running server. Deferred to Tutorial 07 where the tests become non-trivial.

**SIGTERM** — noted with a `ponytail:` comment. Add `tokio::signal::unix::signal(SignalKind::terminate())` before deploying to Kubernetes.

---

## 16. Definition of Done

Complete this tutorial when all of the following pass:

1. **Dependencies declared in workspace `Cargo.toml`:**
   ```bash
   grep -E "^(axum|tokio|tracing)" Cargo.toml
   # axum = ...
   # tokio = ...
   # tracing = ...
   # tracing-subscriber = ...
   ```

2. **`es-mini/Cargo.toml` references them with `workspace = true`:**
   ```bash
   grep "workspace = true" es-mini/Cargo.toml | wc -l
   # at least 6 lines
   ```

3. **Binary compiles and starts:**
   ```bash
   cargo build -p es-mini
   # Finished ... without errors or warnings
   ```

4. **`GET /` returns 200 with correct JSON shape:**
   ```bash
   curl -sf http://localhost:9200/ | jq -e '.version.number'
   # "9.0.0"
   curl -sf http://localhost:9200/ | jq -e '.tagline'
   # "You Know, for Search"
   ```

5. **`GET /_cluster/health` returns 200 with all required fields:**
   ```bash
   curl -sf http://localhost:9200/_cluster/health | jq -e '.status'
   # "green"
   curl -sf http://localhost:9200/_cluster/health | jq -e '.number_of_nodes'
   # 1
   ```

6. **Unknown routes return 404:**
   ```bash
   curl -s -o /dev/null -w "%{http_code}" http://localhost:9200/doesnotexist
   # 404
   ```

7. **Port is configurable via environment:**
   ```bash
   ES_PORT=9201 cargo run -p es-mini &
   curl -sf http://localhost:9201/_cluster/health | jq -e '.status'
   # "green"
   kill %1
   ```

8. **Tracing output appears on startup:**
   ```bash
   cargo run -p es-mini 2>&1 | grep "listening"
   # ... INFO es_mini: es-mini listening on http://127.0.0.1:9200
   ```

9. **`RUST_LOG` controls log verbosity:**
   ```bash
   RUST_LOG=debug cargo run -p es-mini 2>&1 | head -5
   # Debug-level output (no tower-http layer is added in this tutorial, so this is just crate-level verbosity)
   ```

10. **Full workspace checks pass clean:**
    ```bash
    cargo test --workspace
    cargo clippy --workspace --all-targets -- -D warnings
    # Both exit 0
    ```

---

**Next:** Tutorial 05 — Error Handling the Rust Way. Right now, panics and `.unwrap()` calls produce 500s with no useful body. Tutorial 05 builds a proper `EsError` enum, implements `IntoResponse` for it, and maps error cases (index not found → 404, already exists → 400) to ES-compatible error shapes like `{"error": {"type": "index_not_found_exception", "reason": "..."}, "status": 404}`.
