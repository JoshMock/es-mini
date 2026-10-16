# Tutorial 08 — Document CRUD Endpoints

**Series:** es-mini — Building Elasticsearch in Rust
**Phase:** 1, Task 7
**Prerequisites:** Tutorials 01–07 complete — workspace builds, `es-index::IndexStore`/`IndexMetadata` exist (Tutorial 03), `Arc<RwLock<IndexStore>>` in `AppState` (Tutorial 06), `AppError`/`IntoResponse`/`IndexStoreError` pattern exists (Tutorials 05, 07), multi-segment `Path<(String, String)>` previewed (Tutorial 07 §1), `build_app` + `tests/common/mod.rs` integration-test scaffolding exists (Tutorial 07 §7).

---

By the end of this tutorial you will have:

- A `Document` struct and a `documents: HashMap<String, Document>` field added to `IndexMetadata`
- Auto-ID generation via the `uuid` crate
- `document_put`, `document_create`, `document_get`, `document_delete`, `document_update` methods on `IndexMetadata`, all returning `Result<DocumentOpResult, IndexStoreError>`
- `AppError::VersionConflict` (409, `version_conflict_engine_exception`)
- `POST`/`PUT /{index}/_doc[/{id}]`, `PUT /{index}/_create/{id}`, `GET`/`DELETE /{index}/_doc/{id}`, `POST /{index}/_update/{id}` wired to real handlers
- A `merge_json` helper for partial updates, with unit tests
- Integration tests covering the full document lifecycle, `_create` conflicts, missing-doc 404s, auto-ID roundtrips, and partial-update merging

---

## 1. The `Document` Type

A document in ES is: a JSON blob (`_source`), an ID, and three numbers that track how many times it's changed. In `es-index`:

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Document {
    pub id: String,
    pub source: serde_json::Value,
    pub version: u64,
    pub seq_no: u64,
    pub primary_term: u64,
}
```

- **`source: serde_json::Value`** — the document body as the caller sent it, untyped. es-mini has no schema-validated document type (that's what `Mapping`, Tutorial 03, is *for*, but this MVP doesn't enforce it against writes yet) — `Value` is the correct escape hatch, same role as Python `dict` or TS `any` here, not a shortcut around real modeling.
- **`version: u64`** — ES's `_version`: starts at 1, increments by 1 on every successful write, never decrements, never reused even after delete-then-recreate in real ES (this in-memory implementation resets to 1 on recreate after delete — noted as a divergence, not tracked as a gap worth closing for an in-memory MVP).
- **`seq_no: u64`** — ES's `_seq_no`: a per-shard monotonic counter across *all* operations on the index (not per-document like `_version`). Real ES uses this for replication/recovery ordering; single-node es-mini has no replication, but the field is part of the wire contract every client parses, so it's tracked as a plain increment-on-every-write-to-this-index counter.
- **`primary_term: u64`** — ES's `_primary_term`: bumped on primary shard promotion (failover). Single-node es-mini never promotes anything — always `1`. Field exists purely because clients expect it in the response envelope.

`u64` (not `i64`) for all three: these counters are never negative, and "value can never legitimately be negative" is exactly the signal `u64` communicates to a reader that `i64` doesn't — same reasoning as Tutorial 03's numeric-type table, applied to counters instead of shard counts.

## 2. Where Documents Live: `IndexMetadata` vs. a Separate `DocumentStore`

Two shapes were on the table:

**Option A — `documents: HashMap<String, Document>` field on `IndexMetadata`.**
An index "owns" its documents directly, same struct returned by `IndexStore::get`. One less type, one less lookup (`store.get(&index)?.documents` vs. `store.get(&index)?` then `doc_store.get(&index)?`).

**Option B — separate `DocumentStore` struct, keyed by index name, sibling to `IndexStore` in `AppState`.**
Cleaner separation of "index metadata" from "index contents" — mirrors how ES itself separates cluster/index *state* from shard *data*. Costs a second `HashMap` lookup on every document operation (index name → `DocumentStore` entry, then doc ID → `Document`), and a second lock if it's independently wrapped in `Arc<RwLock<_>>`.

| | A: field on `IndexMetadata` | B: separate `DocumentStore` |
|---|---|---|
| Lookups per doc op | 1 (`IndexStore` → `IndexMetadata`) | 2 (`IndexStore` → `IndexMetadata` exists check, `DocumentStore` → docs) |
| Models real ES's state/data split | No | Yes |
| Extra type/file | No | Yes (`DocumentStore`, its own error variants or reuse) |
| Delete-index-deletes-its-docs "for free" | Yes (drop the `IndexMetadata`, docs go with it) | No (must remember to also clear the sibling entry) |

**Recommendation: Option A.** Nothing in this MVP needs documents and metadata to scale, lock, or be deleted independently — real ES's separation exists for replication/shard-relocation reasons that don't apply to a single-node in-memory store. Option B's "for later" flexibility is exactly the kind of speculative structure the project's own guiding principle (build the HTTP contract first, simplest thing that works) argues against. Revisit only if/when documents genuinely need a different lifecycle than their index (e.g. streaming to disk in Phase 4/5 while metadata stays in memory).

```rust
pub struct IndexMetadata {
    pub name: String,
    pub number_of_shards: u32,
    pub number_of_replicas: u32,
    pub uuid: Option<String>,
    pub creation_date: Option<i64>,
    pub documents: HashMap<String, Document>, // new
    next_seq_no: u64,                          // new, private — see §5
}
```

`documents` is `pub` for the same reason every other field on `IndexMetadata` is — `es-mini` needs to read it when building response bodies. `next_seq_no` stays private: it's an internal counter, not part of the public contract, and making it `pub` would let a caller corrupt sequencing by hand.

`IndexMetadata::new` (Tutorial 03) must be updated to initialize both new fields — the struct literal inside it won't compile otherwise:

```rust
impl IndexMetadata {
    pub fn new(name: &str, number_of_shards: u32, number_of_replicas: u32) -> Self {
        IndexMetadata {
            name: name.to_string(),
            number_of_shards,
            number_of_replicas,
            uuid: None,
            creation_date: None,
            documents: HashMap::new(), // new
            next_seq_no: 0,            // new
        }
    }

