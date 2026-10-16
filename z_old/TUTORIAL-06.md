# Tutorial 06 — Shared State in Axum: Arc, Mutex, and RwLock

**Series:** es-mini — Building Elasticsearch in Rust
**Phase:** 1 — HTTP contract skeleton (infrastructure)
**Prerequisites:** Tutorials 01–05 complete — workspace builds, `es-index::IndexStore` exists (Tutorial 03), axum server serves `GET /` and `GET /_cluster/health` with `AppState` (Tutorial 04), `AppError`/`IntoResponse` exists (Tutorial 05).

---

By the end of this tutorial you will have:

- The mental model for why Rust won't let you share mutable state across threads by accident
- `Arc<T>` — the thread-safe multi-owner pointer
- `Mutex<T>` vs `RwLock<T>` — exclusive lock vs reader/writer lock, and lock poisoning
- `AppState` expanded to hold `Arc<RwLock<IndexStore>>`
- A new `GET /_cat/indices` endpoint that reads through the lock
- The pattern every Tutorial 07+ handler follows to touch the store safely

---

## 1. The Problem: Why Can't Handlers Just Share `IndexStore`?

Every HTTP request in axum is handled on its own tokio task, potentially on a different OS thread each time (multi-thread runtime, Tutorial 04 §1). `PUT /my-index` needs to **write** to the index catalog. `GET /_cat/indices` needs to **read** it. Both need to reach the *same* `IndexStore` — not a copy each.

In JS this is a non-issue: one thread, one event loop, `let store = new IndexStore()` closed over by every route handler, done. Python's GIL means only one thread runs Python bytecode at a time — but the GIL doesn't prevent logic races (two coroutines can still interleave a read-modify-write on a shared `dict` incorrectly; asyncio just makes the interleaving points explicit at `await`).

Rust's ownership rules make the naive approach **not compile at all**, rather than compile and misbehave at 3am:

```rust
struct AppState {
    store: IndexStore,   // owned directly, no wrapper
}

async fn handler_a(State(state): State<AppState>) { /* wants &mut state.store */ }
async fn handler_b(State(state): State<AppState>) { /* also wants &mut state.store */ }
```

`State<T>` requires `T: Clone` (axum clones state into every handler invocation, Tutorial 04 §8). Cloning `IndexStore` gives each handler its **own independent copy** — writes in `handler_a` would vanish, invisible to `handler_b`. That's not a compile error, it's a silent correctness bug: the exact "everyone edited their own copy of the spreadsheet" class of problem.

The fix isn't "clone less carefully" — it's "own the data once, share *access* to it." That's what `Arc` + `Mutex`/`RwLock` are for.

---

## 2. `Arc<T>` — Atomically Reference Counted Pointer

`Arc<T>` ("Atomic Reference Counted") is a heap-allocated, shared-ownership smart pointer. `Arc::clone()` doesn't copy `T` — it increments an atomic counter and hands back a new pointer to the *same* heap allocation. The value is dropped only when the last `Arc` pointing at it is dropped (count hits zero).

Closest analogies you already know:

- **C++ `std::shared_ptr`** — same refcounting idea, `Arc` is the thread-safe version.
- **Python object refcounting** — CPython refcounts every object already; `Arc::clone()` is doing explicitly, and only for the values you choose, what CPython does implicitly for everything.
- **JS** — no analogue needed; garbage-collected, multiple references to one object are just... references. `Arc` is what you reach for in a language *without* a GC to get that same "many owners, one value" property, safely, across threads.

```rust
use std::sync::Arc;

let store = Arc::new(IndexStore::new());
let handle_1 = Arc::clone(&store);   // same data, count now 2
let handle_2 = store.clone();        // equivalent, count now 3

// all three point at the same IndexStore on the heap
```

`Arc::clone(&store)` and `store.clone()` are identical — the explicit form is preferred in Rust codebases specifically to make it visually obvious at the call site that this is a cheap refcount bump, not a deep copy of `T`.

