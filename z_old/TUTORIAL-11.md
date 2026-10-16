# Tutorial 11 — In-Memory Inverted Index and JSON Query DSL

Phase 2, Task 10. Builds on Tutorials 01–10 (workspace, errors, shared state, CRUD, bulk,
analysis). `AnalyzerRegistry` and the `Analyzer` trait from Tutorial 10 are used as-is —
this tutorial is the first thing outside a unit test that actually calls `.analyze()`.

Scope: build an in-memory inverted index in `es-query`, wire document writes to populate it,
define a `Query` enum for the ES JSON DSL, execute it against the index, and expose
`_search`/`_count` over HTTP.

---

## 1. What an inverted index is (recap)

Already familiar as an ES user: instead of scanning every document for a term, maintain a map
from term → list of documents containing it. Lookup is O(1) hash + O(k) list scan instead of
O(n) document scan.

For es-mini, the in-memory version is exactly as simple as it sounds:

```rust
type DocId = String; // reuse Document::id (Tutorial 08), no new ID type

pub struct InvertedIndex {
    postings: HashMap<String, Vec<DocId>>,
}
```

No skip lists, no compression, no block encoding — this is Lucene's actual on-disk postings
format (Phase 4) reduced to "a `HashMap` of `Vec`s." Correct, not fast at scale. That tradeoff is
fine: Phase 4 replaces the storage engine, not the query semantics built on top of it here.

---

## 2. `InvertedIndex` struct

```rust
// es-query/src/index.rs

use std::collections::HashMap;

pub type DocId = String;

#[derive(Debug, Default)]
pub struct InvertedIndex {
    /// term -> doc IDs containing that term (duplicates allowed — see term_frequency below)
    postings: HashMap<String, Vec<DocId>>,
}

impl InvertedIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Index one field's analyzed terms for one document. Called once per text/keyword field,
    /// per document write (§4). Pushes the doc ID once per term *occurrence*, not once per
    /// unique term — that repetition is what makes term-frequency scoring (§6) possible without
    /// a separate counter.
    pub fn index_document(&mut self, doc_id: &str, _field: &str, terms: &[String]) {
        for term in terms {
            self.postings
                .entry(term.clone())
                .or_default()
                .push(doc_id.to_string());
        }
    }

    /// All (doc_id, occurrence) entries for a term — an empty slice if the term was never
    /// indexed. No `Result`/`Option` wrapping: "no postings" is a normal, not exceptional, answer.
    pub fn lookup_term(&self, term: &str) -> &[DocId] {
        self.postings.get(term).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Distinct doc IDs containing `term`, for set-style query logic (§10).
    pub fn matching_docs(&self, term: &str) -> HashSet<DocId> {
        self.lookup_term(term).iter().cloned().collect()
    }

    /// Count of occurrences of `term` in a specific document — used for score-by-term-frequency
    /// (§6). O(postings for that term) since there's no per-doc term-frequency table; fine at
    /// this scale.
    pub fn term_frequency(&self, term: &str, doc_id: &str) -> usize {
        self.lookup_term(term).iter().filter(|d| *d == doc_id).count()
    }
}
```

`_field` is unused today (prefixed `_` so the compiler doesn't warn) — real ES keeps per-field
postings so `{"match": {"title": "x"}}` doesn't also match a `body` field containing `x`.
`// ponytail: single flat postings table, no per-field separation — add a (field, term) composite
key if/when cross-field bleed-through becomes a real test failure`. Flagged, not fixed, because
no query type in this tutorial's scope needs it: every test document uses distinct term
vocabularies per field, so it hasn't bitten yet. Note this explicitly if extending §10's `Match`
handling in a later tutorial.

`HashSet` — new import, `std::collections::HashSet` (§18 covers it properly).

---

## 3. `Mappings` — field type lookup (correction: not actually from Tutorial 03)

`reindex_document` (below) needs to know each field's `FieldType` to pick an analyzer. Tutorial
03 defined the `FieldType` enum itself, but **never** added a `mappings` field to `IndexMetadata`
or any `Mappings` container type — `PUT /{index}` (Tutorial 07) parses a mappings body but
explicitly doesn't apply it (`_settings_body` / `_settings` are dropped on the floor, by design,
at that point). This tutorial is the first thing that actually needs field-type information, so
`Mappings` is introduced here, not reused from an earlier one:

```rust
// es-index/src/mappings.rs (new)

use std::collections::HashMap;
use serde_json::Value;
use crate::FieldType;

#[derive(Debug, Clone, Default)]
pub struct Mappings {
    fields: HashMap<String, FieldType>,
}

impl Mappings {
    pub fn new() -> Self {
        Self::default()
    }

    /// Explicitly- or dynamically-mapped type for `field` on this index, if it's been seen.
    pub fn field_type(&self, field: &str) -> Option<FieldType> {
        self.fields.get(field).cloned()
    }

    /// Record (or overwrite) the mapped type for a field. Called by `reindex_document`'s
    /// dynamic-mapping fallback below; also the hook a real explicit-mappings `PUT /{index}`
    /// body would call into (Tutorial 07 parsed but never applied one — wiring it through to
    /// `set()` is a small follow-up, out of this tutorial's scope).
    pub fn set(&mut self, field: &str, field_type: FieldType) {
        self.fields.insert(field.to_string(), field_type);
    }

    /// Analyzer name for a mapped field: `standard` for `text`, `keyword` (identity) for
    /// everything else, including unmapped fields — matches Tutorial 10's registry, which
    /// only has these two analyzers.
    pub fn analyzer_for(&self, field: &str) -> &'static str {
        match self.fields.get(field) {
            Some(FieldType::Text) => "standard",
            _ => "keyword",
        }
    }

    /// Dynamic-mapping inference from a JSON value's shape — matches real ES's default
    /// `dynamic: true` mapping behavior for a field seen for the first time.
    /// ponytail: coarse inference only (string -> text, integer -> integer, other number ->
    /// float, bool -> boolean; arrays/objects/null aren't dynamically mapped and return
    /// `None`). No real ES-style dynamic *templates* (regex/path-based override rules) —
    /// add if a YAML test exercises one.
    pub fn infer(value: &Value) -> Option<FieldType> {
        match value {
            Value::String(_) => Some(FieldType::Text),
            Value::Number(n) if n.is_i64() || n.is_u64() => Some(FieldType::Integer),
            Value::Number(_) => Some(FieldType::Float),
            Value::Bool(_) => Some(FieldType::Boolean),
            _ => None,
        }
    }
}
```