    // name(&self) unchanged from Tutorial 03
}
```

### `IndexStore` gains `get_mut`

Every document-mutating handler below needs a `&mut IndexMetadata` to call `document_put`/`document_create`/etc. on — `IndexStore::get` (Tutorial 07) only ever hands back `&IndexMetadata`. `IndexStore` gains a mutable counterpart, same not-found mapping as `get`:

```rust
impl IndexStore {
    pub fn get_mut(&mut self, name: &str) -> Result<&mut IndexMetadata, IndexStoreError> {
        self.indices
            .get_mut(name)
            .ok_or_else(|| IndexStoreError::NotFound(name.to_string()))
    }
}
```

## 3. Auto-ID Generation

`POST /{index}/_doc` (no `{id}` in the URL) asks ES to generate an ID. Real ES generates a 20-character URL-safe string from Base64-encoded random bytes (technically a flake-style ID, not a UUID). es-mini uses the `uuid` crate instead:

```toml
# Cargo.toml (root), [workspace.dependencies]
uuid = { version = "1", features = ["v4"] }
```

```toml
# es-index/Cargo.toml
uuid = { workspace = true }
```

```rust
use uuid::Uuid;

// ponytail: real ES IDs are 20-char Base64 of random bytes, not UUIDv4.
// They look different (hyphens, 36 chars) but serve the identical purpose
// here — an unpredictable unique key — and no test asserts on ID *format*,
// only that round-tripping an ID works. Swap in a real flake/Base64
// generator only if a test or client ever depends on ES's exact ID shape.
fn generate_id() -> String {
    Uuid::new_v4().to_string()
}
```

`uuid = "1"` resolves to the 1.x series (currently 1.11+); `features = ["v4"]` is required — the crate ships v1/v3/v4/v5/v7 generation behind separate feature flags to avoid pulling in unused dependencies (v1/v6/v7 need a clock+node-id source, v3/v5 need a hasher), and none are enabled by default. `Uuid::new_v4()` needs only the `v4` feature (pure-random, no external inputs) — the cheapest correct choice here.

## 4. `IndexResult` — Mapping to ES's `result` Field

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexResult {
    Created,
    Updated,
    Deleted,
    NotFound,
    NoOp,
}

impl IndexResult {
    pub fn as_es_str(&self) -> &'static str {
        match self {
            IndexResult::Created => "created",
            IndexResult::Updated => "updated",
            IndexResult::Deleted => "deleted",
            IndexResult::NotFound => "not_found",
            IndexResult::NoOp => "noop",
        }
    }
}
```

`as_es_str` returns `&'static str` — a reference to string data baked into the binary at compile time (the match arms are literals), not heap-allocated per call. `match` here is exhaustive: adding a sixth variant without adding a match arm is a compile error, the same guarantee `AppError`'s `status_code`/`error_type` methods rely on (Tutorial 05).