**`Arc` vs `Rc`:** `Rc<T>` is the single-threaded version — same API, cheaper (a plain, non-atomic counter increment). The compiler enforces the boundary: try to send an `Rc<T>` into `tokio::spawn` or across an axum handler and you get exactly the `Send` error already seen in Tutorial 04 §4:

```
error: future cannot be sent between threads safely
   = help: within `impl Future<Output = ()>`, the trait `Send` is not implemented for `Rc<IndexStore>`
```

Rule of thumb: default to `Arc` in anything touching `tokio`/axum. Reach for `Rc` only in genuinely single-threaded code (a CLI tool, a parser) where the atomic overhead is pure waste.

**What `Arc` does *not* give you:** shared **mutable** access. `Arc<T>` only hands out `&T` (shared references) via `Deref` — you still can't get a `&mut T` out of it, because multiple threads could then write concurrently, which is exactly the data race Rust's borrow checker exists to prevent. `Arc` solves "many owners." It doesn't solve "safe mutation." That's the job of `Mutex`/`RwLock`, wrapped *inside* the `Arc`.

---

## 3. `Mutex<T>` and `RwLock<T>`

### `Mutex<T>` — exclusive access

`std::sync::Mutex<T>` wraps a value and hands out access through a lock:

```rust
use std::sync::Mutex;

let counter = Mutex::new(0);

{
    let mut guard = counter.lock().unwrap();   // blocks until lock is free
    *guard += 1;
}   // <- guard dropped here, lock released automatically
```

Analogy: a database row-level `SELECT ... FOR UPDATE` — one transaction holds it, everyone else queues. Only one thread may hold the lock at a time, whether it wants to read or write. `.lock()` blocks the calling thread until the lock is available, then returns a `MutexGuard<T>` — a smart pointer that derefs to `&mut T` and, critically, **releases the lock when it goes out of scope** (Rust's RAII: Resource Acquisition Is Initialization — cleanup tied to a value's lifetime, not to a manual `unlock()` call you might forget). No `try/finally` needed, no equivalent of a Python context manager to remember to write — the compiler-generated `Drop` does it.

### `RwLock<T>` — many readers, one writer

`std::sync::RwLock<T>` is the finer-grained sibling: any number of concurrent readers, *or* exactly one exclusive writer, never both at once.

```rust
use std::sync::RwLock;

let store = RwLock::new(IndexStore::new());

{
    let guard = store.read().unwrap();     // shared — other readers can also .read() right now
    println!("{} indices", guard.len());
}   // <- read guard dropped, lock released

{
    let mut guard = store.write().unwrap(); // exclusive — blocks until all readers/writers finish
    guard.create(/* ... */);
}   // <- write guard dropped, lock released
```

Analogy: a database shared (`S`) vs exclusive (`X`) lock — many transactions can hold `S` concurrently for reads; `X` waits for all of them to release first. For a search index, most traffic is `GET`/`_search` (reads) with occasional `PUT`/index-management writes — exactly the read-heavy shape `RwLock` is built for. `Mutex` would serialize *every* read behind every other read, for no reason; `RwLock` lets `_cat/indices` and `_cluster/health` run concurrently while nobody is writing.

### Lock poisoning

If a thread panics **while holding the lock**, the lock is marked *poisoned*. Every subsequent `.lock()` / `.read()` / `.write()` call returns `Err(PoisonError<T>)` instead of blocking forever or silently handing out a guard — the reasoning being "a panic mid-mutation might have left the data in a half-updated, inconsistent state; don't let anyone touch it without acknowledging that."

```rust
let guard = store.read();   // Result<RwLockReadGuard<T>, PoisonError<RwLockReadGuard<T>>>
```

Two accepted ways to deal with it:

```rust
// Tests: crash loudly, exactly what you want when an assertion-driving lock is broken
let guard = store.read().unwrap();

// Production handler code: recover the guard anyway
// (accepting "the data might be inconsistent" is a judgment call the caller
// is explicitly making, not a silent swallow — the .into_inner() name says so)
let guard = store.read().unwrap_or_else(|poisoned| poisoned.into_inner());
```

`PoisonError::into_inner()` hands back the guard regardless — Rust doesn't force recovery, it forces *acknowledgment*. For `IndexStore` (in-memory, no external durability at stake), recovering with `into_inner()` in handlers is the pragmatic choice: a panic in one handler shouldn't wedge every other handler behind a permanently-poisoned lock for the life of the process. If a handler ever panics mid-write in a way that leaves `IndexStore` truly corrupt, that's a bug to fix at the source (Tutorial 05's `AppError` — turn the panic into a proper `Result` instead), not something a poisoned-lock policy should paper over indefinitely.

---

## 4. `Arc<RwLock<IndexStore>>` — The Pattern

Combine both: `Arc` for multi-owner sharing across handler tasks, `RwLock` for safe concurrent read/write access to the one `IndexStore` underneath.

```
                         Arc<RwLock<IndexStore>>
                                  │
                     ┌────────────┴────────────┐
                     │   heap allocation:        │
                     │   refcount + RwLock +     │
                     │   IndexStore { .. }       │
                     └────────────┬────────────┘
                                  │  (Arc::clone — cheap, refcount++)
        ┌─────────────┬──────────┼──────────┬─────────────┐
        │              │          │           │             │
  GET /_cat/indices  GET /idx  PUT /idx   GET /health   DELETE /idx
   (task, .read())  (.read()) (.write()) (no store access) (.write())
        │              │          │           │             │
        └──────────────┴────read lock, concurrent OK─────────┘
                       (write lock: exclusive, blocks all above)
```

Every handler receives its own clone of the outer `Arc` (cheap — refcount bump, no `IndexStore` copy). Reads (`.read()`) can run concurrently across multiple in-flight requests. A write (`.write()`) waits for in-flight reads to finish, then blocks out everyone else until it completes and its guard drops.

`.clone()` on `Arc<RwLock<IndexStore>>` is what makes `#[derive(Clone)]` on `AppState` cheap even once it holds real, meaningfully-sized data — this is the payoff of the `Clone` requirement introduced back in Tutorial 04 §8.

### The common deadlock: guard held too long

```rust
async fn broken_handler(State(state): State<AppState>) -> impl IntoResponse {
    let mut store = state.store.write().unwrap_or_else(|p| p.into_inner());
    store.create(/* ... */);

    let read_guard = state.store.read().unwrap();   // <- DEADLOCK: write guard still held above
    // ...
}
```

`std::sync::RwLock` is **not reentrant** — the same task trying to acquire a second lock (even a read lock) while it still holds the write lock from the same `RwLock` will block forever, waiting on itself. This is not a compiler-caught mistake (unlike the ownership issues in §5) — it's a runtime hang. Fix: end the scope holding `store` (let it drop) before acquiring a new lock, or do all needed work under one guard:

```rust
async fn fixed_handler(State(state): State<AppState>) -> impl IntoResponse {
    {
        let mut store = state.store.write().unwrap_or_else(|p| p.into_inner());
        store.create(/* ... */);
    } // <- write guard dropped here

    let store = state.store.read().unwrap_or_else(|p| p.into_inner());
    // ...
}
```

An explicit `{ }` scope block is the idiomatic way to force a guard to drop before the next statement that needs a fresh lock — `drop(store)` also works but the block reads better when the point is "this section of code needs the lock, then doesn't."

---

## 5. This Won't Compile: The Borrow Checker Catching a Threading Mistake

Before reaching for `Arc`, it's worth seeing the compiler actually refuse a shared-mutable-state bug outright, so `Arc`/`RwLock` stop looking like ceremony and start looking like the *only* way past this wall.