`es-index/src/lib.rs` needs `mod mappings; pub use mappings::Mappings;` added — the same
lib.rs-export step called out in Tutorial 10 §14.

---

## 4. Wiring analysis into indexing

Every text/keyword field gets analyzed at **write time**, not query time. Tutorial 10 flagged
this decision as open; it's resolved here: an inverted index whose whole purpose is fast lookup
would be pointless if it re-tokenized on every search. Eager indexing, at document-write time.

Data flow for `PUT /{index}/_doc/{id}`:

```
HTTP handler (put_doc)
  -> IndexMetadata::document_put(id, source)     [Tutorial 08, unchanged]
  -> for each field in source:
       field_type = mappings.field_type(field) or dynamically inferred + persisted (§3)
       for each analyzer implied by field_type (see below):
         terms = registry.get(analyzer).analyze(field_value)
         inverted_index.index_document(doc_id, field_name, &terms)
```

```rust
// es-index/src/index_metadata.rs (extends the struct from Tutorial 03/08)

impl IndexMetadata {
    pub fn reindex_document(&mut self, registry: &AnalyzerRegistry, doc_id: &str, source: &Value) {
        let Value::Object(fields) = source else { return };

        for (field_name, field_value) in fields {
            let field_type = match self.mappings.field_type(field_name) {
                Some(ft) => ft,
                None => {
                    // Dynamic mapping: first time this index has seen this field, infer a
                    // type from the value's shape and persist it (§3) — matches real ES's
                    // default `dynamic: true` behavior, rather than silently dropping every
                    // unmapped field.
                    let Some(inferred) = Mappings::infer(field_value) else { continue }; // arrays/objects/null
                    self.mappings.set(field_name, inferred.clone());
                    inferred
                }
            };

            let Some(text) = field_value.as_str() else { continue }; // numeric/boolean/date: not term-indexed

            // Text fields are indexed twice: once analyzed (`standard`, for `match`) and once
            // under their raw lowercased value (`keyword`, for exact-value `term` lookups) —
            // this collapses real ES's implicit `.keyword` multi-field into the single flat
            // postings table (§2's ponytail note) instead of modeling multi-fields properly.
            // Keyword-mapped fields only need the one, already-exact-value form.
            let analyzer_names: &[&str] = match field_type {
                FieldType::Text => &["standard", "keyword"],
                FieldType::Keyword => &["keyword"],
                _ => continue, // numeric/boolean/date: not term-indexed in this tutorial's scope
            };
            for analyzer_name in analyzer_names {
                let Some(analyzer) = registry.get(analyzer_name) else { continue };
                let terms = analyzer.analyze(text);
                self.inverted_index.index_document(doc_id, field_name, &terms);
            }
        }
    }
}
```