## 5. Document Operations on `IndexMetadata`

```rust
use crate::error::IndexStoreError;

pub struct DocumentOpResult {
    pub document: Document,
    pub result: IndexResult,
}

impl IndexMetadata {
    fn next_seq(&mut self) -> u64 {
        let n = self.next_seq_no;
        self.next_seq_no += 1;
        n
    }

    /// PUT semantics: create if absent, overwrite (version++) if present.
    pub fn document_put(
        &mut self,
        id: String,
        source: serde_json::Value,
    ) -> DocumentOpResult {
        let seq_no = self.next_seq();
        let result_kind = if self.documents.contains_key(&id) {
            IndexResult::Updated
        } else {
            IndexResult::Created
        };
        let doc = self
            .documents
            .entry(id.clone())
            .and_modify(|d| {
                d.source = source.clone();
                d.version += 1;
            })
            .or_insert(Document {
                id: id.clone(),
                source: source.clone(),
                version: 1,
                seq_no,
                primary_term: 1,
            });
        doc.seq_no = seq_no;
        DocumentOpResult { document: doc.clone(), result: result_kind }
    }

    /// `_create` semantics: fail with `VersionConflict`-mapped error if `id` already exists.
    pub fn document_create(
        &mut self,
        id: String,
        source: serde_json::Value,
    ) -> Result<DocumentOpResult, IndexStoreError> {
        if self.documents.contains_key(&id) {
            return Err(IndexStoreError::VersionConflict(id));
        }
        let seq_no = self.next_seq();
        let doc = Document { id: id.clone(), source, version: 1, seq_no, primary_term: 1 };
        self.documents.insert(id, doc.clone());
        Ok(DocumentOpResult { document: doc, result: IndexResult::Created })
    }

    pub fn document_get(&self, id: &str) -> Result<Document, IndexStoreError> {
        self.documents
            .get(id)
            .cloned()
            .ok_or_else(|| IndexStoreError::DocumentNotFound(id.to_string()))
    }

    pub fn document_delete(&mut self, id: &str) -> Result<DocumentOpResult, IndexStoreError> {
        let mut doc = self
            .documents
            .remove(id)
            .ok_or_else(|| IndexStoreError::DocumentNotFound(id.to_string()))?;
        doc.version += 1;
        doc.seq_no = self.next_seq();
        Ok(DocumentOpResult { document: doc, result: IndexResult::Deleted })
    }

    /// `_update` semantics: merge `partial` into the existing `_source`.
    pub fn document_update(
        &mut self,
        id: &str,
        partial: serde_json::Value,
    ) -> Result<DocumentOpResult, IndexStoreError> {
        let existing = self
            .documents
            .get(id)
            .ok_or_else(|| IndexStoreError::DocumentNotFound(id.to_string()))?;

        let merged = crate::merge_json(existing.source.clone(), partial);
        if merged == existing.source {
            return Ok(DocumentOpResult { document: existing.clone(), result: IndexResult::NoOp });
        }

        let seq_no = self.next_seq();
        let doc = self.documents.get_mut(id).expect("existence checked above");
        doc.source = merged;
        doc.version += 1;
        doc.seq_no = seq_no;
        Ok(DocumentOpResult { document: doc.clone(), result: IndexResult::Updated })
    }
}
```

`document_get`/`document_update` take `id: &str` (borrowed) — they only read the key to look it up, never store it. `document_put`/`document_create` take `id: String` (owned) — the ID is stored into the map, so ownership genuinely needs to move in. This is the same "read borrows, store owns" rule from Tutorial 03, now applied to method parameters instead of struct fields.

## 6. The HashMap Entry API

`document_put`'s `.entry(id.clone()).and_modify(...).or_insert(...)` is the idiomatic Rust answer to "check if a key exists, then either update or insert" *without* looking the key up twice.

The naive version — and the bug it invites — looks like the JS `Map` pattern most JS developers reach for first:

```js
// JS: two lookups, and a race if this were concurrent
if (map.has(id)) {
  const doc = map.get(id);
  doc.version += 1;
} else {
  map.set(id, newDoc());
}
```

Two problems with the JS version, both solved by `entry`:

