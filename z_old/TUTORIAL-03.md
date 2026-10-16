# Tutorial 03 — Rust Types, Structs, Enums, and Serde

**Series:** es-mini — Building Elasticsearch in Rust  
**Phase:** 0–1 transition  
**Prerequisites:** Tutorials 01 and 02 complete — workspace builds, CI is green, clippy clean.

---

By the end of this tutorial you will have:

- Rust's type system mapped to concepts you already know from JS/Python
- `IndexMetadata`, `FieldType`, `ClusterHealthResponse`, `IndexInfo`, and `IndexStore` types defined in `es-index`
- Full serde serialization/deserialization for all of them
- Unit tests proving the roundtrip works
- Enough ownership knowledge to write this tutorial's code without fighting the borrow checker

This is the data model foundation. Tutorial 04 (async HTTP with Axum) will wire these types directly into endpoint responses.

---

## 1. Primitive Types

### The numeric zoo

JavaScript has one number type (`number`, a 64-bit float). Python has `int` (arbitrary precision) and `float`. Rust has many, because you're choosing the exact binary representation:

| Rust type | Size | Signed | JS/Python analogue | When to use |
|-----------|------|--------|--------------------|-------------|
| `i32`     | 32-bit | yes  | `number` (small)   | General integers, ES shard counts |
| `i64`     | 64-bit | yes  | `BigInt`           | Document counts, timestamps |
| `u32`     | 32-bit | no   | —                  | Things that can't be negative (shard count) |
| `u64`     | 64-bit | no   | —                  | File sizes, byte offsets |
| `f64`     | 64-bit float | — | `number`         | Scores, percentages |
| `f32`     | 32-bit float | — | —                | Only when memory matters more than precision |
| `usize`   | pointer-width | no | — | Array indices, lengths — always use this for "index into a Vec" |
| `bool`    | 1 byte | — | `boolean` | Same |