```rust
use std::thread;

fn main() {
    let mut store = IndexStore::new();

    let t1 = thread::spawn(|| {
        store.create(/* ... */);   // wants &mut store
    });
    let t2 = thread::spawn(|| {
        store.create(/* ... */);   // also wants &mut store
    });

    t1.join().unwrap();
    t2.join().unwrap();
}
```

**Compiler says:**

```
error[E0499]: cannot borrow `store` as mutable more than once at a time
  --> src/main.rs:8:31
   |
6  |     let t1 = thread::spawn(|| {
   |                             -- first mutable borrow occurs here
7  |         store.create(/* ... */);
   |         ----- first borrow used here
8  |     let t2 = thread::spawn(|| {
   |                             ^^ second mutable borrow occurs here
9  |         store.create(/* ... */);
   |         ----- second borrow used here
```

No thread ever actually ran yet — this is caught at **compile time**, before the program exists as a binary. There is no code path in JS or Python that produces an equivalent error; the closest thing is a race condition discovered later, in production, under load, at 3am. Rust rejects it before `cargo build` finishes.

**Fix:** wrap in `Arc<Mutex<IndexStore>>` (or `RwLock`) and clone the `Arc` per thread — this is the *only* way the borrow checker will let two threads touch the same `IndexStore`:

```rust
use std::sync::{Arc, Mutex};
use std::thread;

fn main() {
    let store = Arc::new(Mutex::new(IndexStore::new()));

    let store_1 = Arc::clone(&store);
    let t1 = thread::spawn(move || {
        store_1.lock().unwrap().create(/* ... */);
    });

    let store_2 = Arc::clone(&store);
    let t2 = thread::spawn(move || {
        store_2.lock().unwrap().create(/* ... */);
    });

    t1.join().unwrap();
    t2.join().unwrap();
}
```

Compiles clean. Each thread gets its own `Arc` handle (`move` captures it by ownership into the closure, Tutorial 04 §4), and the `Mutex` — not the borrow checker — is what serializes the actual mutation at runtime.

---

## 6. `std::sync::RwLock` vs `tokio::sync::RwLock`

Tokio ships its own `tokio::sync::RwLock`/`Mutex` — async-aware versions where `.read().await` / `.write().await` yield to the scheduler instead of blocking the OS thread.

**Rule for choosing:** if you ever hold the guard across an `.await` point, use `tokio::sync`. If you acquire the lock, do synchronous work only, and drop the guard before any `.await`, use `std::sync` — it's simpler, has no `.await`, and is measurably faster (no async machinery, no scheduler yield/wake overhead).

```rust
// BAD with std::sync::RwLock — holding a std guard across .await
// blocks the OS thread while waiting on I/O, defeating the point of async,
// and can deadlock the whole tokio runtime under a single-threaded flavor.
let guard = store.read().unwrap();
some_async_operation().await;   // <- guard still held here, blocking the thread
drop(guard);
```

```rust
// FINE with std::sync::RwLock — no .await while the guard is alive
let count = {
    let guard = store.read().unwrap_or_else(|p| p.into_inner());
    guard.len()
};   // guard dropped before any async work
some_async_operation().await;
```

Every `IndexStore` operation in es-mini is a synchronous, in-memory `HashMap`/`BTreeMap` lookup — nothing ever `.await`s while the lock is held. **`std::sync::RwLock` is the right choice for es-mini**, now and through the in-memory phase of the project (Phase 1–3 in `EXECUTION.md`). Revisit if/when the on-disk Lucene codec (Phase 4+) introduces actual async I/O that needs to happen *while* holding a lock on shared index state — that's the point where `tokio::sync::RwLock` earns its overhead.

---

## 7. Wiring It In: `AppState`, `main.rs`, `Cargo.toml`

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

`es-query` and `es-index` were both already declared back in Tutorial 04's dependency list (`es-index` is how `ClusterHealthResponse` got imported; `es-query` has no call sites yet — its first one is Tutorial 11's `_search` endpoint) — no new dependency lines needed here, called out explicitly since this tutorial is the first one that pulls a second type (`IndexStore`) from `es-index`.