Called from every write path that already exists (Tutorial 08's `document_put`/`document_create`/
`document_update`, Tutorial 09's bulk actions) — one extra line after the existing store mutation,
no new HTTP handler needed. `document_delete` needs the mirror operation (remove the doc's
postings) — out of scope for this tutorial's `Query`/`_search` focus; flagged as a known gap:
`// ponytail: deleted documents are not removed from postings — deleted-doc filtering happens at
query time by checking IndexMetadata::documents.contains_key(doc_id) before returning a hit; a
compacting removal pass is the correct fix if the postings table's unbounded growth becomes a
real problem`.

---

## 5. `InvertedIndex` lives on `IndexMetadata`

Each ES index has its own term vocabulary — `products`' inverted index must never answer a query
against `logs`. One `InvertedIndex` per `IndexMetadata`, constructed alongside `documents`:

```rust
// es-index/src/index_metadata.rs

pub struct IndexMetadata {
    pub name: String,
    pub number_of_shards: u32,
    pub number_of_replicas: u32,
    pub uuid: Option<String>,
    pub creation_date: Option<i64>,
    pub documents: HashMap<String, Document>,   // Tutorial 08
    next_seq_no: u64,                            // Tutorial 08
    pub mappings: Mappings,                      // new — see §3
    pub inverted_index: InvertedIndex,           // new
}
```

No separate `DocumentStore`-style sibling struct (same reasoning Tutorial 08 used for
`documents`) — the inverted index has no independent lifecycle from the documents it indexes,
so it lives right beside them.

---

## 6. Relevance scoring (minimal)

Real ES uses BM25 — term frequency, inverse document frequency, and length normalization,
tuned by two free parameters. Implementing it correctly is a multi-day task on its own and buys
nothing until ranking quality is actually being tested (it isn't yet — no YAML test in this
project's scope asserts on relative `_score` ordering).

Scoring implemented here: **raw term frequency**, no IDF, no length norm.

```rust
/// Score for one query term against one document: how many times the term occurs in that doc's
/// indexed fields. Not BM25 — no IDF (rare terms don't score higher), no length normalization
/// (short docs don't score higher for the same term count).
/// ponytail: TF-only scoring, no BM25. Correct enough to rank "has 3 matches" above "has 1
/// match"; wrong the moment two indices need comparable relevance tuning. Upgrade path: add
/// idf = ln(1 + total_docs / docs_containing_term) as a multiplier once a test asserts on it.
fn term_score(index: &InvertedIndex, term: &str, doc_id: &str) -> f64 {
    index.term_frequency(term, doc_id) as f64
}
```

A `Bool` query sums its `must`/`should` clause scores (§10) — same simplification real ES's
`bool` scorer uses in spirit (sum of matching clause scores), just without BM25 underneath each
clause.

---

## 7. Query types to implement

From `EXECUTION.md` Task 10: `match`, `term`, `bool` (`must`/`should`/`must_not`/`filter`),
`range`, plus `from`/`size` and `sort`. `match_all` is added too — every hand-rolled ES client
test suite uses it as the trivial baseline query, and it costs one enum variant.

---

## 8. Representing queries: a closed enum, not a trait

Tutorial 10 flagged this decision explicitly: `Analyzer` needed a trait because analyzers are
open-ended (plugins, custom configs). Query DSL types are **not** open-ended — they're fixed by
the Elasticsearch spec, nobody adds a tenth query type from outside this codebase. An exhaustive
`enum` + `match` is the better fit: the compiler enforces every execution path handles every
variant (§10), with zero `dyn`/vtable overhead.

```rust
// es-query/src/query.rs

use serde_json::Value;

#[derive(Debug, Clone)]
pub enum Query {
    MatchAll,
    Match { field: String, query: String },
    Term { field: String, value: Value },
    Range {
        field: String,
        gte: Option<Value>,
        lte: Option<Value>,
        gt: Option<Value>,
        lt: Option<Value>,
    },
    Bool {
        must: Vec<Query>,
        should: Vec<Query>,
        must_not: Vec<Query>,
        filter: Vec<Query>,
    },
}
```

### Recursive enums need no `Box` here

`Bool` contains `Vec<Query>`, and `Query` contains `Bool` — a directly recursive type. Rust
normally rejects a type that contains itself by value (`Query` inside `Query` would have
infinite size — the classic "recursive type has infinite size" compile error, same shape as a
self-referential Python dataclass without a forward-ref string). The usual fix is `Box<Query>`
(heap-indirection breaks the infinite-size cycle).

Not needed here: `Vec<T>` is *already* a heap-allocated pointer + length + capacity — its own
size is fixed regardless of what `T` is. `Vec<Query>` has a known, fixed size at compile time
even though `Query` (transitively) contains more `Vec<Query>`s. The indirection `Box` would add
is already present, for free, because a list of queries needs to be heap-allocated anyway. Only
a *bare* `Query` field directly inside `Query` (no `Vec`/`Box` around it) would need `Box<Query>`
explicitly — that shape doesn't occur in this enum.

---

## 9. Deserializing the query DSL

The real DSL is irregular nested JSON — `{"bool": {"must": [{"match": {"title": "x"}}]}}` — where
the *query type* is a JSON object key, the same "discriminant is a key, not a tagged field" shape
Tutorial 09 hit for bulk actions. Serde's `#[derive(Deserialize)]` with `#[serde(tag = "...")]`
assumes a fixed tag field name across all variants; this DSL has none. Same conclusion as
Tutorial 09: go through `serde_json::Value` and parse manually.

```rust
// es-query/src/parse.rs

use serde_json::Value;
use crate::query::Query;
use crate::error::QueryError; // thiserror enum, es-query, zero HTTP knowledge — Tutorial 07's precedent

pub fn parse_query(value: &Value) -> Result<Query, QueryError> {
    let obj = value
        .as_object()
        .ok_or_else(|| QueryError::Malformed("query must be a JSON object".into()))?;

    if obj.is_empty() {
        return Err(QueryError::Malformed("query object has no clauses".into()));
    }
    // Real ES rejects multiple top-level keys in one query object too; same rule here.
    if obj.len() > 1 {
        return Err(QueryError::Malformed("query object must have exactly one key".into()));
    }
    let (kind, body) = obj.iter().next().expect("checked non-empty above");

    match kind.as_str() {
        "match_all" => Ok(Query::MatchAll),
        "match" => parse_match(body),
        "term" => parse_term(body),
        "range" => parse_range(body),
        "bool" => parse_bool(body),
        other => Err(QueryError::UnsupportedQueryType(other.to_string())),
    }
}

fn single_field_body(body: &Value) -> Result<(&String, &Value), QueryError> {
    body.as_object()
        .and_then(|o| o.iter().next())
        .ok_or_else(|| QueryError::Malformed("expected a single field key".into()))
}

fn parse_match(body: &Value) -> Result<Query, QueryError> {
    let (field, clause) = single_field_body(body)?;
    // Supports both {"match": {"title": "text"}} and the verbose {"match": {"title": {"query": "text"}}}
    let query_text = clause
        .as_str()
        .map(str::to_string)
        .or_else(|| clause.get("query").and_then(Value::as_str).map(str::to_string))
        .ok_or_else(|| QueryError::Malformed("match clause missing query text".into()))?;
    Ok(Query::Match { field: field.clone(), query: query_text })
}

fn parse_term(body: &Value) -> Result<Query, QueryError> {
    let (field, clause) = single_field_body(body)?;
    // {"term": {"status": "active"}} or {"term": {"status": {"value": "active"}}}
    let value = clause.get("value").cloned().unwrap_or_else(|| clause.clone());
    Ok(Query::Term { field: field.clone(), value })
}

fn parse_range(body: &Value) -> Result<Query, QueryError> {
    let (field, clause) = single_field_body(body)?;
    Ok(Query::Range {
        field: field.clone(),
        gte: clause.get("gte").cloned(),
        lte: clause.get("lte").cloned(),
        gt: clause.get("gt").cloned(),
        lt: clause.get("lt").cloned(),
    })
}

fn parse_bool(body: &Value) -> Result<Query, QueryError> {
    let clause_list = |key: &str| -> Result<Vec<Query>, QueryError> {
        match body.get(key) {
            None => Ok(Vec::new()),
            Some(Value::Array(items)) => items.iter().map(parse_query).collect(),
            Some(single) => Ok(vec![parse_query(single)?]), // ES allows a bare object, not just an array
        }
    };
    Ok(Query::Bool {
        must: clause_list("must")?,
        should: clause_list("should")?,
        must_not: clause_list("must_not")?,
        filter: clause_list("filter")?,
    })
}
```

`clause_list` recurses through `parse_query` for nested `bool` — the `Bool` variant's execution
(§10) and its parser are both naturally recursive, mirroring the JSON's own nesting.

---

## 10. Executing queries

```rust
// es-query/src/execute.rs

use std::collections::HashSet;
use es_index::{AnalyzerRegistry, Document, IndexMetadata};
use crate::query::Query;

pub struct ScoredDoc<'a> {
    pub doc: &'a Document,
    pub score: f64,
}

pub fn execute<'a>(query: &Query, index: &'a IndexMetadata, registry: &AnalyzerRegistry) -> Vec<ScoredDoc<'a>> {
    match query {
        Query::MatchAll => index
            .documents
            .values()
            .map(|doc| ScoredDoc { doc, score: 1.0 })
            .collect(),

        Query::Match { field, query } => execute_match(field, query, index, registry),

        Query::Term { field, value } => execute_term(field, value, index),

        Query::Range { field, gte, lte, gt, lt } => {
            execute_range(field, gte, lte, gt, lt, index)
        }

        Query::Bool { must, should, must_not, filter } => {
            execute_bool(must, should, must_not, filter, index, registry)
        }
    }
}

fn execute_match(field: &str, query_text: &str, index: &IndexMetadata, registry: &AnalyzerRegistry) -> Vec<ScoredDoc<'_>> {
    // `AnalyzerRegistry` is a plain function argument here, not a field on `IndexMetadata` —
    // it's one shared, stateless singleton (`Arc<AnalyzerRegistry>` on `AppState`, Tutorial 10
    // §14), so every index borrows the same instance rather than each carrying its own copy.
    // Same "pass what's needed explicitly" shape `reindex_document` already uses (§4).
    let analyzer_name = index.mappings.analyzer_for(field); // "standard" for text, "keyword" otherwise
    let Some(analyzer) = registry.get(analyzer_name) else { return Vec::new() };
    let terms = analyzer.analyze(query_text);

    // Union of docs matching ANY query term, scored by summed term frequency across all terms.
    let mut scores: std::collections::HashMap<&str, f64> = std::collections::HashMap::new();
    for term in &terms {
        for doc_id in index.inverted_index.matching_docs(term) {
            let tf = index.inverted_index.term_frequency(term, &doc_id);
            *scores.entry(index.documents.get(&doc_id).map(|d| d.id.as_str()).unwrap_or(""))
                .or_insert(0.0) += tf as f64;
        }
    }
    index
        .documents
        .values()
        .filter_map(|doc| scores.get(doc.id.as_str()).map(|&score| ScoredDoc { doc, score }))
        .collect()
}

fn execute_term(field: &str, value: &serde_json::Value, index: &IndexMetadata) -> Vec<ScoredDoc<'_>> {
    match value.as_str() {
        // String term: exact-value lookup via the flat postings table. Matches a text field's
        // additional raw-lowercased indexing pass (§4) or a keyword field's one-and-only form —
        // both were indexed via the `keyword` analyzer's identity-lowercasing behavior.
        Some(text) => {
            let doc_ids = index.inverted_index.matching_docs(text);
            index
                .documents
                .values()
                .filter(|doc| doc_ids.contains(&doc.id))
                .map(|doc| ScoredDoc { doc, score: 1.0 }) // term queries don't score in real ES either (filter context by default)
                .collect()
        }
        // Non-string term (bool/number/etc.): numeric and boolean fields are never written into
        // the postings table (§4 — not term-indexed in this tutorial's scope), so there's nothing
        // to look up there. Compare directly against the stored `_source` value instead — exact
        // equality, no analysis, same document-scan shape `execute_range` already uses below.
        None => index
            .documents
            .values()
            .filter(|doc| doc.source.get(field) == Some(value))
            .map(|doc| ScoredDoc { doc, score: 1.0 })
            .collect(),
    }
}

/// ponytail: O(n) full document scan — there's no range/BKD index yet (that's Lucene's job in
/// Phase 4). Correct for the MVP's data sizes; revisit if range queries show up in a perf test.
fn execute_range<'a>(
    field: &str,
    gte: &Option<serde_json::Value>,
    lte: &Option<serde_json::Value>,
    gt: &Option<serde_json::Value>,
    lt: &Option<serde_json::Value>,
    index: &'a IndexMetadata,
) -> Vec<ScoredDoc<'a>> {
    index
        .documents
        .values()
        .filter(|doc| {
            let Some(actual) = doc.source.get(field) else { return false };
            in_range(actual, gte, lte, gt, lt)
        })
        .map(|doc| ScoredDoc { doc, score: 1.0 })
        .collect()
}

fn in_range(
    actual: &serde_json::Value,
    gte: &Option<serde_json::Value>,
    lte: &Option<serde_json::Value>,
    gt: &Option<serde_json::Value>,
    lt: &Option<serde_json::Value>,
) -> bool {
    let cmp = |bound: &serde_json::Value| -> Option<std::cmp::Ordering> {
        actual.as_f64()?.partial_cmp(&bound.as_f64()?)
    };
    if let Some(b) = gte { if !matches!(cmp(b), Some(std::cmp::Ordering::Greater | std::cmp::Ordering::Equal)) { return false; } }
    if let Some(b) = lte { if !matches!(cmp(b), Some(std::cmp::Ordering::Less | std::cmp::Ordering::Equal)) { return false; } }
    if let Some(b) = gt  { if !matches!(cmp(b), Some(std::cmp::Ordering::Greater)) { return false; } }
    if let Some(b) = lt  { if !matches!(cmp(b), Some(std::cmp::Ordering::Less)) { return false; } }
    true
}

fn execute_bool<'a>(
    must: &[Query],
    should: &[Query],
    must_not: &[Query],
    filter: &[Query],
    index: &'a IndexMetadata,
    registry: &AnalyzerRegistry,
) -> Vec<ScoredDoc<'a>> {
    let doc_ids = |results: &[ScoredDoc<'a>]| -> HashSet<&'a str> {
        results.iter().map(|r| r.doc.id.as_str()).collect()
    };

    let must_results: Vec<Vec<ScoredDoc<'a>>> = must.iter().map(|q| execute(q, index, registry)).collect();
    let filter_results: Vec<Vec<ScoredDoc<'a>>> = filter.iter().map(|q| execute(q, index, registry)).collect();
    let should_results: Vec<Vec<ScoredDoc<'a>>> = should.iter().map(|q| execute(q, index, registry)).collect();
    let must_not_ids: HashSet<&str> = must_not
        .iter()
        .flat_map(|q| execute(q, index, registry))
        .map(|r| r.doc.id.clone())
        .collect::<Vec<_>>()
        .iter()
        .map(String::as_str)
        .collect(); // collected to owned Strings first to avoid borrowing execute()'s temporaries

    // Start from must ∩ filter ∩ ... (intersection of every required clause's doc-ID set).
    // No must/filter clauses at all -> match_all's universe (an empty bool query matches everything).
    let mut candidate_ids: Option<HashSet<&str>> = None;
    for results in must_results.iter().chain(filter_results.iter()) {
        let ids = doc_ids(results);
        candidate_ids = Some(match candidate_ids {
            None => ids,
            Some(existing) => existing.intersection(&ids).copied().collect(),
        });
    }
    let candidate_ids = candidate_ids.unwrap_or_else(|| index.documents.keys().map(String::as_str).collect());

    // should: union of doc IDs from any should clause, only used to filter/boost when should is
    // non-empty AND there's no must/filter (ES rule: should becomes a scoring-only clause when a
    // must/filter clause is also present, but a required-match clause when it's the only clause type).
    let should_ids: HashSet<&str> = should_results.iter().flat_map(|r| doc_ids(r)).collect();
    let should_required = !should.is_empty() && must.is_empty() && filter.is_empty();

    index
        .documents
        .values()
        .filter(|doc| candidate_ids.contains(doc.id.as_str()))
        .filter(|doc| !must_not_ids.contains(doc.id.as_str()))
        .filter(|doc| !should_required || should_ids.contains(doc.id.as_str()))
        .map(|doc| {
            let score: f64 = must_results.iter().chain(should_results.iter())
                .flatten()
                .filter(|r| r.doc.id == doc.id)
                .map(|r| r.score)
                .sum::<f64>()
                .max(1.0); // never score 0 for a matching doc
            ScoredDoc { doc, score }
        })
        .collect()
}
```

`Bool`'s must/filter distinction: both require the clause to match; `filter` skips scoring
(matches ES's own filter-context optimization — filter clauses are cached, unscored booleans in
real ES; here it just means `filter`'s per-clause scores aren't summed into the final `_score`,
which the code above already achieves by construction since only `must`/`should` feed the score
sum).

---

## 11. `SearchRequest`

```rust
// es-mini/src/search.rs

use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Deserialize)]
pub struct SearchRequest {
    #[serde(default = "default_query")]
    pub query: Value,
    #[serde(default)]
    pub from: usize,
    #[serde(default = "default_size")]
    pub size: usize,
    #[serde(default)]
    pub sort: Vec<SortClause>,
}

fn default_query() -> Value {
    serde_json::json!({"match_all": {}})
}
fn default_size() -> usize {
    10 // ES's own default page size
}

impl SearchRequest {
    /// Used by both handlers below (§13, §14) when the request has no JSON body at all —
    /// `#[serde(default = ...)]` only kicks in for *missing fields within a body*, so a
    /// genuinely absent body (`GET /{index}/_search` with no payload) needs its own
    /// all-defaults constructor rather than deserializing `{}`.
    pub fn default_all() -> Self {
        SearchRequest {
            query: default_query(),
            from: 0,
            size: default_size(),
            sort: Vec::new(),
        }
    }
}

/// One sort entry. `{"_score": "desc"}` or `{"price": "asc"}` — either a bare string
/// (`"_score"`, meaning default direction) or a `{"field": "asc"|"desc"}` object. Kept as
/// `Value` and parsed manually in `parse_sort` (§16) for the same "irregular shape, not worth a
/// full derive" reason as the query DSL — no attempt at a fully-typed `SortClause` here, this
/// alias just documents intent at the `SearchRequest` field.
pub type SortClause = Value;
```

`GET /{index}/_search` has no body in the common case — query params (`q=`, `size=`, `from=`)
are the ES "URI search" shorthand. Supporting the full URI-search mini-language is out of scope;
`GET` here accepts an **optional JSON body** only (same `Option<Json<Value>>` extractor pattern
Tutorial 07 established for bodyless `PUT /{index}`), defaulting to `match_all` if absent.

---

## 12. `SearchResponse` / hits envelope

Verified against the ES `_search` API response shape (`hits.total` as an object with `value` +
`relation` — the modern default since ES 7, not the legacy bare-integer form; `_shards` present
even for a single-node cluster):

```rust
// es-mini/src/search.rs (continued)

use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct SearchResponse {
    pub took: u64,
    pub timed_out: bool,
    #[serde(rename = "_shards")]
    pub shards: Shards, // Shards struct reused from Tutorial 08 (Shards::single_node())
    pub hits: HitsEnvelope,
}

#[derive(Debug, Serialize)]
pub struct HitsEnvelope {
    pub total: HitsTotal,
    pub max_score: Option<f64>, // null when there are zero hits, matching real ES
    pub hits: Vec<Hit>,
}

#[derive(Debug, Serialize)]
pub struct HitsTotal {
    pub value: usize,
    pub relation: String, // always "eq" here — no early-termination approximate counting in an MVP
}

#[derive(Debug, Serialize)]
pub struct Hit {
    #[serde(rename = "_index")]
    pub index: String,
    #[serde(rename = "_id")]
    pub id: String,
    #[serde(rename = "_score")]
    pub score: Option<f64>, // null when sorting by a field with no scoring, matching real ES
    #[serde(rename = "_source")]
    pub source: Value,
}
```

`#[serde(rename = "...")]` per field, not `rename_all` — same call Tutorial 08 made for
`DocGetResponse`: ES's underscore-prefixed metadata fields don't share one uniform casing
transform with `took`/`timed_out`/`hits` in the same response tree.

Response shape on the wire:

```json
{
  "took": 1,
  "timed_out": false,
  "_shards": {"total": 2, "successful": 1, "failed": 0},
  "hits": {
    "total": {"value": 3, "relation": "eq"},
    "max_score": 1.0,
    "hits": [
      {"_index": "products", "_id": "1", "_score": 1.0, "_source": {"name": "Widget"}}
    ]
  }
}
```

---

## 13. `POST`/`GET /{index}/_search`

```rust
// es-mini/src/handlers/search.rs

pub async fn search(
    State(state): State<AppState>,
    Path(index_name): Path<String>,
    body: Option<Json<SearchRequest>>,
) -> Result<Json<SearchResponse>, AppError> {
    let started = std::time::Instant::now();
    let req = body.map(|Json(b)| b).unwrap_or_else(SearchRequest::default_all);

    let query = es_query::parse_query(&req.query)?; // QueryError -> AppError via From (§ below)

    let store = state.store.read().unwrap_or_else(|p| p.into_inner());
    let index = store
        .get(&index_name)
        .map_err(|_| AppError::IndexNotFound(index_name.clone()))?;

    let mut scored = es_query::execute(&query, index, &state.analyzers);
    apply_sort(&mut scored, &req.sort);           // §16
    let total = scored.len();
    let page = apply_pagination(scored, req.from, req.size); // §15

    let hits: Vec<Hit> = page
        .into_iter()
        .map(|s| Hit {
            index: index_name.clone(),
            id: s.doc.id.clone(),
            score: Some(s.score),
            source: s.doc.source.clone(),
        })
        .collect();
    let max_score = hits.iter().filter_map(|h| h.score).fold(None, |acc, s| {
        Some(acc.map_or(s, |a: f64| a.max(s)))
    });

    Ok(Json(SearchResponse {
        took: started.elapsed().as_millis() as u64,
        timed_out: false,
        shards: Shards::single_node(),
        hits: HitsEnvelope {
            total: HitsTotal { value: total, relation: "eq".into() },
            max_score,
            hits,
        },
    }))
}
```

Routes:

```rust
.route("/{index}/_search", get(search).post(search))
```

Same handler for both verbs — the only difference between `GET` and `POST /_search` in real ES
is convention (GET-with-body is unusual HTTP but ES has always allowed it); the handler doesn't
need to know which verb was used.

`QueryError` (es-query, `thiserror`) → `AppError` bridge follows Tutorial 07's `IndexStoreError`
precedent exactly:

```rust
// es-mini/src/error.rs
impl From<es_query::QueryError> for AppError {
    fn from(e: es_query::QueryError) -> Self {
        AppError::InvalidRequest(e.to_string()) // 400, action_request_validation_exception
    }
}
```

---

## 14. `POST /{index}/_count`

```rust
#[derive(Debug, Serialize)]
pub struct CountResponse {
    pub count: usize,
    #[serde(rename = "_shards")]
    pub shards: Shards,
}

pub async fn count(
    State(state): State<AppState>,
    Path(index_name): Path<String>,
    body: Option<Json<SearchRequest>>,
) -> Result<Json<CountResponse>, AppError> {
    let req = body.map(|Json(b)| b).unwrap_or_else(SearchRequest::default_all);
    let query = es_query::parse_query(&req.query)?;

    let store = state.store.read().unwrap_or_else(|p| p.into_inner());
    let index = store
        .get(&index_name)
        .map_err(|_| AppError::IndexNotFound(index_name.clone()))?;

    let count = es_query::execute(&query, index, &state.analyzers).len();
    Ok(Json(CountResponse { count, shards: Shards::single_node() }))
}
```

`_count` reuses `execute()` verbatim and just measures the `Vec`'s length before pagination is
applied — this is exactly what makes `_count` agree with `hits.total.value` for the same query
(§ integration test below): both paths call the same `execute()` function, `_count` just skips
the `Hit` envelope construction and the sort/paginate steps that don't affect a bare count.

Wire shape:

```json
{"count": 3, "_shards": {"total": 2, "successful": 1, "failed": 0}}
```

---

## 15. Pagination: `from`/`size`

```rust
fn apply_pagination(mut scored: Vec<ScoredDoc<'_>>, from: usize, size: usize) -> Vec<ScoredDoc<'_>> {
    if from >= scored.len() {
        return Vec::new();
    }
    // ponytail: O(n) drain + truncate over an already-materialized Vec — fine for an in-memory
    // MVP with no persistent result cursor. Real ES's deep-pagination cost (`from + size` must
    // be computed and discarded even for page 1000) is a known, documented limitation of the
    // `from`/`size` API itself, not something es-mini makes worse; `search_after` is the ES fix
    // for it and is out of scope here.
    scored.drain(0..from);
    scored.truncate(size);
    scored
}
```

---

## 16. Sort

Default sort is `_score` descending (already the natural order `execute()` returns docs in for
`match`/`bool` since nothing reorders them yet — but that's incidental `HashMap` iteration order,
not a guarantee, so an explicit sort step is still needed for correctness, not just for
`sort: [...]` requests).

```rust
fn apply_sort(scored: &mut [ScoredDoc<'_>], sort: &[Value]) {
    if sort.is_empty() {
        // default: highest score first
        scored.sort_unstable_by(|a, b| {
            b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal)
        });
        return;
    }
    for clause in sort.iter().rev() {
        // iterate in reverse so the FIRST sort clause in the request ends up as the primary
        // sort key — later clauses run first and get stably overridden. This relies on a stable
        // sort for correctness (see sort_by vs sort_unstable_by note below).
        let (field, ascending) = parse_sort_clause(clause);
        if field == "_score" {
            scored.sort_by(|a, b| {
                let ord = a.score.partial_cmp(&b.score).unwrap_or(std::cmp::Ordering::Equal);
                if ascending { ord } else { ord.reverse() }
            });
            continue;
        }
        scored.sort_by(|a, b| {
            let av = a.doc.source.get(&field);
            let bv = b.doc.source.get(&field);
            let ord = compare_json(av, bv);
            if ascending { ord } else { ord.reverse() }
        });
    }
}

fn parse_sort_clause(clause: &Value) -> (String, bool) {
    match clause {
        Value::String(s) => (s.clone(), true),
        Value::Object(o) => {
            let (field, dir) = o.iter().next().unwrap();
            let ascending = dir.as_str().map(|d| d != "desc").unwrap_or(true);
            (field.clone(), ascending)
        }
        _ => ("_score".to_string(), false),
    }
}

fn compare_json(a: Option<&Value>, b: Option<&Value>) -> std::cmp::Ordering {
    match (a.and_then(Value::as_f64), b.and_then(Value::as_f64)) {
        (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal),
        _ => a.and_then(Value::as_str)
            .unwrap_or("")
            .cmp(b.and_then(Value::as_str).unwrap_or("")),
    }
}
```

Multi-key sort (§ above, "iterate in reverse") is exactly why this uses `sort_by`, **not**
`sort_unstable_by`, for every clause after the first: a stable sort guarantees that ties on the
current key preserve the ordering already established by the previous (higher-priority) key.
`sort_unstable_by` makes no such guarantee — it may reorder equal elements arbitrarily, which
would silently corrupt secondary sort keys. The single-key `_score`-default path (no `sort: [...]`
in the request) has no secondary key to preserve, so it safely uses `sort_unstable_by` — the
faster, no-extra-allocation variant.

---

## 17. `f64`, `NAN`, and `partial_cmp`

Rust's `f64`/`f32` deliberately do **not** implement `Ord` (only `PartialOrd`) — because
`NaN != NaN` and `NaN` compares as neither less-than, greater-than, nor equal-to anything,
including itself. `Ord` requires a *total* order (every pair of values must be comparable);
floats don't have one. `sort()` (which requires `Ord`) flatly refuses to compile on a `Vec<f64>`
— `sort_by`/`sort_unstable_by` (which take a comparator closure, no `Ord` bound) are the only
options, and the closure itself is on the hook for deciding what to do about `NaN`.

`.partial_cmp(&other)` returns `Option<Ordering>` — `None` exactly when either side is `NaN`.
`.unwrap_or(Ordering::Equal)` is the standard escape hatch: treat incomparable values as "equal"
for sort purposes rather than panicking. es-mini's scores are always finite here (`term_score`
never divides, never takes a log — see §6's ponytail note about a future IDF term, which *would*
introduce a real `NaN` risk if a term count and a corpus size both hit zero without a guard), so
this is a defensive default, not something exercised by current data — same category of
"contractual annotation for a case that can't currently happen" as a Python type hint.

