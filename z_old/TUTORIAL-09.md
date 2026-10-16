# Tutorial 09 — Bulk Ingest and NDJSON Parsing

**Series:** es-mini — Building Elasticsearch in Rust
**Phase:** 1, Task 8
**Prerequisites:** Tutorials 01–08 complete — `document_put`/`document_create`/`document_delete`/`document_update` exist on `IndexMetadata` (Tutorial 08), `generate_id()` exists in `es-index` (Tutorial 08), `AppError`/`IndexStoreError` bridge pattern exists (Tutorials 05, 07), `Arc<RwLock<IndexStore>>` in `AppState` (Tutorial 06), `build_app` + `tests/common/mod.rs` integration-test scaffolding exists (Tutorial 07).

---

By the end of this tutorial you will have:

- A custom `Ndjson` axum extractor implementing `FromRequest`, splitting a request body on newlines into raw line buffers
- A `BulkActionType` enum and `BulkActionMeta` struct for parsing the ES bulk action line format
- A cursor-based pairing loop that walks action/source line pairs, correctly handling `delete`'s missing source line
- `BulkResponseItem`/`BulkResponse` types matching ES's `items[]` / `errors` response shape
- `POST /_bulk` and `POST /{index}/_bulk` wired to the existing document-op methods from Tutorial 08
- Integration tests covering a mixed-action batch and a batch with one deliberately malformed item

---

## 1. The ES Bulk Wire Format: NDJSON

Every other endpoint so far has been one JSON object per request. Bulk is different: the body is **NDJSON** (newline-delimited JSON) — one *complete* JSON value per line, no enclosing array, no commas between lines.

```
{"index": {"_index": "logs", "_id": "1"}}
{"message": "hello", "level": "info"}
{"create": {"_index": "logs", "_id": "2"}}
{"message": "world", "level": "warn"}
{"update": {"_index": "logs", "_id": "1"}}
{"doc": {"level": "debug"}}
{"delete": {"_index": "logs", "_id": "2"}}
```