**The rule of thumb:** Use `i64` for integers when unsure. Use `u32` when the value is logically unsigned and bounded (like shard count — it can't be -3). Use `usize` for lengths and indices. Use `f64` for floats.

The compiler will not silently convert between these. `let x: i32 = 5; let y: i64 = x;` is a compile error — you write `let y: i64 = x as i64;`. This is deliberate: silent integer coercion is a source of real bugs.

### String types: `&str` vs `String`

This is the single most confusing thing coming from JavaScript (which has one string type) or Python (same). Here's the mental model:

**`String`** is an *owned*, heap-allocated, growable string. Think `new Buffer()` in Node — it holds the memory and is responsible for freeing it. You can push characters onto it, concatenate into it, and pass it around as a value.

**`&str`** (pronounced "string slice") is a *borrowed reference* to string data that lives somewhere else. It's a pointer + length. It doesn't own the data. It's like a view into a `String`, or a reference to a string literal in the binary.

```rust
let owned: String = String::from("my-index");   // heap allocation
let literal: &str = "my-index";                 // points into binary data
let slice: &str = &owned;                        // points into the heap String
let partial: &str = &owned[0..2];               // "my" — a window into owned
```

**Practical rule for this tutorial:** If you're putting a string in a struct that you want to store, use `String`. If you're writing a function that just reads a string and doesn't need to keep it, take `&str`. When in doubt, use `String` — you can always refine later.

```rust
// Struct field: store the string → String
struct IndexMetadata {
    name: String,
}

// Function that just reads: &str avoids forcing callers to allocate
fn print_name(name: &str) {
    println!("{}", name);
}

// You can call it with either:
let meta = IndexMetadata { name: String::from("logs-2024") };
print_name(&meta.name);        // &String coerces to &str automatically
print_name("logs-2024");       // string literals are &str
```

The coercion `&String → &str` happens automatically at function call sites. You don't need to think about it much.

---

## 2. Structs

Rust structs are like TypeScript interfaces but they have methods and they own their data.

### Defining a struct

```rust
pub struct IndexMetadata {
    pub name: String,
    pub number_of_shards: u32,
    pub number_of_replicas: u32,
}
```

**`pub`** on the struct makes it visible outside the module. **`pub`** on each field makes that field accessible outside the module. Without `pub`, a field is private — only code in the same module can read or write it. This is stricter than JavaScript (where properties are always public) but matches Python's convention of `_private` (except Rust enforces it).

### Constructing a struct

Rust doesn't have constructors in the OOP sense. You just fill in the fields:

```rust
let meta = IndexMetadata {
    name: String::from("logs-2024"),
    number_of_shards: 1,
    number_of_replicas: 0,
};
```

Every field must be set. Forget one and the compiler tells you which.

### `impl` blocks — methods on structs

Methods live in `impl` blocks, separate from the struct definition. This is similar to Python's class methods but syntax is different:

```rust
impl IndexMetadata {
    // Associated function (no `self`) — called as IndexMetadata::new(...)
    // This is Rust's convention for constructors
    pub fn new(name: &str, shards: u32, replicas: u32) -> Self {
        IndexMetadata {
            name: name.to_string(),   // &str → String (allocates)
            number_of_shards: shards,
            number_of_replicas: replicas,
        }
    }

    // Method (takes `&self`) — called as meta.display_name()
    pub fn display_name(&self) -> &str {
        &self.name
    }
}
```

`Self` (capital S) is shorthand for the struct type inside `impl` blocks.  
`self` is the instance receiver — like Python's `self` or JS's `this`.  
`&self` means "borrow this instance for reading" — we're not consuming it.

```rust
let meta = IndexMetadata::new("logs-2024", 1, 0);
println!("{}", meta.display_name());  // "logs-2024"
```

### Struct update syntax

If you want a copy of a struct with a few fields changed:

```rust
let meta2 = IndexMetadata {
    name: String::from("logs-2025"),
    ..meta     // copy remaining fields from `meta`
};
```

This is like JavaScript's spread: `{ ...meta, name: "logs-2025" }`. Note: `..meta` must come last.

---

## 3. Enums — Algebraic Data Types

Rust enums are not C-style integer constants or Python's `Enum` class. They are *algebraic data types* — each variant can carry its own data.

For now, let's define a simple tag enum for Elasticsearch field types:

```rust
pub enum FieldType {
    Text,
    Keyword,
    Integer,
    Float,
    Boolean,
    Date,
}
```

### Why not string constants?

In JavaScript you might write:

```js
const FIELD_TYPE_TEXT = "text";
const FIELD_TYPE_KEYWORD = "keyword";
// ... and then pass them around as strings
```

In Rust, the enum approach gives you:

1. **Exhaustiveness checking** — if you `match` on a `FieldType` and forget a variant, the compiler errors.
2. **No typos at runtime** — `FieldType::Texxt` is a compile error; `"texxt"` is a silent bug.
3. **Free documentation** — the type tells you exactly what values are possible.

### `match` — exhaustive pattern matching

`match` is like `switch` in JS/Python but it must cover every case. The compiler enforces this.

```rust
fn index_type_for_mapping(ft: &FieldType) -> &'static str {
    match ft {
        FieldType::Text => "text",
        FieldType::Keyword => "keyword",
        FieldType::Integer => "integer",
        FieldType::Float => "float",
        FieldType::Boolean => "boolean",
        FieldType::Date => "date",
    }
}
```

If you delete one arm, you get:

```
error[E0004]: non-exhaustive patterns: `FieldType::Date` not covered
  --> src/lib.rs:20:11
   |
20 |     match ft {
   |           ^^ pattern `FieldType::Date` not covered
```

This is the compiler catching a bug before it ships. There's no runtime exception — it doesn't compile.

### Enums with data (preview)

Rust enums can carry data per variant — this is where they diverge from every other language. You won't need this heavily in this tutorial, but here's a taste:

```rust
pub enum FieldValue {
    Text(String),
    Integer(i64),
    Float(f64),
    Boolean(bool),
    Null,
}
```

Each variant is its own type. `FieldValue::Text(String::from("hello"))` and `FieldValue::Integer(42)` are the same type (`FieldValue`) but carry different data. You'll use this pattern extensively in Tutorial 10+ when building the inverted index.

---

## 4. `Option<T>` — No More `null`

Rust has no `null`, no `undefined`, no `None` that isn't typed. If a value might be absent, the type system forces you to acknowledge it.

`Option<T>` is an enum with two variants:
- `Some(value)` — there's a value
- `None` — there's no value

```rust
// In JavaScript:       let uuid: string | null = null;
// In Python:           uuid: Optional[str] = None
// In Rust:
let uuid: Option<String> = None;
let uuid2: Option<String> = Some(String::from("abc-123"));
```

### Unwrapping options

```rust
// .unwrap() — get the value, panic if None
let val = uuid2.unwrap();  // "abc-123"

// .expect("message") — same but custom panic message (prefer this over unwrap)
let val = uuid2.expect("uuid should always be set after creation");

// .unwrap_or(default) — get value or fall back — like JS `??` or Python `or`
let val = uuid.unwrap_or(String::from("unknown"));

// .unwrap_or_else(|| ...) — like unwrap_or but lazy (closure runs only if None)
let val = uuid.unwrap_or_else(|| generate_uuid());

// if let — pattern match without the full match block
if let Some(id) = &uuid2 {
    println!("uuid is {}", id);
}

// match
match uuid {
    Some(id) => println!("uuid: {}", id),
    None => println!("no uuid"),
}
```

### Option in structs

Use `Option<T>` for fields that may not always be present:

```rust
pub struct IndexMetadata {
    pub name: String,
    pub number_of_shards: u32,
    pub number_of_replicas: u32,
    pub uuid: Option<String>,       // assigned after creation
    pub creation_date: Option<i64>, // Unix millis, optional
}
```

---

## 5. `Result<T, E>` — Error Handling Without Exceptions

Rust has no exceptions. Functions that can fail return `Result<T, E>`:

- `Ok(value)` — success, contains the value
- `Err(error)` — failure, contains the error

```rust
// In Python:  try: ... except Exception as e: ...
// In JS:      Promise.resolve(val) / Promise.reject(err)
// In Rust:
fn parse_shard_count(s: &str) -> Result<u32, std::num::ParseIntError> {
    s.parse::<u32>()
}
```

### The `?` operator — error propagation

The `?` operator is the key to ergonomic error handling. It means: "if this is `Ok(v)`, unwrap to `v` and continue; if it's `Err(e)`, return the error from this function immediately."

```rust
fn load_index_settings(json: &str) -> Result<IndexMetadata, serde_json::Error> {
    let meta: IndexMetadata = serde_json::from_str(json)?;  // return Err if parse fails
    Ok(meta)
}
```

Compare to Python:

```python
def load_index_settings(json_str: str) -> IndexMetadata:
    try:
        return IndexMetadata(**json.loads(json_str))
    except json.JSONDecodeError as e:
        raise e  # `?` does this automatically
```

The difference: in Rust, the possibility of failure is encoded in the *return type*. Callers can't ignore it — if they don't handle the `Result`, the compiler warns them. In Python, any function can throw; the call site has no compile-time indication of this.

### Reading `Result` at call sites

```rust
// match — explicit handling
match parse_shard_count("3") {
    Ok(n) => println!("shards: {}", n),
    Err(e) => eprintln!("bad shard count: {}", e),
}

// .unwrap() / .expect() — panic on error (fine in tests, not in production)
let n = parse_shard_count("3").expect("shard count in test data is always valid");

// ? — propagate upward (only valid inside a function returning Result)
let n = parse_shard_count("3")?;
```

You'll use `?` constantly once you're writing real code. Think of it as the "I handled this by propagating it" shorthand.

---

## 6. `Vec<T>` and `HashMap<K, V>`

### `Vec<T>` — dynamic array

```rust
// Like Python list or JS Array
let mut shards: Vec<u32> = Vec::new();
shards.push(1);
shards.push(2);
shards.push(3);

println!("len: {}", shards.len());    // 3
println!("first: {}", shards[0]);     // 1

// Iterate
for shard in &shards {
    println!("{}", shard);
}

// The vec! macro — like Python's list literal
let primaries = vec![0u32, 1, 2, 3, 4];
```

`&shards` in the for loop is a borrow (read-only). More on this in section 7.

### `HashMap<K, V>` — hash map

```rust
use std::collections::HashMap;

// Like Python dict or JS Map/object
let mut aliases: HashMap<String, String> = HashMap::new();
aliases.insert(String::from("logs-current"), String::from("logs-2024"));

// Lookup — returns Option<&V>
if let Some(target) = aliases.get("logs-current") {
    println!("alias points to: {}", target);
}

// .entry() for "insert if absent" — like Python's setdefault
aliases.entry(String::from("logs-archive")).or_insert(String::from("logs-2023"));
```

### `BTreeMap<K, V>` — sorted map

`HashMap` has random iteration order (like Python 3.7+ dicts that are insertion-ordered, except Rust's are not). `BTreeMap` iterates in sorted key order. Use it when you need deterministic ordering (e.g., for `_cat/indices` output sorted by index name):

```rust
use std::collections::BTreeMap;

let mut index_map: BTreeMap<String, IndexMetadata> = BTreeMap::new();
// Iteration will be alphabetical by key
```

---

## 7. Ownership — Just Enough to Write This Tutorial

Ownership is Rust's headline feature and the source of most beginner friction. Here's the minimum you need to get through this tutorial without fighting the compiler.

### The three rules (simplified)

1. Every value has one *owner* — the variable that holds it.
2. When the owner goes out of scope, the value is dropped (freed).
3. You can either have one owner (which can move the value around) or you can *borrow* it (via references `&T`).

### Move vs. clone

When you assign a `String` (or any heap-allocated type) to a new variable, Rust *moves* it — the original is no longer valid:

```rust
let a = String::from("logs");
let b = a;          // b now owns the string
println!("{}", a);  // COMPILE ERROR: a was moved
```

Compiler error:

```
error[E0382]: borrow of moved value: `a`
 --> src/lib.rs:4:20
  |
2 |     let a = String::from("logs");
  |         - move occurs because `a` has type `String`
3 |     let b = a;
  |             - value moved here
4 |     println!("{}", a);
  |                    ^ value borrowed here after move
```

If you want both variables to have a copy, call `.clone()`:

```rust
let a = String::from("logs");
let b = a.clone();    // deep copy — now both are valid
println!("{}", a);    // works
println!("{}", b);    // works
```

**When to clone:** When you genuinely need two independent copies. Don't over-clone — it allocates. For this tutorial, you'll mostly pass references.

### References `&T` — borrowing

Instead of moving or cloning, pass a reference:

```rust
fn print_name(meta: &IndexMetadata) {   // borrows, doesn't take ownership
    println!("{}", meta.name);
}

let meta = IndexMetadata::new("logs-2024", 1, 0);
print_name(&meta);    // lend it
print_name(&meta);    // lend it again — still valid because we never moved it
```

`&` is "give me a read-only borrow". The function can read but not mutate. The caller retains ownership.

**Practical rule for this tutorial:** Functions that read a struct should take `&StructName`. Functions that build and return a struct should return `StructName` (owned). When storing something in another struct, store the owned value. If you get a "moved value" error, look at whether you should be passing `&thing` instead of `thing`.

### Copy types don't have this problem

Primitive types (`i32`, `u32`, `f64`, `bool`, `usize`) implement `Copy` — they're copied on assignment, not moved. This is why `let a: i32 = 5; let b = a; println!("{}", a);` compiles fine. Strings and `Vec`s don't implement `Copy` because copying them is non-trivial.

---

## 8. Adding Serde to the Workspace

`serde` is the de-facto serialization framework for Rust. It's what `#[derive(Serialize, Deserialize)]` hooks into. `serde_json` is the JSON backend. They're separate crates — serde is the framework, serde_json is one of many format implementations (there's also serde_yaml, serde_msgpack, etc.).

### Declaring workspace dependencies

In `Cargo.toml` (root), add to `[workspace.dependencies]`:

```toml
[workspace.dependencies]
es-codec  = { path = "es-codec" }
es-query  = { path = "es-query" }
es-index  = { path = "es-index" }
serde     = { version = "1", features = ["derive"] }
serde_json = { version = "1" }
```

`features = ["derive"]` unlocks the `#[derive(Serialize, Deserialize)]` macros. Without it, serde ships only the core traits — the derive macros are opt-in to keep compile times down for crates that use serde's traits manually.

This syntax (declaring versions once at workspace root, having crates inherit with `workspace = true`) was stabilized in Rust 1.64. You're on 1.97.1, so it's fully available.

### Adding to `es-index`

In `es-index/Cargo.toml`:

```toml
[package]
name = "es-index"
version.workspace = true
edition.workspace = true

[dependencies]
es-codec  = { path = "../es-codec" }
serde     = { workspace = true }
serde_json = { workspace = true }
```

`workspace = true` pulls the version (and features) declared in the root. No version duplication, no drift.

---

## 9. Serde Derives and Attributes

### `#[derive(Serialize, Deserialize)]`

This is a *procedural macro* — it generates serialization code at compile time. The ergonomics are similar to Python's `@dataclass` or Pydantic's model, but the serialization code is fully generated at compile time with zero runtime overhead.

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct IndexMetadata {
    pub name: String,
    pub number_of_shards: u32,
    pub number_of_replicas: u32,
}
```

`#[derive(Debug)]` is unrelated to serde — it auto-implements the `Debug` trait so you can print the struct with `{:?}`. Always derive `Debug` on your domain types; you'll thank yourself when debugging.

### Key serde attributes

**`#[serde(rename_all = "snake_case")]`** — renames all fields to snake_case on the wire. Since Rust fields are already snake_case, this mainly affects enum variants (which are PascalCase in Rust but ES expects `"text"`, `"keyword"`, etc.):

```rust
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldType {
    Text,       // serializes as "text"
    Keyword,    // serializes as "keyword"
    Integer,    // serializes as "integer"
    Float,      // serializes as "float"
    Boolean,    // serializes as "boolean"
    Date,       // serializes as "date"
}
```

**`#[serde(rename = "field_name")]`** — rename a specific field. Use this when the ES JSON field name doesn't match what you want in Rust:

```rust
#[derive(Debug, Serialize, Deserialize)]
pub struct ClusterHealthResponse {
    pub cluster_name: String,
    pub status: String,
    pub timed_out: bool,
    pub number_of_nodes: u32,
    pub number_of_data_nodes: u32,
    pub active_primary_shards: u32,
    pub active_shards: u32,
    pub relocating_shards: u32,
    pub initializing_shards: u32,
    pub unassigned_shards: u32,
    pub delayed_unassigned_shards: u32,
    pub number_of_pending_tasks: u32,
    pub number_of_in_flight_fetch: u32,
    pub task_max_waiting_in_queue_millis: u64,
    pub active_shards_percent_as_number: f64,
}
```

ES uses snake_case here so no rename needed — Rust field names match directly.

**`#[serde(skip_serializing_if = "Option::is_none")]`** — omit the field from JSON output if it's `None`. Without this, an `Option<String>` field would serialize as `"field": null`. ES APIs typically omit absent fields entirely:

```rust
#[derive(Debug, Serialize, Deserialize)]
pub struct IndexMetadata {
    pub name: String,
    pub number_of_shards: u32,
    pub number_of_replicas: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub creation_date: Option<i64>,
}
```

**`#[serde(default)]`** — use `Default::default()` if the field is missing during deserialization. Useful for fields ES might omit in some response variants.

---

## 10. Building the Types — `es-index/src/lib.rs`

Now assemble everything. Replace `es-index/src/lib.rs` with:

```rust
//! Index and document model: metadata, mappings, in-memory store.

#![deny(clippy::all)]

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

// ─── Field type enum ───────────────────────────────────────────────────────────

/// The Elasticsearch field type for a mapping field.
///
/// Maps directly to the `"type"` field in ES mapping definitions, e.g.:
/// `{ "properties": { "title": { "type": "text" } } }`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldType {
    Text,
    Keyword,
    Integer,
    Float,
    Boolean,
    Date,
}

// ─── Index metadata ────────────────────────────────────────────────────────────

/// Metadata for a single index: name, shard config, optional runtime fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexMetadata {
    /// The index name, e.g. "logs-2024".
    pub name: String,
    /// Number of primary shards (set at creation, immutable after).
    pub number_of_shards: u32,
    /// Number of replica shards per primary.
    pub number_of_replicas: u32,
    /// UUID assigned by the cluster. None until the index is created.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
    /// Creation timestamp in Unix milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub creation_date: Option<i64>,
}

impl IndexMetadata {
    /// Create a new `IndexMetadata` with the given name and shard config.
    ///
    /// `uuid` and `creation_date` start as `None` — set them after the index
    /// is actually created.
    pub fn new(name: &str, number_of_shards: u32, number_of_replicas: u32) -> Self {
        IndexMetadata {
            name: name.to_string(),
            number_of_shards,
            number_of_replicas,
            uuid: None,
            creation_date: None,
        }
    }

    /// Return the index name as a string slice.
    pub fn name(&self) -> &str {
        &self.name
    }
}

// ─── Cluster health response ───────────────────────────────────────────────────

/// Response body for `GET /_cluster/health`.
///
/// Field names match the actual Elasticsearch JSON response exactly so that
/// `serde_json::to_string(&health)` produces a valid ES-compatible response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterHealthResponse {
    pub cluster_name: String,
    /// "green", "yellow", or "red"
    pub status: String,
    pub timed_out: bool,
    pub number_of_nodes: u32,
    pub number_of_data_nodes: u32,
    pub active_primary_shards: u32,
    pub active_shards: u32,
    pub relocating_shards: u32,
    pub initializing_shards: u32,
    pub unassigned_shards: u32,
    pub delayed_unassigned_shards: u32,
    pub number_of_pending_tasks: u32,
    pub number_of_in_flight_fetch: u32,
    pub task_max_waiting_in_queue_millis: u64,
    pub active_shards_percent_as_number: f64,
}

impl ClusterHealthResponse {
    /// A healthy single-node cluster with no indices.
    pub fn healthy_empty(cluster_name: &str) -> Self {
        ClusterHealthResponse {
            cluster_name: cluster_name.to_string(),
            status: String::from("green"),
            timed_out: false,
            number_of_nodes: 1,
            number_of_data_nodes: 1,
            active_primary_shards: 0,
            active_shards: 0,
            relocating_shards: 0,
            initializing_shards: 0,
            unassigned_shards: 0,
            delayed_unassigned_shards: 0,
            number_of_pending_tasks: 0,
            number_of_in_flight_fetch: 0,
            task_max_waiting_in_queue_millis: 0,
            active_shards_percent_as_number: 100.0,
        }
    }
}

// ─── Index info (_cat/indices style) ──────────────────────────────────────────

/// Per-index info returned by `GET /_cat/indices?format=json`.
///
/// ES returns these as a JSON array. Field names use dot-notation in the
/// original _cat API text output, but the JSON format renames them to
/// underscores. We match the JSON format.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexInfo {
    pub health: String,
    pub status: String,
    pub index: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
    /// Primary shard count (stored as string in ES _cat output)
    #[serde(rename = "pri")]
    pub primary_shards: u32,
    /// Replica count
    #[serde(rename = "rep")]
    pub replicas: u32,
    #[serde(rename = "docs.count", skip_serializing_if = "Option::is_none")]
    pub docs_count: Option<i64>,
    #[serde(rename = "docs.deleted", skip_serializing_if = "Option::is_none")]
    pub docs_deleted: Option<i64>,
    #[serde(rename = "store.size", skip_serializing_if = "Option::is_none")]
    pub store_size: Option<String>,
}

impl IndexInfo {
    /// Build an `IndexInfo` from an `IndexMetadata` for a newly created,
    /// empty index.
    pub fn from_metadata(meta: &IndexMetadata) -> Self {
        IndexInfo {
            health: String::from("green"),
            status: String::from("open"),
            index: meta.name.clone(),
            uuid: meta.uuid.clone(),
            primary_shards: meta.number_of_shards,
            replicas: meta.number_of_replicas,
            docs_count: Some(0),
            docs_deleted: Some(0),
            store_size: None,
        }
    }
}

// ─── serde_json::Value — the escape hatch ─────────────────────────────────────

/// Parse arbitrary ES JSON that doesn't have a typed struct yet.
///
/// `serde_json::Value` is like TypeScript's `any` or Python's `dict` —
/// it can hold any JSON: object, array, string, number, bool, null.
/// Use it when you need to handle dynamic JSON shapes before defining
/// proper types, or when the shape is genuinely unknowable at compile time.
///
/// Example:
/// ```
/// use serde_json::Value;
/// let v: Value = serde_json::from_str(r#"{"index": "logs"}"#).expect("valid json");
/// let name = v["index"].as_str().unwrap_or("unknown");
/// ```
///
/// Prefer typed structs over `Value` wherever the shape is known —
/// `Value` gives up compile-time guarantees.
pub type DynamicJson = serde_json::Value;

// ─── In-memory index store ─────────────────────────────────────────────────────

/// Simple in-memory map of index name → metadata.
///
/// This is the backing store for Phase 1. It will be wrapped in
/// `Arc<RwLock<IndexStore>>` in Tutorial 06 when shared across HTTP handlers.
#[derive(Debug, Default)]
pub struct IndexStore {
    indices: HashMap<String, IndexMetadata>,
}

impl IndexStore {
    pub fn new() -> Self {
        IndexStore::default()
    }

    /// Create an index. Returns `false` if it already exists.
    pub fn create(&mut self, meta: IndexMetadata) -> bool {
        if self.indices.contains_key(&meta.name) {
            return false;
        }
        self.indices.insert(meta.name.clone(), meta);
        true
    }

    /// Get an index by name.
    pub fn get(&self, name: &str) -> Option<&IndexMetadata> {
        self.indices.get(name)
    }

    /// Delete an index. Returns `false` if it didn't exist.
    pub fn delete(&mut self, name: &str) -> bool {
        self.indices.remove(name).is_some()
    }

    /// List all indices, sorted alphabetically.
    pub fn list(&self) -> Vec<&IndexMetadata> {
        let mut v: Vec<&IndexMetadata> = self.indices.values().collect();
        v.sort_by_key(|m| m.name.as_str());
        v
    }

    /// Number of indices.
    pub fn len(&self) -> usize {
        self.indices.len()
    }

    /// True if no indices exist.
    pub fn is_empty(&self) -> bool {
        self.indices.is_empty()
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── FieldType serialization ───────────────────────────────────────────────

    #[test]
    fn field_type_serializes_to_snake_case() {
        assert_eq!(serde_json::to_string(&FieldType::Text).unwrap(), r#""text""#);
        assert_eq!(serde_json::to_string(&FieldType::Keyword).unwrap(), r#""keyword""#);
        assert_eq!(serde_json::to_string(&FieldType::Integer).unwrap(), r#""integer""#);
        assert_eq!(serde_json::to_string(&FieldType::Float).unwrap(), r#""float""#);
        assert_eq!(serde_json::to_string(&FieldType::Boolean).unwrap(), r#""boolean""#);
        assert_eq!(serde_json::to_string(&FieldType::Date).unwrap(), r#""date""#);
    }

    #[test]
    fn field_type_deserializes_from_string() {
        let ft: FieldType = serde_json::from_str(r#""keyword""#).unwrap();
        assert_eq!(ft, FieldType::Keyword);
    }

    #[test]
    fn field_type_roundtrip() {
        for ft in [
            FieldType::Text,
            FieldType::Keyword,
            FieldType::Integer,
            FieldType::Float,
            FieldType::Boolean,
            FieldType::Date,
        ] {
            let json = serde_json::to_string(&ft).unwrap();
            let back: FieldType = serde_json::from_str(&json).unwrap();
            assert_eq!(ft, back);
        }
    }

    // ── IndexMetadata serialization ───────────────────────────────────────────

    #[test]
    fn index_metadata_serializes_without_none_fields() {
        let meta = IndexMetadata::new("logs-2024", 1, 0);
        let json = serde_json::to_string(&meta).unwrap();
        // uuid and creation_date are None — must not appear in output
        assert!(!json.contains("uuid"));
        assert!(!json.contains("creation_date"));
        assert!(json.contains(r#""name":"logs-2024""#));
        assert!(json.contains(r#""number_of_shards":1"#));
    }

    #[test]
    fn index_metadata_serializes_with_some_fields() {
        let meta = IndexMetadata {
            name: String::from("logs-2024"),
            number_of_shards: 3,
            number_of_replicas: 1,
            uuid: Some(String::from("abc-123")),
            creation_date: Some(1_700_000_000_000),
        };
        let json = serde_json::to_string(&meta).unwrap();
        assert!(json.contains(r#""uuid":"abc-123""#));
        assert!(json.contains(r#""creation_date":1700000000000"#));
    }

    #[test]
    fn index_metadata_deserialize_roundtrip() {
        let original = IndexMetadata {
            name: String::from("products"),
            number_of_shards: 5,
            number_of_replicas: 2,
            uuid: Some(String::from("uuid-999")),
            creation_date: None,
        };
        let json = serde_json::to_string(&original).unwrap();
        let back: IndexMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, "products");
        assert_eq!(back.number_of_shards, 5);
        assert_eq!(back.number_of_replicas, 2);
        assert_eq!(back.uuid.as_deref(), Some("uuid-999"));
        assert!(back.creation_date.is_none());
    }

    // ── ClusterHealthResponse serialization ───────────────────────────────────

    #[test]
    fn cluster_health_serializes_with_correct_field_names() {
        let health = ClusterHealthResponse::healthy_empty("es-mini");
        let json = serde_json::to_string(&health).unwrap();
        // Spot-check that actual ES field names are present
        assert!(json.contains(r#""cluster_name":"es-mini""#));
        assert!(json.contains(r#""status":"green""#));
        assert!(json.contains(r#""timed_out":false"#));
        assert!(json.contains(r#""number_of_nodes":1"#));
        assert!(json.contains(r#""active_shards_percent_as_number":100.0"#));
    }

    #[test]
    fn cluster_health_deserialize_roundtrip() {
        // Simulate parsing a real ES response
        let raw = r#"{
            "cluster_name": "my-cluster",
            "status": "yellow",
            "timed_out": false,
            "number_of_nodes": 3,
            "number_of_data_nodes": 3,
            "active_primary_shards": 10,
            "active_shards": 10,
            "relocating_shards": 0,
            "initializing_shards": 2,
            "unassigned_shards": 5,
            "delayed_unassigned_shards": 0,
            "number_of_pending_tasks": 0,
            "number_of_in_flight_fetch": 0,
            "task_max_waiting_in_queue_millis": 0,
            "active_shards_percent_as_number": 66.7
        }"#;
        let health: ClusterHealthResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(health.cluster_name, "my-cluster");
        assert_eq!(health.status, "yellow");
        assert_eq!(health.number_of_nodes, 3);
        assert_eq!(health.active_shards_percent_as_number, 66.7);
    }

    // ── IndexInfo serialization ───────────────────────────────────────────────

    #[test]
    fn index_info_renames_fields_correctly() {
        let meta = IndexMetadata::new("orders", 2, 1);
        let info = IndexInfo::from_metadata(&meta);
        let json = serde_json::to_string(&info).unwrap();
        // pri and rep instead of primary_shards and replicas
        assert!(json.contains(r#""pri":2"#));
        assert!(json.contains(r#""rep":1"#));
        // docs.count uses dot notation
        assert!(json.contains(r#""docs.count":0"#));
        // store.size is None — must not appear
        assert!(!json.contains("store.size"));
    }

    #[test]
    fn index_info_from_metadata_fields() {
        let mut meta = IndexMetadata::new("users", 1, 0);
        meta.uuid = Some(String::from("uuid-abc"));
        let info = IndexInfo::from_metadata(&meta);
        assert_eq!(info.index, "users");
        assert_eq!(info.health, "green");
        assert_eq!(info.status, "open");
        assert_eq!(info.primary_shards, 1);
        assert_eq!(info.replicas, 0);
        assert_eq!(info.uuid.as_deref(), Some("uuid-abc"));
    }

    // ── IndexStore ────────────────────────────────────────────────────────────

    #[test]
    fn index_store_create_and_get() {
        let mut store = IndexStore::new();
        assert!(store.is_empty());
        let created = store.create(IndexMetadata::new("logs", 1, 0));
        assert!(created);
        assert_eq!(store.len(), 1);
        assert!(store.get("logs").is_some());
        assert!(store.get("nonexistent").is_none());
    }

    #[test]
    fn index_store_create_duplicate_returns_false() {
        let mut store = IndexStore::new();
        assert!(store.create(IndexMetadata::new("logs", 1, 0)));
        assert!(!store.create(IndexMetadata::new("logs", 3, 1))); // duplicate
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn index_store_delete() {
        let mut store = IndexStore::new();
        store.create(IndexMetadata::new("logs", 1, 0));
        assert!(store.delete("logs"));
        assert!(!store.delete("logs")); // already gone
        assert!(store.is_empty());
    }

    #[test]
    fn index_store_list_sorted() {
        let mut store = IndexStore::new();
        store.create(IndexMetadata::new("zebra", 1, 0));
        store.create(IndexMetadata::new("alpha", 1, 0));
        store.create(IndexMetadata::new("middle", 1, 0));
        let names: Vec<&str> = store.list().iter().map(|m| m.name()).collect();
        assert_eq!(names, vec!["alpha", "middle", "zebra"]);
    }

    // ── serde_json::Value escape hatch ────────────────────────────────────────

    #[test]
    fn dynamic_json_value_access() {
        let raw = r#"{"index": "logs-2024", "shards": 3}"#;
        let v: serde_json::Value = serde_json::from_str(raw).unwrap();
        assert_eq!(v["index"].as_str().unwrap(), "logs-2024");
        assert_eq!(v["shards"].as_u64().unwrap(), 3);
        assert!(v["missing"].is_null());
    }
}
```

---

## 11. `serde_json::Value` — The Dynamic Escape Hatch

Sometimes you need to handle JSON whose shape you don't know at compile time — for example, when proxying an unknown query body, or during early development before you've modeled a type.

`serde_json::Value` is the fully dynamic JSON type. Think of it as:
- TypeScript's `any`
- Python's `dict` / `list` / `str` etc.
- JavaScript's plain object

```rust
use serde_json::{json, Value};

// Build dynamic JSON with the json! macro
let body: Value = json!({
    "query": {
        "match": {
            "title": "search term"
        }
    },
    "size": 10
});

// Serialize to string
let s = serde_json::to_string(&body).unwrap();

// Deserialize from string
let v: Value = serde_json::from_str(&s).unwrap();

// Navigate with indexing
let size = v["size"].as_u64().unwrap_or(10);
```

**When to use `Value`:** Prototyping, proxying unknown payloads, or fields that are genuinely polymorphic. Once you know the shape, define a typed struct — you get compile-time guarantees and better performance.

### Converting between `Value` and typed structs

```rust
// Value → typed struct
let v: Value = serde_json::from_str(raw_json)?;
let meta: IndexMetadata = serde_json::from_value(v)?;

// typed struct → Value
let meta = IndexMetadata::new("logs", 1, 0);
let v: Value = serde_json::to_value(&meta)?;
```

---

## 12. Reading Compiler Errors

Rust's compiler errors are verbose by design. Here are two you'll hit in this tutorial:

### Missing trait: `Serialize`/`Deserialize`

```
error[E0277]: the trait bound `FieldType: Serialize` is not satisfied
  --> src/lib.rs:15:23
   |
15 |     let s = serde_json::to_string(&field_type)?;
   |             ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |             the trait `Serialize` is not implemented for `FieldType`
   |
help: the following other types implement trait `Serialize`: ...
```

**Fix:** Add `#[derive(Serialize, Deserialize)]` to `FieldType` and `use serde::{Deserialize, Serialize};` at the top of the file.

### Moved value

```
error[E0382]: borrow of moved value: `meta`
  --> src/lib.rs:42:5
   |
38 |     let meta = IndexMetadata::new("logs", 1, 0);
   |         ---- move occurs because `meta` has type `IndexMetadata`
41 |     store.create(meta);
   |                  ---- value moved here
42 |     println!("{}", meta.name);
   |                    ^^^^^^^^^ value borrowed here after move
```

**Fix:** Either pass a reference `store.create(&meta)` (if you update `create` to accept a reference and clone internally), or read the field you need before the move: `let name = meta.name.clone(); store.create(meta); println!("{}", name);`.

### Partial move in struct update

```
error[E0382]: use of partially moved value: `other`
```

This happens when you use `..other` spread syntax but `other` has non-Copy fields you've already moved. Fix: clone the fields explicitly, or clone the whole struct first: `let other2 = other.clone(); ... = IndexMetadata { name: new_name, ..other2 };`.

---

## 13. Putting It Together

After making the changes above, run:

```bash
# From repo root
cargo test -p es-index
```

Expected output:

```
running 15 tests
test tests::cluster_health_deserialize_roundtrip ... ok
test tests::cluster_health_serializes_with_correct_field_names ... ok
test tests::dynamic_json_value_access ... ok
test tests::field_type_deserializes_from_string ... ok
test tests::field_type_roundtrip ... ok
test tests::field_type_serializes_to_snake_case ... ok
test tests::index_info_from_metadata_fields ... ok
test tests::index_info_renames_fields_correctly ... ok
test tests::index_metadata_deserialize_roundtrip ... ok
test tests::index_metadata_serializes_with_some_fields ... ok
test tests::index_metadata_serializes_without_none_fields ... ok
test tests::index_store_create_and_get ... ok
test tests::index_store_create_duplicate_returns_false ... ok
test tests::index_store_delete ... ok
test tests::index_store_list_sorted ... ok

test result: ok. 15 passed; 0 failed; 0 ignored
```

Then run the full workspace to confirm nothing broke:

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

---

## 14. What Was Skipped (and Why)

**Lifetimes** — Rust's explicit lifetime annotations (`'a`) are part of the ownership system but you don't need them to write struct-with-owned-fields code. They appear when you store references inside structs or return references from functions. Deferred to Tutorial 10 (iterators, traits) where they become unavoidable.

**Traits** — Rust's equivalent of interfaces/protocols. `Serialize` and `Deserialize` are traits, but you're using them via `#[derive]` so the implementation is invisible. Tutorial 10 will implement traits manually.

**Error types** — The tutorial uses `serde_json::Error` for simplicity. Tutorial 05 covers building a proper `EsError` enum for the whole crate.

**`Arc<RwLock<IndexStore>>`** — `IndexStore` as written is single-threaded. Tutorial 06 wraps it for concurrent HTTP access.

---

## 15. Definition of Done

Complete this tutorial when all of the following pass:

1. **`es-index/Cargo.toml` declares serde and serde_json as workspace dependencies:**
   ```bash
   grep "workspace = true" es-index/Cargo.toml | wc -l
   # At least 2 lines (serde and serde_json)
   ```

2. **Root `Cargo.toml` declares serde in `[workspace.dependencies]`:**
   ```bash
   grep -A2 '\[workspace.dependencies\]' Cargo.toml | grep serde
   ```

3. **All five structs/types exist in `es-index/src/lib.rs`:**
   ```bash
   grep -E "^pub (struct|enum) " es-index/src/lib.rs
   # Should list: IndexMetadata, ClusterHealthResponse, IndexInfo, FieldType, IndexStore
   ```

4. **All tests pass:**
   ```bash
   cargo test -p es-index
   # test result: ok. N passed; 0 failed
   ```

5. **Full workspace tests pass:**
   ```bash
   cargo test --workspace
   # test result: ok. N passed; 0 failed (across all crates)
   ```

6. **Clippy clean:**
   ```bash
   cargo clippy --workspace --all-targets -- -D warnings
   # Finished ... (no warnings or errors)
   ```

7. **Serialization produces correct ES-compatible JSON:**
   ```bash
   # Run just the JSON shape tests
   cargo test -p es-index cluster_health_serializes
   cargo test -p es-index field_type_serializes
   cargo test -p es-index index_info_renames
   ```

8. **Deserialization roundtrip works (can parse real ES response JSON):**
   ```bash
   cargo test -p es-index roundtrip
   cargo test -p es-index deserialize
   ```

9. **`Option::is_none` fields are absent from JSON output:**
   ```bash
   cargo test -p es-index without_none_fields
   ```

10. **No direct `unwrap()` calls in non-test code** (use `expect()` with a message or `?`):
    ```bash
    awk '/^mod tests/{exit} 1' es-index/src/lib.rs | grep -n "\.unwrap()"
    # Should be empty (all unwraps are inside the tests block)
    ```

---

**Next:** Tutorial 04 — Async Rust: Tokio and Axum. The types defined here become the response bodies for `GET /` and `GET /_cluster/health`. `ClusterHealthResponse` will be the first real JSON response you serve over HTTP.