1. **Double lookup.** `has()` walks the hash table, then `get()`/`set()` walks it again. `entry()` computes the bucket location once and hands back a handle (`Entry<'_, K, V>`) that both branches reuse.
2. **Check-then-act gap.** Between `has()` returning and `set()` running, nothing prevents another thread from writing to the same key — a classic TOCTOU (time-of-check-to-time-of-use) bug. `entry()` doesn't eliminate the need for `IndexMetadata`'s caller to hold a write lock (Tutorial 06's `RwLock` is still what makes this safe across handlers), but it does eliminate the *redundant* internal lookup, and its type-level shape (`Entry` is consumed by exactly one of `.or_insert`/`.and_modify().or_insert()`/`.or_insert_with()`) makes "I checked but forgot to act consistently" a compile-time non-issue rather than a runtime race.

Two entry-API forms used here:

- `entry(key).or_insert_with(|| expensive())` — insert-if-absent, lazy default (the closure only runs if the key is actually missing — contrast with `.or_insert(expensive())`, which evaluates eagerly even when unused).
- `entry(key).and_modify(|v| { /* mutate in place */ }).or_insert(default)` — the exact "update or insert" shape `document_put` needs: mutate the existing `Document` if present, otherwise insert a fresh one.

Analogous to Python's `dict.setdefault(key, default)` (insert-if-absent, no modify-branch) or `collections.defaultdict` (insert-if-absent, always the same default factory) — Rust's `entry` API covers both of those *plus* the update-existing branch in one chained call, because Python's stdlib equivalents don't attempt the "or update" half at all.

## 7. Merging JSON for Partial Updates

`serde_json::Value::Object` wraps a `serde_json::Map<String, Value>` (an insertion-order-preserving map, not `std::collections::HashMap` — this matters if response field order is ever asserted on, though no test here does). Merging two JSON objects means: for every key in the incoming partial update, overwrite (or insert) that key in the existing object; recurse if both sides are also objects at that key.

```rust
use serde_json::Value;

/// Merge `patch` into `base`, ES `_update`-style: shallow overwrite per key,
/// recursing only when both sides are JSON objects at that key. Arrays and
/// scalars in `patch` fully replace the corresponding value in `base`.
pub fn merge_json(base: Value, patch: Value) -> Value {
    match (base, patch) {
        (Value::Object(mut base_map), Value::Object(patch_map)) => {
            for (key, patch_val) in patch_map {
                let merged = match base_map.remove(&key) {
                    Some(base_val) => merge_json(base_val, patch_val),
                    None => patch_val,
                };
                base_map.insert(key, merged);
            }
            Value::Object(base_map)
        }
        // Non-object on either side: patch wins outright (matches ES: a
        // partial update replaces scalars/arrays, it doesn't array-merge).
        (_, patch) => patch,
    }
}

#[cfg(test)]
mod merge_json_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn shallow_key_overwrite() {
        let base = json!({"a": 1, "b": 2});
        let patch = json!({"b": 3});
        assert_eq!(merge_json(base, patch), json!({"a": 1, "b": 3}));
    }

    #[test]
    fn adds_new_keys() {
        let base = json!({"a": 1});
        let patch = json!({"c": 3});
        assert_eq!(merge_json(base, patch), json!({"a": 1, "c": 3}));
    }

    #[test]
    fn recurses_into_nested_objects() {
        let base = json!({"user": {"name": "a", "age": 30}});
        let patch = json!({"user": {"age": 31}});
        assert_eq!(
            merge_json(base, patch),
            json!({"user": {"name": "a", "age": 31}})
        );
    }

    #[test]
    fn scalar_patch_replaces_array() {
        let base = json!({"tags": ["a", "b"]});
        let patch = json!({"tags": ["c"]});
        assert_eq!(merge_json(base, patch), json!({"tags": ["c"]}));
    }

    #[test]
    fn no_op_when_identical() {
        let base = json!({"a": 1});
        let patch = json!({"a": 1});
        assert_eq!(merge_json(base.clone(), patch), base);
    }
}
```

`base_map.remove(&key)` (not `base_map.get(&key)`) is the `Option::take`-style pattern this tutorial's Rust-concepts list calls out: `remove` both returns the existing value *and* deletes the entry in one call, avoiding a separate `base_map.remove(&key);` afterward. `HashMap::remove` (used again below, in `document_delete`) has the identical shape: "take the value out and hand it to me, I'm done with the slot."

`document_update` (§5) compares `merged == existing.source` to detect the no-op case — `Value` derives `PartialEq`, so this is a deep structural comparison, not a reference check, which is exactly what's needed to match real ES's `"result":"noop"` behavior (a `_update` call whose `doc` produces no actual change to `_source`).

## 8. New `IndexStoreError` Variants

Extend the enum from Tutorial 07:

```rust
#[derive(Debug, Error, PartialEq, Eq)]
pub enum IndexStoreError {
    #[error("index already exists: {0}")]
    AlreadyExists(String),
    #[error("index not found: {0}")]
    NotFound(String),
    #[error("document not found: {0}")]
    DocumentNotFound(String),        // new
    #[error("version conflict for document: {0}")]
    VersionConflict(String),          // new
}
```

## 9. `AppError::VersionConflict`

```rust
// es-mini/src/error.rs
#[derive(Debug, Error)]
pub enum AppError {
    IndexNotFound(String),
    ResourceAlreadyExists(String),
    DocumentNotFound { index: String, id: String },
    VersionConflict { index: String, id: String },   // new
    InvalidRequest(String),
    Internal(String),
}
```

`status_code`/`error_type` gain a new exhaustive match arm — the compiler flags every place a match on `AppError` needs updating, same guarantee as every other variant addition since Tutorial 05:

```rust
AppError::VersionConflict { .. } => StatusCode::CONFLICT,       // 409
```
```rust
AppError::VersionConflict { .. } => "version_conflict_engine_exception",
```

`version_conflict_engine_exception` is real ES's exception type string for this case (confirmed against ES's optimistic-concurrency-control error responses — the same exception fires for `_create` on an existing doc and for a mismatched `if_seq_no`/`if_primary_term` on any write; this tutorial only exercises the `_create`-conflict path, the seq/term-conflict path is out of scope).