Each **action line** (`index`/`create`/`update`/`delete`) is followed by a **source line** — except `delete`, which has no source (there's nothing to write, only a key to remove). The body above is 7 lines encoding 4 operations: index, create, update, delete.

The trailing newline convention matters too: ES requires the body end with `\n`, and most client libraries add it automatically. A parser has to tolerate a trailing empty "line" from that final newline without treating it as a 5th empty action.

**Why NDJSON and not a JSON array of objects?** Streaming. A JSON array requires reading the entire body before you know it's well-formed (a truncated array is invalid JSON). NDJSON lets ES — and this implementation — parse and act on each line as it arrives, and lets one malformed line fail *only that line's operation* instead of invalidating the whole request. That property is the entire reason this tutorial's error handling looks the way it does (§9).

## 2. Two Routes, One Difference: the Default Index

```
POST /_bulk
POST /{index}/_bulk
```

`POST /_bulk` requires every action line to carry its own `"_index"`. `POST /{index}/_bulk` supplies a **default** index — action lines may omit `"_index"` and inherit the path segment, but may still override it per-line if they want to target a different index (real ES allows this; this implementation does too, since it costs nothing extra once `BulkActionMeta._index` is `Option<String>`).

Precedence: `action_line["_index"]` if present, else the path's `{index}`, else (for bare `/_bulk` with no per-line `_index`) a per-item error — not a whole-request failure.

## 3. Content-Type

Real ES requires `Content-Type: application/x-ndjson` or `application/json` on bulk requests — anything else is `406 Not Acceptable`. This implementation's `Ndjson` extractor does **not** check the header.

```rust
// ponytail: real ES rejects bulk bodies whose Content-Type is neither
// application/x-ndjson nor application/json (406). No test here sends a
// bad Content-Type, so the check is skipped. Add a header guard in
// `Ndjson::from_request` the day a test needs it — it's a few lines
// against `req.headers()` before the body is even read.
```

## 4. Why `Json<T>` Doesn't Work Here

Every prior handler extracted a request body with `Json<Value>` (Tutorial 08) — axum deserializes the whole body as one JSON document. NDJSON isn't one JSON document; it's N of them stacked with newlines instead of commas. Handing NDJSON bytes to `serde_json::from_slice::<Value>` directly returns a parse error at the first newline, because `{"index":...}\n{"message":...}` isn't valid JSON on its own (two top-level values, no array wrapper).

So a different extractor is needed — one that reads the raw body and does its own newline-aware parsing instead of delegating to `serde_json` for the whole thing.

**Analogy:** this is the same shape as writing a custom Express body-parser middleware when `express.json()` doesn't fit — `express.json()` assumes "the whole body is one JSON document"; a bulk/NDJSON endpoint needs `req.on('data', ...)` and its own line-splitting, same as here.

## 5. `FromRequest`: axum's Custom-Body-Parser Trait

Axum's built-in extractors (`Json<T>`, `String`, `Bytes`, and now `Ndjson`) all implement one of two traits:

- **`FromRequestParts<S>`** — for extractors that only look at the request's *parts* (method, URI, headers, path params) and never touch the body. `Path<T>` and `Query<T>` (Tutorial 07) are this kind. Because they don't consume the body, a handler can have several of them.
- **`FromRequest<S>`** — for extractors that consume the *body*. `Json<T>`, `Bytes`, `String`, and this tutorial's `Ndjson` are this kind. A request body can only be read once, so **a handler may have at most one `FromRequest` extractor**, and it must be the last argument.

The trait, as defined in axum 0.8.9 (confirmed against the pinned axum version from Tutorial 04):

```rust
pub trait FromRequest<S, M = ViaRequest>: Sized {
    type Rejection: IntoResponse;

    fn from_request(
        req: Request<Body>,
        state: &S,
    ) -> impl Future<Output = Result<Self, Self::Rejection>> + Send;
}
```

- **`type Rejection: IntoResponse`** — the associated error type. It's constrained to `IntoResponse` (the same trait `AppError` implements, Tutorial 05) so axum can turn a failed extraction directly into an HTTP response without the handler ever running. There is no fixed `BoxError`/`Infallible` rejection type mandated by the trait itself — each extractor picks its own; `Ndjson` below reuses `Bytes`'s existing rejection type rather than inventing a new one, since the only way `Ndjson` extraction fails is the same way reading the raw body can fail.
- **`-> impl Future<Output = ...> + Send`** — return-position `impl Trait` in a trait definition (axum 0.8's async-fn-in-trait desugaring). Implementations can just write `async fn from_request(...) { ... }` directly; the compiler handles producing the future.
- **`S`** — the router's state type (`AppState` in this codebase, Tutorial 06). Most extractors are generic over `S`, so they work regardless of what a given app stores in its state.

## 6. The `Ndjson` Extractor

```rust
use axum::{
    body::Bytes,
    extract::{FromRequest, Request},
};

/// Raw NDJSON lines from the request body, trailing/blank lines removed.
/// JSON parsing of each line happens later (§7) — this extractor's only
/// job is splitting bytes on `\n`, the same job `req.on('data')` +
/// manual buffering would do in an Express custom body parser.
pub struct Ndjson(pub Vec<Bytes>);

impl<S> FromRequest<S> for Ndjson
where
    S: Send + Sync,
{
    type Rejection = <Bytes as FromRequest<S>>::Rejection;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let body = Bytes::from_request(req, state).await?;

        let lines = body
            .split(|&b| b == b'\n')
            .filter(|line| !line.is_empty())
            .map(Bytes::copy_from_slice)
            .collect::<Vec<_>>();

        Ok(Ndjson(lines))
    }
}
```

- **`Bytes::from_request(req, state).await?`** — delegates the actual body-read to axum's own `Bytes` extractor (which is itself a `FromRequest` impl, `bytes = "1"` pulled in transitively by axum — no direct dependency needed in `Cargo.toml`, `bytes = "1.12"` is what `cargo tree` shows resolving under axum today). Reusing `Bytes`'s `Rejection` type means `Ndjson` doesn't have to invent its own "body read failed" error variant — if reading the raw body fails (e.g. body too large, connection dropped mid-read), that's exactly the same failure `Bytes` alone would report, so borrowing its `Rejection` type via `<Bytes as FromRequest<S>>::Rejection` is correct, not just convenient.
- **`body.split(|&b| b == b'\n')`** — `Bytes` derefs to `&[u8]`, so the standard slice `split` method (predicate-based, like Python's `str.split` with a custom condition, or JS `Array.prototype.filter` composed with a manual scan) works directly. This produces sub-slices, not new allocations, for each split segment.
- **`.filter(|line| !line.is_empty())`** — drops the empty segment produced by a trailing `\n` (and any accidental blank lines a client sends). Real ES also tolerates a trailing newline; treating it as a phantom empty action would misalign every pairing calculation in §7 and §8.
- **`Bytes::copy_from_slice`** — `split` yields borrowed `&[u8]` slices tied to the original `body`'s lifetime; `.map(Bytes::copy_from_slice)` gives each line its own independently-owned `Bytes` handle (cheap — `Bytes` is refcounted, this bumps a refcount and adjusts an offset, it does not memcpy the underlying buffer) so the `Vec<Bytes>` can outlive the loop that built it and be freely moved into the handler.
- **`.collect::<Vec<_>>()`** — turns the filtered/mapped iterator into a concrete `Vec<Bytes>`. The `::<Vec<_>>` turbofish tells the compiler "collect into a `Vec`, infer the element type from context" — needed here because `collect` is generic over *any* type implementing `FromIterator` (a `HashMap`, a `String`, a `Result`, all qualify), so without an annotation the compiler can't pick one. `_` for the element type (rather than spelling out `Vec<Bytes>`) is the common shorthand once one side of the type is already unambiguous from the pipeline above it.

**Why `serde_json::from_slice`, not `from_str`, appears in §7 and not here:** parsing happens per-line in the handler, once each line's *role* (action vs. source) is known — not in the extractor. `Ndjson` deliberately stays JSON-agnostic; it's a line-splitter, nothing more, so it has exactly one job and exactly one way to fail (the underlying body read failing), which is what keeps its `Rejection` type simple.

## 7. Parsing a Line Without Allocating a `String`

Each line is `Bytes` (owned bytes), not a `String`. `serde_json::from_slice::<T>(&bytes)` parses directly from `&[u8]`, skipping the "decode as valid UTF-8 into a `String`, then re-scan that `String` as JSON" detour `from_str` would require:

```rust
let action_json: serde_json::Value = serde_json::from_slice(&line)?;
```

`from_slice` exists because JSON's grammar is already defined over bytes (JSON is ASCII-superset-safe at the structural level — braces, colons, commas — with UTF-8 only inside string content, which `serde_json` validates as it goes). Requiring a `String` first would mean validating UTF-8 twice: once to build the `String`, once implicitly while parsing. `from_slice` validates once, during parsing, and is the standard choice whenever the source bytes are already in memory as bytes (an HTTP body, a file read via `Vec<u8>`) rather than something that was already a `String` for other reasons.

## 8. Parsing the Action Line

An action line is a JSON object with exactly one key — `"index"`, `"create"`, `"update"`, or `"delete"` — whose value is a metadata object:

```json
{"index": {"_index": "logs", "_id": "1"}}
{"create": {"_index": "logs"}}
{"update": {"_index": "logs", "_id": "1"}}
{"delete": {"_index": "logs", "_id": "2"}}
```

```rust
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BulkActionType {
    Index,
    Create,
    Update,
    Delete,
}

impl BulkActionType {
    fn as_es_str(&self) -> &'static str {
        match self {
            BulkActionType::Index => "index",
            BulkActionType::Create => "create",
            BulkActionType::Update => "update",
            BulkActionType::Delete => "delete",
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct BulkActionMeta {
    #[serde(rename = "_index")]
    index: Option<String>,
    #[serde(rename = "_id")]
    id: Option<String>,
}

/// Parse one action line into its type + metadata. `Value` is used as the
/// entry point (rather than a `#[serde(tag = "...")]` enum) because the
/// "tag" here isn't a fixed field value, it's the *key itself* — serde's
/// internally-tagged/adjacently-tagged enum representations both assume a
/// tag field with a known name, which doesn't fit "the key IS the tag."
fn parse_action_line(bytes: &[u8]) -> Result<(BulkActionType, BulkActionMeta), String> {
    let value: Value = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    let obj = value
        .as_object()
        .ok_or_else(|| "action line must be a JSON object".to_string())?;
    let (key, meta_value) = obj
        .iter()
        .next()
        .ok_or_else(|| "action line must have exactly one key".to_string())?;
    if obj.len() != 1 {
        return Err("action line must have exactly one key".to_string());
    }

    let action_type = match key.as_str() {
        "index" => BulkActionType::Index,
        "create" => BulkActionType::Create,
        "update" => BulkActionType::Update,
        "delete" => BulkActionType::Delete,
        other => return Err(format!("unknown bulk action: {other}")),
    };

    let meta: BulkActionMeta =
        serde_json::from_value(meta_value.clone()).map_err(|e| e.to_string())?;

    Ok((action_type, meta))
}
```

Going through `Value` first (rather than trying to `#[derive(Deserialize)]` a single enum matching all four shapes) is the flexible, low-ceremony option: it handles all four action types with one code path, at the cost of one intermediate `Value` allocation per line — a fine trade at MVP/single-node scale, and the same "escape hatch" role `Value` has played since Tutorial 03.

## 9. Pairing Action and Source Lines

### The naive first draft

Every action line has exactly one source line after it — *except* `delete`. A first attempt reaches for `.chunks(2)`:

```rust
for (i, pair) in lines.chunks(2).enumerate() {
    let action_bytes = &pair[0];
    let source_bytes = pair.get(1); // None if this is the last, odd-length chunk
    // parse pair[0] as the action, pair[1] as the source...
    println!("processing item {i}");
}
```

`.chunks(2)` groups a slice into non-overlapping windows of (up to) 2 elements — the last chunk may be shorter, which is why `pair.get(1)` (not `pair[1]`) is used defensively. Contrast with `.windows(2)`, which produces *overlapping* pairs (`[0,1]`, `[1,2]`, `[2,3]`, ...) — the tool for "look at every adjacent pair," e.g. detecting whether a sorted list stays sorted. Bulk pairing needs non-overlapping consecutive groups, so `.chunks` is the right one; `.windows` would process every source line twice, once as `pair[1]` of its own action and again as `pair[0]` of the (wrong) next chunk.

`.enumerate()` here tags each chunk with its position (`i`) — useful for logging or for an error message like "item 3 failed," the same role Python's `enumerate()` or JS's `.map((x, i) => ...)` index parameter plays.

### Why the naive draft breaks

`.chunks(2)` assumes **every** record is exactly 2 lines. `delete` records are 1 line. The moment a `delete` appears anywhere but the last position, every subsequent pairing shifts by one and the rest of the batch is parsed as garbage — an action line gets treated as a source line, a source line gets treated as the next action line, and every following item fails or, worse, silently does the wrong thing.

Fixing this means walking the lines with a **cursor** that only advances past a source line when the just-read action type actually has one:

```rust
fn process_bulk(
    store: &mut es_index::IndexStore,
    lines: &[Bytes],
    default_index: Option<&str>,
) -> BulkResponse {
    let mut items = Vec::new();
    let mut errors = false;
    let mut cursor = 0;

    while cursor < lines.len() {
        let action_bytes = &lines[cursor];
        cursor += 1;

        let (action_type, meta) = match parse_action_line(action_bytes) {
            Ok(v) => v,
            Err(reason) => {
                // Can't tell whether this malformed line expected a source
                // line to follow — skip only this line and resume at the
                // next one. See the ponytail note below.
                // Fall back to `Index` as the nesting key: the action line
                // itself didn't parse, so its real type is unknowable.
                items.push(
                    BulkItemOutcome::error(
                        BulkActionType::Index, "", "", 400, "parse_exception", reason,
                    )
                    .into_value(),
                );
                errors = true;
                continue;
            }
        };

        let source: Option<Value> = if action_type == BulkActionType::Delete {
            None
        } else {
            match lines.get(cursor) {
                Some(bytes) => {
                    cursor += 1;
                    match serde_json::from_slice::<Value>(bytes) {
                        Ok(v) => Some(v),
                        Err(e) => {
                            items.push(
                                BulkItemOutcome::error(
                                    action_type,
                                    meta.index.as_deref().unwrap_or_default(),
                                    meta.id.as_deref().unwrap_or_default(),
                                    400,
                                    "parse_exception",
                                    e.to_string(),
                                )
                                .into_value(),
                            );
                            errors = true;
                            continue;
                        }
                    }
                }
                None => {
                    items.push(
                        BulkItemOutcome::error(
                            action_type,
                            meta.index.as_deref().unwrap_or_default(),
                            meta.id.as_deref().unwrap_or_default(),
                            400,
                            "parse_exception",
                            "expected source line, found end of body",
                        )
                        .into_value(),
                    );
                    errors = true;
                    continue;
                }
            }
        };

        let item = apply_bulk_action(store, default_index, action_type, meta, source);
        if item.is_error() {
            errors = true;
        }
        items.push(item.into_value());
    }

    BulkResponse { took: 0, errors, items }
}
```

```rust
// ponytail: a malformed action LINE (bad JSON, unknown action key) is
// skipped without knowing whether it was meant to have a source line
// after it. If it was, the next "real" line is now misread as an action
// line, and the rest of the batch can cascade into further per-item
// errors. Real ES has the identical ambiguity for a truncated/corrupt
// action line — there is no way to recover alignment from a line whose
// own shape can't be trusted. Acceptable for this MVP: the failure stays
// contained to per-item error entries (never a whole-batch 500), and no
// test in this tutorial exercises the cascade case specifically.
```

This is the first "iterator method as a first draft, cursor loop as the real fix" moment in the series — `.chunks(2)`/`.windows(2)` are the right *vocabulary* for uniform, fixed-size groupings, but bulk's format isn't uniform, and reaching for the iterator adapter that "looks right" without checking the data shape is exactly the kind of bug that only shows up once a `delete` lands mid-batch in production traffic.

## 10. Applying the Action

```rust
fn apply_bulk_action(
    store: &mut es_index::IndexStore,
    default_index: Option<&str>,
    action_type: BulkActionType,
    meta: BulkActionMeta,
    source: Option<Value>,
) -> BulkItemOutcome {
    let index_name = meta
        .index
        .or_else(|| default_index.map(str::to_string));
    let Some(index_name) = index_name else {
        return BulkItemOutcome::error(
            action_type, "", "", 400, "action_request_validation_exception",
            "no _index specified and no default index in the URL",
        );
    };

    let id = meta.id.unwrap_or_else(es_index::generate_id);

    let meta_result = store.get_mut(&index_name);
    let Ok(index) = meta_result else {
        return BulkItemOutcome::error(
            action_type, &index_name, &id, 404, "index_not_found_exception",
            format!("no such index [{index_name}]"),
        );
    };

    match action_type {
        BulkActionType::Index => {
            let op = index.document_put(id, source.unwrap_or(Value::Null));
            BulkItemOutcome::success(action_type, &index_name, op)
        }
        BulkActionType::Create => match index.document_create(id.clone(), source.unwrap_or(Value::Null)) {
            Ok(op) => BulkItemOutcome::success(action_type, &index_name, op),
            Err(_) => BulkItemOutcome::error(
                action_type, &index_name, &id, 409, "version_conflict_engine_exception",
                format!("[{id}]: version conflict, document already exists"),
            ),
        },
        BulkActionType::Update => {
            let doc = source.and_then(|v| v.get("doc").cloned()).unwrap_or(Value::Null);
            match index.document_update(&id, doc) {
                Ok(op) => BulkItemOutcome::success(action_type, &index_name, op),
                Err(_) => BulkItemOutcome::error(
                    action_type, &index_name, &id, 404, "document_missing_exception",
                    format!("[{id}]: document missing"),
                ),
            }
        }
        BulkActionType::Delete => match index.document_delete(&id) {
            Ok(op) => BulkItemOutcome::success(action_type, &index_name, op),
            Err(_) => BulkItemOutcome::error(
                action_type, &index_name, &id, 404, "document_missing_exception",
                format!("[{id}]: document missing"),
            ),
        },
    }
}
```

This calls the exact same `document_put`/`document_create`/`document_update`/`document_delete` methods Tutorial 08 wrote for the single-document endpoints — bulk is not a new storage code path, only a new *parsing and aggregation* layer sitting in front of the same operations.

## 11. `BulkResponseItem` and the Response Envelope

Tutorial 08's closing notes suggested reusing `doc_write_response`'s field mapping here instead of duplicating it. It doesn't fit as-is: `DocWriteResponse` (Tutorial 08) has non-optional `version`/`result`/`shards`/`seq_no`/`primary_term` fields, since a single-document response is always either a full success body or an `AppError`-rendered error — there's no in-between. A bulk item, by contrast, must serialize *either* the success fields *or* `error`, never both, from one `BulkResponse.items` entry, so those fields need to be `Option<T>` with `skip_serializing_if`. `BulkItemBody` below is that shape; `BulkItemOutcome::success` populates the same field values `doc_write_response` would, just wrapped in `Some(...)`.

```rust
use serde::Serialize;
use serde_json::json;

#[derive(Debug, Serialize)]
struct BulkItemBody {
    #[serde(rename = "_index")]
    index: String,
    #[serde(rename = "_id")]
    id: String,
    #[serde(rename = "_version", skip_serializing_if = "Option::is_none")]
    version: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<String>,
    #[serde(rename = "_shards", skip_serializing_if = "Option::is_none")]
    shards: Option<Shards>, // defined in Tutorial 08
    #[serde(rename = "_seq_no", skip_serializing_if = "Option::is_none")]
    seq_no: Option<u64>,
    #[serde(rename = "_primary_term", skip_serializing_if = "Option::is_none")]
    primary_term: Option<u64>,
    status: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<BulkItemError>,
}

#[derive(Debug, Serialize)]
struct BulkItemError {
    #[serde(rename = "type")]
    error_type: String,
    reason: String,
}

struct BulkItemOutcome {
    action_type: BulkActionType,
    body: BulkItemBody,
}

impl BulkItemOutcome {
    fn success(action_type: BulkActionType, index_name: &str, op: es_index::DocumentOpResult) -> Self {
        let status = match op.result {
            es_index::IndexResult::Created => 201,
            _ => 200,
        };
        BulkItemOutcome {
            action_type,
            body: BulkItemBody {
                index: index_name.to_string(),
                id: op.document.id,
                version: Some(op.document.version),
                result: Some(op.result.as_es_str().to_string()),
                shards: Some(Shards::single_node()),
                seq_no: Some(op.document.seq_no),
                primary_term: Some(op.document.primary_term),
                status,
                error: None,
            },
        }
    }

    fn error(
        action_type: BulkActionType,
        index_name: &str,
        id: &str,
        status: u16,
        error_type: &str,
        reason: impl Into<String>,
    ) -> Self {
        BulkItemOutcome {
            action_type,
            body: BulkItemBody {
                index: index_name.to_string(),
                id: id.to_string(),
                version: None,
                result: None,
                shards: None,
                seq_no: None,
                primary_term: None,
                status,
                error: Some(BulkItemError { error_type: error_type.to_string(), reason: reason.into() }),
            },
        }
    }

    fn is_error(&self) -> bool {
        self.body.error.is_some()
    }

    fn into_value(self) -> Value {
        json!({ self.action_type.as_es_str(): self.body })
    }
}

#[derive(Debug, Serialize)]
struct BulkResponse {
    took: u64,
    errors: bool,
    items: Vec<Value>,
}
```

`reason: impl Into<String>` on `BulkItemOutcome::error` is `impl Trait` used as an *argument* type — plain sugar for a generic parameter, `fn error<R: Into<String>>(..., reason: R)` written more tersely. It means callers can pass either a `String` (the `format!(...)` cases above) or a `&str` literal without the caller having to `.to_string()` first; the function converts once, internally. This shows up in a few places in axum's own extractor code (e.g. handler functions generic over `impl IntoResponse` in some internal helpers) — worth recognizing on sight, not worth reaching for reflexively; a concrete `&str` or `String` parameter is simpler whenever only one call site exists.

**`errors: bool`** — set to `true` the moment any single item's `error` field is populated (see `process_bulk`, §9: `if item.is_error() { errors = true; }`). A client can check this one top-level flag instead of scanning every item — exactly how real ES's bulk clients decide whether to inspect `items[]` at all.

## 12. Partial Failure: Always 200

The bulk endpoint returns **`200 OK`** even when every single item failed. There is no `207 Multi-Status` in the ES bulk API (unlike, say, some batch REST APIs elsewhere) — item-level failure is reported entirely inside the JSON body (`errors: true`, and each failing item's own `status`/`error`). This is why `bulk_no_index`/`bulk_with_index` (§13) return a bare `Json<BulkResponse>`, not `Result<impl IntoResponse, AppError>` — there is no whole-request failure mode left to model once NDJSON line-splitting itself succeeded. A malformed body that isn't valid NDJSON-shaped bytes at all (e.g. genuinely unreadable body) still fails at the `Ndjson` extractor level with axum's normal rejection handling, before the handler ever runs.

## 13. Routes

```rust
use axum::{
    extract::{Path, State},
    response::IntoResponse,
    routing::post,
    Json, Router,
};

async fn bulk_no_index(State(state): State<AppState>, Ndjson(lines): Ndjson) -> impl IntoResponse {
    let mut store = state.store.write().unwrap_or_else(|p| p.into_inner());
    Json(process_bulk(&mut store, &lines, None))
}

async fn bulk_with_index(
    State(state): State<AppState>,
    Path(index): Path<String>,
    Ndjson(lines): Ndjson,
) -> impl IntoResponse {
    let mut store = state.store.write().unwrap_or_else(|p| p.into_inner());
    Json(process_bulk(&mut store, &lines, Some(&index)))
}
```

```rust
Router::new()
    .route("/_bulk", post(bulk_no_index))
    .route("/{index}/_bulk", post(bulk_with_index))
```

`process_bulk`'s signature takes `store: &mut es_index::IndexStore` — one write-lock acquisition covers the whole batch, matching Tutorial 06's "scope the guard, don't hold it across an `.await`" rule; nothing in `process_bulk` is `async`, so holding the guard for its entire (synchronous) body is safe.

## 14. Integration Tests

```rust
// es-mini/tests/bulk.rs
mod common;

use serde_json::json;

fn ndjson_body(lines: &[serde_json::Value]) -> String {
    let mut body = String::new();
    for line in lines {
        body.push_str(&line.to_string());
        body.push('\n');
    }
    body
}

#[tokio::test]
async fn mixed_action_bulk_happy_path() {
    let server = common::test_server();
    server.put("/logs").await.assert_status_ok();

    let body = ndjson_body(&[
        json!({"index": {"_index": "logs", "_id": "1"}}),
        json!({"message": "hello"}),
        json!({"create": {"_index": "logs", "_id": "2"}}),
        json!({"message": "world"}),
        json!({"delete": {"_index": "logs", "_id": "1"}}),
    ]);

    let response = server
        .post("/_bulk")
        .bytes(body.into())
        .content_type("application/x-ndjson")
        .await;

    response.assert_status_ok();
    let json: serde_json::Value = response.json();
    assert_eq!(json["errors"], false);
    assert_eq!(json["items"].as_array().unwrap().len(), 3);
    assert_eq!(json["items"][0]["index"]["result"], "created");
    assert_eq!(json["items"][1]["create"]["result"], "created");
    assert_eq!(json["items"][2]["delete"]["result"], "deleted");

    server.get("/logs/_doc/1").await.assert_status_not_found();
    server
        .get("/logs/_doc/2")
        .await
        .assert_json_contains(&json!({"_source": {"message": "world"}}));
}

#[tokio::test]
async fn bad_item_alongside_good_items_reports_partial_failure() {
    let server = common::test_server();
    server.put("/logs").await.assert_status_ok();

    let body = ndjson_body(&[
        json!({"index": {"_index": "logs", "_id": "1"}}),
        json!({"message": "good"}),
        json!({"bogus_action": {"_index": "logs", "_id": "2"}}),
        json!({"message": "orphaned"}),
        json!({"index": {"_index": "logs", "_id": "3"}}),
        json!({"message": "also good"}),
    ]);

    let response = server
        .post("/_bulk")
        .bytes(body.into())
        .content_type("application/x-ndjson")
        .await;

    // Whole request still succeeds at the HTTP layer.
    response.assert_status_ok();
    let json: serde_json::Value = response.json();
    assert_eq!(json["errors"], true);

    server
        .get("/logs/_doc/1")
        .await
        .assert_json_contains(&json!({"_source": {"message": "good"}}));
}

#[tokio::test]
async fn default_index_from_url_is_used_when_action_omits_it() {
    let server = common::test_server();
    server.put("/logs").await.assert_status_ok();

    let body = ndjson_body(&[
        json!({"index": {"_id": "no-index-here"}}),
        json!({"message": "inherits /logs from the URL"}),
    ]);

    let response = server
        .post("/logs/_bulk")
        .bytes(body.into())
        .content_type("application/x-ndjson")
        .await;

    response.assert_status_ok();
    server
        .get("/logs/_doc/no-index-here")
        .await
        .assert_json_contains(&json!({"found": true}));
}
```

`bad_item_alongside_good_items_reports_partial_failure` is the test this whole tutorial is built around: an unrecognized action key (`"bogus_action"`) in the middle of the batch triggers the §9 ponytail-noted cascade — the `orphaned` source line, never consumed by the cursor, is next misread as its own (also-invalid) action line, so the batch actually produces *two* error items, not one. Both the good item before the bad pair and the good item after it still succeed. The test only asserts `errors == true` and checks the surviving `_doc/1`, deliberately not pinning down the exact item count or indices, since the cascade is the documented, accepted edge case rather than a bug being asserted against.

## 15. Definition of Done

1. `cargo build --workspace` succeeds with `Ndjson`, `BulkActionType`, `BulkActionMeta`, `BulkItemBody`, `BulkItemError`, `BulkItemOutcome`, `BulkResponse`, `parse_action_line`, `process_bulk`, and `apply_bulk_action` added to `es-mini`.
2. `Ndjson` implements `axum::extract::FromRequest<S>`, reusing `Bytes`'s `Rejection` type — no new error type invented for body-read failures.
3. `POST /_bulk` and `POST /{index}/_bulk` both route to handlers that acquire the store's write lock once per request and call `process_bulk`.
4. `cargo clippy --workspace --all-targets -- -D warnings` passes.
5. A mixed-action NDJSON body (`index` + `create` + `delete`) against `POST /_bulk` returns `200`, `"errors": false`, and an `items` array whose length matches the number of actions (not the number of lines).
6. A batch with one unrecognized action key alongside two valid actions returns `200` (not 4xx/5xx), `"errors": true`, and the two valid items both reflect their successful writes.
7. `POST /{index}/_bulk` allows an action line to omit `"_index"` and inherit the URL's index; a per-line `"_index"` still overrides it when present.
8. `delete` actions (no source line) do not shift the parsing of subsequent action/source pairs — verified by a `delete` appearing before the end of a multi-action batch in a test.
9. `curl -X POST localhost:9200/_bulk -H 'Content-Type: application/x-ndjson' --data-binary @body.ndjson` against a real running instance returns a body shaped like `{"took":0,"errors":false,"items":[...]}`.
10. `cargo test --workspace` passes, including the three `es-mini/tests/bulk.rs` integration tests (mixed-action happy path, partial failure, default-index inheritance).

---

**Notes for Tutorial 10 — Text Analysis: Iterators, Traits, and Unicode:**

- The cursor-based pairing loop in `process_bulk` (§9) is the first hand-rolled iteration-with-lookahead pattern in the codebase — Tutorial 10's tokenizer will need a similar "peek at the next item before deciding how much to consume" shape when handling multi-codepoint grapheme boundaries; worth referencing back to this tutorial's naive-`.chunks()`-then-cursor-fix narrative rather than re-deriving it.
- `BulkActionMeta`/`parse_action_line`'s "go through `Value` first, no `#[serde(tag=...)]`" pattern is reusable wherever a JSON shape is keyed by a variable field name rather than a fixed discriminant value.
- No new production dependency added this tutorial — `bytes` (currently resolving to `1.12.x`) arrives transitively via `axum`, confirmed via `cargo search bytes`; no explicit `bytes = "1"` line was added to any `Cargo.toml` since nothing outside axum's own re-export (`axum::body::Bytes`) is used.