### `AppState` — expanded, and the outer `Arc` dropped

Tutorial 04 wrapped the whole state struct in an outer `Arc<AppState>` because `State<T>` requires `Clone`, and back then `AppState` held a bare `String`. Now that `AppState` holds `Arc<RwLock<IndexStore>>` — already cheap to clone — the *struct itself* is cheap to clone (a `String` clone plus an `Arc` refcount bump), so the extra outer `Arc` wrapper is redundant. Drop it:

```rust
use std::sync::{Arc, RwLock};
use es_index::IndexStore;

/// Application-wide state shared across all request handlers.
///
/// `#[derive(Clone)]` works cheaply here because every field is cheap to
/// clone: `cluster_name` is a small String, and `store` is an `Arc` (a
/// refcount bump, not a deep copy of `IndexStore`). No outer `Arc<AppState>`
/// wrapper needed — see Tutorial 04 §8 for why one was used before this.
#[derive(Clone)]
struct AppState {
    cluster_name: String,
    store: Arc<RwLock<IndexStore>>,
}
```

Handler signatures change from `State<Arc<AppState>>` to `State<AppState>` — forgetting to update one is exactly the "state type mismatch" compiler error already shown in Tutorial 04 §14:

```
error[E0277]: the trait bound `fn(State<Arc<AppState>>) ...: Handler<_, _>` is not satisfied
note: `State<Arc<AppState>>` can only be used as an extractor if the state is `Arc<AppState>`
      but the state is `AppState`
```

**Fix:** update every handler's `State<...>` type parameter to match whatever `.with_state(...)` is actually given.

### `main()` — building and passing state

```rust
let store = Arc::new(RwLock::new(IndexStore::new()));
let state = AppState { cluster_name, store };

let app = Router::new()
    .route("/", get(root_handler))
    .route("/_cluster/health", get(cluster_health_handler))
    .route("/_cat/indices", get(cat_indices_handler))
    .with_state(state);
```

### Updated handlers (`root_handler` / `cluster_health_handler`)

Only the `State` type changes — bodies are unchanged from Tutorial 04:

```rust
async fn root_handler(State(state): State<AppState>) -> impl IntoResponse { /* ... */ }

async fn cluster_health_handler(State(state): State<AppState>) -> impl IntoResponse { /* ... */ }
```

### New: `GET /_cat/indices`

The first handler that actually touches `state.store`. Demonstrates the full read-lock pattern: acquire, read, let the guard drop before returning.

```rust
use es_index::IndexInfo;

/// `GET /_cat/indices` — one row per index, ES `_cat` API shape.
/// Empty store returns `[]`, not an error — an empty cluster is a valid state.
async fn cat_indices_handler(State(state): State<AppState>) -> impl IntoResponse {
    let indices: Vec<IndexInfo> = {
        // Read lock: any number of concurrent GETs can be in this block at once.
        let store = state
            .store
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        store
            .list()                              // sorted Vec<&IndexMetadata>, from Tutorial 03
            .into_iter()
            .map(IndexInfo::from_metadata)
            .collect()
    }; // <- `store` (the RwLockReadGuard) drops here, lock released

    (StatusCode::OK, Json(indices))
}
```

The inner `{ }` block is deliberate, same reasoning as §4: it scopes the guard's lifetime to exactly the work that needs the lock, and nothing after that point (JSON serialization, building the response) holds it. Nothing in this handler `.await`s while `store` is alive, so `std::sync::RwLock` (§6) is correct here.

```bash
curl -s http://localhost:9200/_cat/indices | jq .
# []
```

### Complete diff summary

```
changed:
  - es-mini/Cargo.toml: no new deps (es-index already present since Tutorial 04)
  - es-mini/src/main.rs:
      + use std::sync::RwLock
      + use es_index::{IndexStore, IndexInfo}
      AppState: + store: Arc<RwLock<IndexStore>>, outer Arc<AppState> wrapper removed
      root_handler / cluster_health_handler: State<Arc<AppState>> -> State<AppState>
      + cat_indices_handler + GET /_cat/indices route
      main(): + let store = Arc::new(RwLock::new(IndexStore::new()));