**Bridge, following the `IndexStoreError → AppError` precedent from Tutorial 07:**

```rust
impl From<es_index::IndexStoreError> for AppError {
    fn from(err: es_index::IndexStoreError) -> Self {
        match err {
            es_index::IndexStoreError::AlreadyExists(name) => AppError::ResourceAlreadyExists(name),
            es_index::IndexStoreError::NotFound(name) => AppError::IndexNotFound(name),
            es_index::IndexStoreError::DocumentNotFound(id) => {
                // index name filled in by the handler (IndexStoreError doesn't carry it)
                AppError::DocumentNotFound { index: String::new(), id }
            }
            es_index::IndexStoreError::VersionConflict(id) => {
                AppError::VersionConflict { index: String::new(), id }
            }
        }
    }
}
```

The blank `index: String::new()` above is a real gap: `IndexStoreError` (by design, per Tutorial 07 — zero HTTP knowledge, zero coupling to what called it) doesn't know which index it came from. Handlers below construct `AppError::DocumentNotFound`/`VersionConflict` directly instead of relying on `?`'s automatic `From` conversion when the index name matters for the error body — see §10.

## 10. The Endpoints

Response envelope structs, matching the ES field names exactly via `#[serde(rename = "...")]`:

```rust
use serde::Serialize;

#[derive(Debug, Serialize)]
struct Shards {
    total: u32,
    successful: u32,
    failed: u32,
}

impl Shards {
    fn single_node() -> Self {
        Shards { total: 2, successful: 1, failed: 0 }
    }
}

#[derive(Debug, Serialize)]
struct DocWriteResponse {
    #[serde(rename = "_index")]
    index: String,
    #[serde(rename = "_id")]
    id: String,
    #[serde(rename = "_version")]
    version: u64,
    result: String,
    #[serde(rename = "_shards")]
    shards: Shards,
    #[serde(rename = "_seq_no")]
    seq_no: u64,
    #[serde(rename = "_primary_term")]
    primary_term: u64,
}

#[derive(Debug, Serialize)]
struct DocGetResponse {
    #[serde(rename = "_index")]
    index: String,
    #[serde(rename = "_id")]
    id: String,
    #[serde(rename = "_version")]
    version: u64,
    #[serde(rename = "_seq_no")]
    seq_no: u64,
    #[serde(rename = "_primary_term")]
    primary_term: u64,
    found: bool,
    #[serde(rename = "_source")]
    source: serde_json::Value,
}
```