### Why `sort_unstable_by` over `sort_by` (when stability doesn't matter)

`sort()`/`sort_by()` are merge-sort-family, guaranteed stable, and allocate a temporary buffer
(`O(n)` extra space). `sort_unstable()`/`sort_unstable_by()` are pattern-defeating quicksort,
typically faster and allocate no extra buffer — but may reorder equal elements. Current stable
Rust guidance (and this project's default): reach for `sort_unstable_by` whenever the input has
no meaningful "original order" to preserve across ties, which is the default case; drop to the
stable variant only when a specific tie-breaking guarantee is required, as §16's multi-key sort
does.

---

## 18. `HashSet` for deduplication: intersection, union

`Bool` query execution (§10) is where set algebra actually earns its keep: `must` = intersection,
`should` = union, `must_not` = set difference.

```rust
use std::collections::HashSet;

let a: HashSet<&str> = ["1", "2", "3"].into_iter().collect();
let b: HashSet<&str> = ["2", "3", "4"].into_iter().collect();

let intersection: HashSet<&str> = a.intersection(&b).copied().collect(); // {"2", "3"}
let union: HashSet<&str> = a.union(&b).copied().collect();               // {"1","2","3","4"}
let difference: HashSet<&str> = a.difference(&b).copied().collect();      // {"1"}
```

`.intersection()`/`.union()`/`.difference()` return **iterators** (borrowing from both sets, not
owned collections) — `.copied()`/`.cloned()` + `.collect()` materializes them, same lazy-until-
`.collect()` pattern as every other iterator adapter chain in this series (Tutorial 10 §2).

`Vec::dedup()` — mentioned for completeness, not used in `execute()`: `dedup()` only removes
**consecutive** duplicates (it's an O(n) single pass, not a full duplicate scan — the same
distinction as Python's `itertools.groupby` vs a full `set()`). Building via `HashSet` from the
start (as `matching_docs()` does, §2) is simpler and correct regardless of ordering; `dedup()`
would require sorting first to bring duplicates adjacent, which is strictly more work here.

---

## 19. Recursive enums, revisited

Point back to §8: `Query::Bool { must: Vec<Query>, ... }` compiles without `Box<Query>` because
`Vec<T>`'s own size (a pointer + length + capacity, 24 bytes on a 64-bit target) never depends on
`T`. The compiler doesn't need to know `Query`'s size to lay out `Vec<Query>` — only `Query`
needs to know `Vec<Query>`'s size to lay out itself, and that's fixed. This is the general rule
for recursive Rust types: any indirection that's already heap-allocated (`Vec<T>`, `Box<T>`,
`HashMap<K, V>`, `String`) breaks a would-be-infinite-size cycle for free; a *bare* field of the
recursive type itself is the only shape that needs an explicit `Box<T>`.

---

## Integration tests

```rust
// es-mini/tests/search.rs
mod common;

use serde_json::json;

async fn seed(server: &axum_test::TestServer) {
    server.put("/products").await;
    server.put("/products/_doc/1").json(&json!({"name": "Red Widget", "price": 10, "in_stock": true})).await;
    server.put("/products/_doc/2").json(&json!({"name": "Blue Widget", "price": 20, "in_stock": true})).await;
    server.put("/products/_doc/3").json(&json!({"name": "Red Gadget", "price": 30, "in_stock": false})).await;
}

#[tokio::test]
async fn match_all_returns_every_document() {
    let server = common::test_server();
    seed(&server).await;
    let resp = server.post("/products/_search").json(&json!({"query": {"match_all": {}}})).await;
    resp.assert_status_ok();
    resp.assert_json_contains(&json!({"hits": {"total": {"value": 3, "relation": "eq"}}}));
}

#[tokio::test]
async fn match_query_finds_docs_by_analyzed_term() {
    let server = common::test_server();
    seed(&server).await;
    let resp = server.post("/products/_search").json(&json!({"query": {"match": {"name": "widget"}}})).await;
    resp.assert_json_contains(&json!({"hits": {"total": {"value": 2, "relation": "eq"}}}));
}

#[tokio::test]
async fn term_query_matches_exact_keyword_value() {
    let server = common::test_server();
    seed(&server).await;
    let resp = server.post("/products/_search").json(&json!({"query": {"term": {"name": "red widget"}}})).await;
    resp.assert_json_contains(&json!({"hits": {"total": {"value": 1, "relation": "eq"}}}));
}

#[tokio::test]
async fn bool_must_and_must_not_intersect_and_exclude() {
    let server = common::test_server();
    seed(&server).await;
    let resp = server
        .post("/products/_search")
        .json(&json!({"query": {"bool": {
            "must": [{"match": {"name": "red"}}],
            "must_not": [{"term": {"in_stock": false}}]
        }}}))
        .await;
    // "Red Widget" matches must (name contains "red") and passes must_not (in_stock: true);
    // "Red Gadget" matches must but is excluded by must_not (in_stock: false).
    resp.assert_json_contains(&json!({"hits": {"total": {"value": 1, "relation": "eq"}}}));
}

#[tokio::test]
async fn range_query_scans_numeric_field() {
    let server = common::test_server();
    seed(&server).await;
    let resp = server
        .post("/products/_search")
        .json(&json!({"query": {"range": {"price": {"gte": 15}}}}))
        .await;
    resp.assert_json_contains(&json!({"hits": {"total": {"value": 2, "relation": "eq"}}}));
}

#[tokio::test]
async fn pagination_applies_from_and_size() {
    let server = common::test_server();
    seed(&server).await;
    let resp = server
        .post("/products/_search")
        .json(&json!({"query": {"match_all": {}}, "from": 1, "size": 1}))
        .await;
    let body: serde_json::Value = resp.json();
    assert_eq!(body["hits"]["hits"].as_array().unwrap().len(), 1);
    assert_eq!(body["hits"]["total"]["value"], 3); // total unaffected by pagination
}

#[tokio::test]
async fn count_agrees_with_search_hits_total() {
    let server = common::test_server();
    seed(&server).await;
    let query = json!({"query": {"match": {"name": "widget"}}});

    let search_resp = server.post("/products/_search").json(&query).await;
    let count_resp = server.post("/products/_count").json(&query).await;

    let search_total = search_resp.json::<serde_json::Value>()["hits"]["total"]["value"].clone();
    let count = count_resp.json::<serde_json::Value>()["count"].clone();
    assert_eq!(search_total, count);
}
```

---

## Definition of Done

1. `InvertedIndex` (in `es-query`) exists with `index_document`, `lookup_term`,
   `matching_docs`, `term_frequency` — unit-tested independent of any HTTP wiring.
2. `IndexMetadata` gains a `pub inverted_index: InvertedIndex` field and a `pub mappings:
   Mappings` field (§3 — not carried over from Tutorial 03, first introduced here), populated on
   every document write path (`document_put`/`document_create`/`document_update`, and each bulk
   action from Tutorial 09) via a `reindex_document` call, including dynamic-mapping inference
   for previously-unmapped fields — verified by a unit test that indexes a document and confirms
   `lookup_term` returns its ID.
3. `Query` enum defined with all five variants (`MatchAll`, `Match`, `Term`, `Range`, `Bool`);
   `Bool`'s `Vec<Query>` fields compile with no `Box<Query>` needed.
4. `parse_query(&Value) -> Result<Query, QueryError>` handles all five query shapes, rejects
   unknown query-type keys and multi-key query objects with a `QueryError` that maps to a 400
   `action_request_validation_exception` via `AppError`.
5. `execute(&Query, &IndexMetadata, &AnalyzerRegistry) -> Vec<ScoredDoc>` implements every
   variant: `match` unions analyzed-term matches; `term` does exact unanalyzed lookup (postings
   for string values, a direct `_source` comparison for bool/numeric values); `bool` correctly
   intersects must/filter, unions should, subtracts must_not; `range` scans and compares;
   `match_all` returns everything.
6. `POST /{index}/_search` and `GET /{index}/_search` both route to the same handler; `GET`
   accepts an optional JSON body and defaults to `match_all` when absent.
7. `POST /{index}/_count` reuses `execute()` and returns `{"count": N, "_shards": {...}}`;
   integration test asserts `count` equals the corresponding `_search`'s `hits.total.value` for
   the same query.
8. `SearchResponse`/`HitsEnvelope`/`HitsTotal`/`Hit` match the real ES `_search` shape:
   `hits.total` is `{value, relation}` (not a bare integer), `max_score` is nullable, each hit
   carries `_index`/`_id`/`_score`/`_source`.
9. `from`/`size` pagination applied after scoring/sorting; a pagination test confirms `size`
   limits `hits.hits.len()` while `hits.total.value` still reflects the full unpaginated count.
10. Sort supports default (`_score` descending) and at least one explicit field sort using
    `partial_cmp` with a documented `NaN` fallback; multi-key sort correctness relies on
    `sort_by`'s stability, contrasted explicitly with `sort_unstable_by`'s use for the
    no-secondary-key default path.
11. Integration tests cover all five query types individually plus one `bool` combination,
    pagination, and the `_count`-agrees-with-`_search` invariant; `cargo test --workspace` green.

---

## Suggestions for Tutorial 12 — Parser Combinators with nom: ES|QL Subset

- `parse_query`'s hand-rolled `serde_json::Value` walk (§9) is deliberately *not* a parser
  combinator — it's a flat, ES-DSL-specific recursive-descent function over already-parsed JSON.
  ES|QL is a genuine text grammar (pipes, string literals, operator precedence) and needs an
  actual parser combinator library (`nom`) — don't reuse this tutorial's approach as a template,
  it solves a different problem (JSON tree walking vs. text grammar parsing).
- `execute()`'s `Query` enum + exhaustive `match` (§10) is a candidate shape for ES|QL's own AST
  (`WHERE`/`LIMIT`/`KEEP`/`DROP`/`SORT` as enum variants over a pipeline) — the "closed enum, not
  a trait, because the grammar is spec-fixed" reasoning from §8 applies again there.
  `InvertedIndex::lookup_term`/`matching_docs` and `execute_range`'s scan-and-compare logic are
  directly reusable for ES|QL's `WHERE` clause — no new storage-layer code needed, only a new
  frontend over the same `IndexMetadata`.
- `Peekable` (Tutorial 10 §2, already used for tokenizer lookahead) will come up again for
  `nom`'s own combinator internals — mention the callback if useful context.