```

---

## 8. What Was Skipped

- **`tokio::sync::RwLock`** — not used. Nothing in es-mini `.await`s while holding the store lock yet (§6). Revisit if Phase 4's on-disk codec reader needs to do async I/O mid-lock.
- **`IndexStore` returning `Result`** — deferred again, same call as Tutorial 05 §11. `list()` still can't fail; Tutorial 07's `PUT`/`DELETE` handlers are the first to need `AppError::IndexNotFound`/`ResourceAlreadyExists` against the store.
- **Sharding/per-index locks** — one `RwLock` guards the whole `IndexStore`. Fine at MVP scale (single node, in-memory catalog); a lock-per-index map would be premature — nothing has measured contention yet.
  <br>`// ponytail: single RwLock over the whole store, per-index locking if catalog-write contention ever shows up in profiling`
- **`parking_lot::RwLock`** — a popular non-std alternative (no poisoning, smaller/faster guards). Not adopted: `std::sync::RwLock` has no dependency cost and poisoning is a feature here, not friction, for a store this small.

---

## 9. Definition of Done

1. **`Arc<RwLock<IndexStore>>` field present on `AppState`:**
   ```bash
   grep -A3 "struct AppState" es-mini/src/main.rs
   # store: Arc<RwLock<IndexStore>>,
   ```

2. **Outer `Arc<AppState>` wrapper removed; handlers use `State<AppState>`:**
   ```bash
   grep -c "State<Arc<AppState>>" es-mini/src/main.rs
   # 0
   grep -c "State<AppState>" es-mini/src/main.rs
   # 3 (root_handler, cluster_health_handler, cat_indices_handler)
   ```

3. **`IndexStore` imported from `es-index`:**
   ```bash
   grep "use es_index::" es-mini/src/main.rs
   # use es_index::{IndexStore, IndexInfo, ClusterHealthResponse};
   ```

4. **Workspace builds clean:**
   ```bash
   cargo build --workspace
   cargo clippy --workspace --all-targets -- -D warnings
   # both exit 0
   ```

5. **`GET /_cat/indices` returns `200` and an empty JSON array on a fresh store:**
   ```bash
   curl -s -o /dev/null -w "%{http_code}" http://localhost:9200/_cat/indices
   # 200
   curl -s http://localhost:9200/_cat/indices | jq .
   # []
   ```

6. **Existing endpoints from Tutorials 04/05 still work after the `AppState` shape change:**
   ```bash
   curl -sf http://localhost:9200/ | jq -e '.version.number'
   curl -sf http://localhost:9200/_cluster/health | jq -e '.status'
   # "9.0.0" / "green"
   ```

7. **Lock poisoning is handled, not `.unwrap()`'d blindly, in the handler:**
   ```bash
   grep "unwrap_or_else(|poisoned| poisoned.into_inner())" es-mini/src/main.rs
   ```

8. **The negative-compile example (§5) actually fails to build when tried verbatim:**
   ```bash
   # paste the two-thread bare-IndexStore example into a scratch file
   rustc --edition 2021 --crate-type bin scratch.rs 2>&1 | grep "E0499"
   # error[E0499]: cannot borrow `store` as mutable more than once at a time
   ```

9. **Full test suite still green:**
   ```bash
   cargo test --workspace
   # all tests pass, 0 failures
   ```

---

**Next:** Tutorial 07 — Index Management Endpoints. `PUT /{index}`, `DELETE /{index}`, `GET`/`HEAD /{index}` wire up against `state.store.write()`/`.read()` using the pattern established here — and are the first real call sites for `AppError::IndexNotFound` and `AppError::ResourceAlreadyExists` (Tutorial 05).