**Why `#[serde(rename = "...")]` here and not `#[serde(rename_all = "...")]`:** `rename_all` (used in Tutorial 03 for `FieldType`'s snake_case variants) transforms *every* field/variant by one consistent rule. ES's document response fields don't follow one consistent rule relative to Rust's naming convention — they're prefixed with `_` (`_index`, `_id`, `_version`, `_seq_no`, `_primary_term`, `_shards`, `_source`) specifically to visually distinguish "ES metadata about the document" from "the document's own content" (unprefixed `found`, `result`). A leading underscore isn't valid Rust identifier convention to lean on (a field named `_index` in Rust is legal but conventionally means "unused" — the opposite of this field's meaning), so each metadata field gets its own explicit `rename`, and the unprefixed fields (`found`, `result`) are left alone.

**`POST`/`PUT /{index}/_doc[/{id}]`** — with `{id}`: PUT semantics (create-or-overwrite). Without `{id}`: always create, auto-generated ID.

```rust
use axum::extract::{Path, State};
use axum::{http::StatusCode, response::IntoResponse, Json};
use serde_json::Value;

async fn put_doc(
    State(state): State<AppState>,
    Path((index_name, id)): Path<(String, String)>,
    Json(source): Json<Value>,
) -> Result<impl IntoResponse, AppError> {
    let mut store = state.store.write().unwrap_or_else(|p| p.into_inner());
    let meta = store
        .get_mut(&index_name)
        .map_err(|_| AppError::IndexNotFound(index_name.clone()))?;
    let op = meta.document_put(id, source);
    let status = match op.result {
        es_index::IndexResult::Created => StatusCode::CREATED,
        _ => StatusCode::OK,
    };
    Ok((status, Json(doc_write_response(&index_name, op))))
}

async fn post_doc_auto_id(
    State(state): State<AppState>,
    Path(index_name): Path<String>,
    Json(source): Json<Value>,
) -> Result<impl IntoResponse, AppError> {
    let mut store = state.store.write().unwrap_or_else(|p| p.into_inner());
    let meta = store
        .get_mut(&index_name)
        .map_err(|_| AppError::IndexNotFound(index_name.clone()))?;
    let id = es_index::generate_id();
    let op = meta.document_put(id, source);
    Ok((StatusCode::CREATED, Json(doc_write_response(&index_name, op))))
}

fn doc_write_response(index_name: &str, op: es_index::DocumentOpResult) -> DocWriteResponse {
    DocWriteResponse {
        index: index_name.to_string(),
        id: op.document.id,
        version: op.document.version,
        result: op.result.as_es_str().to_string(),
        shards: Shards::single_node(),
        seq_no: op.document.seq_no,
        primary_term: op.document.primary_term,
    }
}
```

Response shape (`PUT`, first write to a new `id`):

```json
{"_index":"my-index","_id":"abc","_version":1,"result":"created","_shards":{"total":2,"successful":1,"failed":0},"_seq_no":0,"_primary_term":1}
```

`POST /{index}/_doc` (auto-ID) returns the identical shape with `"result":"created"` — ES makes no distinction in the response body between an auto-ID create and an explicit-ID create; the only difference is which endpoint generated the `_id`.

**`PUT /{index}/_create/{id}`** — must not overwrite:

```rust
async fn create_doc(
    State(state): State<AppState>,
    Path((index_name, id)): Path<(String, String)>,
    Json(source): Json<Value>,
) -> Result<impl IntoResponse, AppError> {
    let mut store = state.store.write().unwrap_or_else(|p| p.into_inner());
    let meta = store
        .get_mut(&index_name)
        .map_err(|_| AppError::IndexNotFound(index_name.clone()))?;
    let op = meta.document_create(id.clone(), source).map_err(|e| match e {
        es_index::IndexStoreError::VersionConflict(_) => {
            AppError::VersionConflict { index: index_name.clone(), id: id.clone() }
        }
        other => other.into(),
    })?;
    Ok((StatusCode::CREATED, Json(doc_write_response(&index_name, op))))
}
```

This is the call site referenced in §9: `.map_err` intercepts `VersionConflict` specifically to fill in `index_name` (which bare `?`-via-`From` can't do, since `IndexStoreError` doesn't carry it), and falls through to the blanket `From` conversion (`.into()`) for any other variant — cheap insurance against this match arm going stale if `document_create` ever grows a new error case.

**`GET /{index}/_doc/{id}`:**

```rust
async fn get_doc(
    State(state): State<AppState>,
    Path((index_name, id)): Path<(String, String)>,
) -> Result<impl IntoResponse, AppError> {
    let store = state.store.read().unwrap_or_else(|p| p.into_inner());
    let meta = store
        .get(&index_name)
        .map_err(|_| AppError::IndexNotFound(index_name.clone()))?;
    let doc = meta
        .document_get(&id)
        .map_err(|_| AppError::DocumentNotFound { index: index_name.clone(), id: id.clone() })?;
    Ok(Json(DocGetResponse {
        index: index_name,
        id: doc.id,
        version: doc.version,
        seq_no: doc.seq_no,
        primary_term: doc.primary_term,
        found: true,
        source: doc.source,
    }))
}
```

Response shape (found):

```json
{"_index":"my-index","_id":"abc","_version":1,"_seq_no":0,"_primary_term":1,"found":true,"_source":{"...":"..."}}
```

Missing document → 404, `document_missing_exception` (Tutorial 05's existing `AppError::DocumentNotFound` variant — reused as-is, no new variant needed for GET/DELETE 404s, only `_create`'s 409 needed one).

**`DELETE /{index}/_doc/{id}`:**

```rust
async fn delete_doc(
    State(state): State<AppState>,
    Path((index_name, id)): Path<(String, String)>,
) -> Result<impl IntoResponse, AppError> {
    let mut store = state.store.write().unwrap_or_else(|p| p.into_inner());
    let meta = store
        .get_mut(&index_name)
        .map_err(|_| AppError::IndexNotFound(index_name.clone()))?;
    let op = meta
        .document_delete(&id)
        .map_err(|_| AppError::DocumentNotFound { index: index_name.clone(), id: id.clone() })?;
    Ok((StatusCode::OK, Json(doc_write_response(&index_name, op))))
}
```

Response shape:

```json
{"_index":"my-index","_id":"abc","_version":2,"result":"deleted","_shards":{"total":2,"successful":1,"failed":0},"_seq_no":1,"_primary_term":1}
```

**`POST /{index}/_update/{id}`** — body is `{"doc": {...}}`, merged into the existing `_source`:

```rust
#[derive(Debug, serde::Deserialize)]
struct UpdateBody {
    doc: Value,
}

async fn update_doc(
    State(state): State<AppState>,
    Path((index_name, id)): Path<(String, String)>,
    Json(body): Json<UpdateBody>,
) -> Result<impl IntoResponse, AppError> {
    let mut store = state.store.write().unwrap_or_else(|p| p.into_inner());
    let meta = store
        .get_mut(&index_name)
        .map_err(|_| AppError::IndexNotFound(index_name.clone()))?;
    let op = meta
        .document_update(&id, body.doc)
        .map_err(|_| AppError::DocumentNotFound { index: index_name.clone(), id: id.clone() })?;
    Ok((StatusCode::OK, Json(doc_write_response(&index_name, op))))
}
```

### Router wiring

```rust
use axum::routing::{post, put};

Router::new()
    .route(
        "/{index}/_doc/{id}",
        put(put_doc).get(get_doc).delete(delete_doc),
    )
    .route("/{index}/_doc", post(post_doc_auto_id))
    .route("/{index}/_create/{id}", put(create_doc))
    .route("/{index}/_update/{id}", post(update_doc))
```

## 11. Integration Tests

New file, reusing the exact scaffolding from Tutorial 07 (no changes needed to `tests/common/mod.rs`):

```rust
// es-mini/tests/document_crud.rs
mod common;

use serde_json::json;

#[tokio::test]
async fn create_put_get_update_delete_happy_path() {
    let server = common::test_server();
    server.put("/my-index").await.assert_status_ok();

    server
        .put("/my-index/_doc/1")
        .json(&json!({"title": "hello", "views": 1}))
        .await
        .assert_status(axum::http::StatusCode::CREATED);

    server
        .get("/my-index/_doc/1")
        .await
        .assert_json_contains(&json!({"found": true, "_source": {"title": "hello", "views": 1}}));

    server
        .post("/my-index/_update/1")
        .json(&json!({"doc": {"views": 2}}))
        .await
        .assert_json_contains(&json!({"result": "updated"}));

    server
        .get("/my-index/_doc/1")
        .await
        .assert_json_contains(&json!({"_source": {"title": "hello", "views": 2}}));

    server
        .delete("/my-index/_doc/1")
        .await
        .assert_json_contains(&json!({"result": "deleted"}));

    server.get("/my-index/_doc/1").await.assert_status_not_found();
}

#[tokio::test]
async fn create_on_existing_id_is_409() {
    let server = common::test_server();
    server.put("/my-index").await.assert_status_ok();
    server
        .put("/my-index/_create/dup")
        .json(&json!({"a": 1}))
        .await
        .assert_status(axum::http::StatusCode::CREATED);

    server
        .put("/my-index/_create/dup")
        .json(&json!({"a": 2}))
        .await
        .assert_status(axum::http::StatusCode::CONFLICT)
        .assert_json_contains(&json!({"error": {"type": "version_conflict_engine_exception"}}));
}

#[tokio::test]
async fn get_missing_document_is_404() {
    let server = common::test_server();
    server.put("/my-index").await.assert_status_ok();
    server.get("/my-index/_doc/nope").await.assert_status_not_found();
}

#[tokio::test]
async fn auto_id_post_then_get_roundtrip() {
    let server = common::test_server();
    server.put("/my-index").await.assert_status_ok();

    let created = server
        .post("/my-index/_doc")
        .json(&json!({"title": "auto"}))
        .await;
    created.assert_status(axum::http::StatusCode::CREATED);
    let id = created.json::<serde_json::Value>()["_id"].as_str().unwrap().to_string();

    server
        .get(&format!("/my-index/_doc/{id}"))
        .await
        .assert_json_contains(&json!({"found": true, "_source": {"title": "auto"}}));
}

#[tokio::test]
async fn partial_update_merges_and_bumps_version_and_seq_no() {
    let server = common::test_server();
    server.put("/my-index").await.assert_status_ok();

    server
        .put("/my-index/_doc/merge-me")
        .json(&json!({"a": 1, "b": {"x": 1, "y": 2}}))
        .await
        .assert_status(axum::http::StatusCode::CREATED);

    let updated = server
        .post("/my-index/_update/merge-me")
        .json(&json!({"doc": {"b": {"y": 99}}}))
        .await;
    updated.assert_json_contains(&json!({
        "result": "updated",
        "_version": 2,
        "_seq_no": 1
    }));

    server
        .get("/my-index/_doc/merge-me")
        .await
        .assert_json_contains(&json!({"_source": {"a": 1, "b": {"x": 1, "y": 99}}}));
}
```

Auto-ID roundtrip pulls `_id` out of the create response's JSON body (`created.json::<serde_json::Value>()`) — the ID is only known after the server generates it, so the test can't hardcode a path; this is the same "read the response, use it in the next request" shape any client-library integration test needs, JS/Python or otherwise.

## 12. Definition of Done

1. `cargo build --workspace` succeeds with `Document`, `IndexResult`, `DocumentOpResult`, `merge_json`, and the `document_put`/`document_create`/`document_get`/`document_delete`/`document_update` methods added to `es-index`.
2. `uuid = { version = "1", features = ["v4"] }` added to root `[workspace.dependencies]`; `es-index/Cargo.toml` depends on it via `{ workspace = true }`.
3. `IndexStoreError` gains `DocumentNotFound` and `VersionConflict` variants; `AppError` gains `VersionConflict { index, id }` mapped to `409`/`version_conflict_engine_exception`.
4. `cargo clippy --workspace --all-targets -- -D warnings` passes.
5. `curl -X PUT localhost:9200/my-index/_doc/1 -d '{"a":1}'` returns `201` with `_index`, `_id`, `_version:1`, `result:"created"`, `_shards`, `_seq_no`, `_primary_term` all present.
6. `curl -X PUT localhost:9200/my-index/_create/1 -d '{"a":1}'` a second time (same ID) returns `409` with `error.type == "version_conflict_engine_exception"`.
7. `curl localhost:9200/my-index/_doc/does-not-exist` returns `404` with `error.type == "document_missing_exception"`.
8. `curl -X POST localhost:9200/my-index/_doc -d '{"a":1}'` returns a generated `_id`; a subsequent `GET` on that `_id` returns `found:true` with matching `_source`.
9. `curl -X POST localhost:9200/my-index/_update/1 -d '{"doc":{"a":2}}'` returns `result:"updated"`, `_version` and `_seq_no` both incremented; running it again with an identical `doc` returns `result:"noop"`.
10. `cargo test --workspace` passes: `merge_json` unit tests in `es-index` (shallow overwrite, new keys, nested recursion, scalar/array replacement, no-op equality) and all five `es-mini/tests/document_crud.rs` integration tests (happy path, `_create` 409, missing-doc 404, auto-ID roundtrip, partial-update merge/version/seq_no).

---

**Notes for Tutorial 09 — Bulk Ingest and NDJSON Parsing:**

- `document_put`/`document_create`/`document_delete`/`document_update` on `IndexMetadata` are ready to call directly per parsed bulk action — no new per-document logic should be needed, only NDJSON parsing and per-item response aggregation.
- `IndexResult::as_es_str()` and `doc_write_response`-style envelope construction are reusable for each bulk `items[]` entry; consider factoring `doc_write_response` into something callable from both single-doc handlers and the bulk handler rather than duplicating field mapping.
- `generate_id()` (uuid-based) is the same function bulk `index`/`create` actions without an explicit `_id` need.
- A mixed-action bulk body needs per-item try/catch-equivalent: each action's `Result` should be caught individually (not short-circuit the whole batch via `?`) so one failing item doesn't abort the others — this is the first place in the codebase where an error needs to become *data* (an item in a response array) instead of an early return.
