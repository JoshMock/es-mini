# Tutorial 10 — Text Analysis: Iterators, Traits, and Unicode

Phase 2, Task 9. Builds on Tutorials 01–09 (workspace, errors, shared state, CRUD, bulk).

Scope: implement the `keyword` and `standard` analyzers in `es-index`. This is the first
tutorial in the series requiring a hand-written trait — everything before this used
`#[derive(...)]` to get behavior for free. Traits are the mechanism the rest of Phase 2 (query
execution, Tutorial 11) is built on, so this is a load-bearing tutorial.

---

## 1. Traits: Rust's interface mechanism

A trait is a contract: a set of method signatures a type promises to implement.

```rust
trait Analyzer {
    fn analyze(&self, text: &str) -> Vec<String>;
}
```

Any type can implement it:

```rust
struct KeywordAnalyzer;

impl Analyzer for KeywordAnalyzer {
    fn analyze(&self, text: &str) -> Vec<String> {
        vec![text.to_lowercase()]
    }
}
```

### Traits vs TypeScript interfaces vs Python ABCs

| | TypeScript `interface` | Python `ABC` | Rust `trait` |
|---|---|---|---|
| Typing | structural (duck typing) — any object with matching shape satisfies it, no declaration needed | nominal — must subclass explicitly | nominal — must `impl Trait for Type` explicitly, even for existing types |
| Enforcement | compile-time (erased at runtime) | run-time (`TypeError` on instantiation if abstract methods unimplemented) | compile-time, zero runtime cost |
| Can implement for types you don't own | no (interface only describes shape) | no (must subclass) | **yes** — `impl Display for MyExternalType` is legal (subject to the orphan rule: one of the trait or the type must be local to your crate) |
| Default method bodies | no | yes (concrete methods on the ABC) | yes (§5 below) |
| Multiple implementations per type | trivially, it's just shape-matching | multiple inheritance, real but messier | multiple traits, no conflict — each `impl` block is separate |

The practical difference from TypeScript: writing a struct with an `analyze` method does **not**
make it an `Analyzer`. `impl Analyzer for KeywordAnalyzer` must exist somewhere, explicitly. This
catches "coincidentally shaped" bugs that structural typing lets slide.

### Why a trait, not an enum, for analyzers

An enum (`enum AnalyzerKind { Keyword, Standard }` + a big `match`) works today, for two variants.
It breaks the moment analysis needs to be extensible (custom analyzers, per-field analyzer
config, a plugin-style registry) — every new analyzer means editing the enum and every `match`
arm that dispatches on it. A trait is *open*: new analyzers are new structs plus one `impl` block,
touching zero existing code. `AnalyzerRegistry` (§16) is the payoff.

---

## 2. Trait objects: `dyn Trait` and runtime polymorphism

`AnalyzerRegistry` needs to hold *different concrete types* (`KeywordAnalyzer`, `StandardAnalyzer`)
behind one handle. Generics (`fn f<A: Analyzer>(a: A)`) pick a single concrete type at compile
time per call site — no good for "a `HashMap` where the value type varies per entry."

`dyn Analyzer` is the answer: an existential type, "some type that implements `Analyzer`, decided
at runtime." Since its size isn't known at compile time, it must live behind a pointer —
`Box<dyn Analyzer>` (heap-allocated) or `&dyn Analyzer` (borrowed).

```rust
let analyzers: Vec<Box<dyn Analyzer>> = vec![
    Box::new(KeywordAnalyzer),
    Box::new(StandardAnalyzer),
];
for a in &analyzers {
    println!("{:?}", a.analyze("Hello World"));
}
```

Analogy: a Python `List[AnalyzerBase]` where each element is some subclass instance — you don't
know or care which subclass, you just call the interface method and dynamic dispatch resolves it.
`Box<dyn Trait>` is that, minus a garbage collector: the box owns its heap allocation and frees it
on drop.

**Generics vs `dyn Trait`:**

| | Generics (`impl<A: Analyzer>` / `fn f<A: Analyzer>`) | `dyn Trait` |
|---|---|---|
| Dispatch | static — monomorphized, one specialized copy of the function per concrete type used | dynamic — vtable lookup at each call |
| Performance | faster, inlinable | small indirection cost, rarely matters here |
| Heterogeneous collections (`Vec` of mixed types) | no — one `T` per instantiation | yes, this is the reason to reach for it |
| Binary size | larger (code duplicated per type) | smaller (one copy, called through a pointer) |

Rule of thumb: reach for generics when the concrete type is known at the call site and only
performance matters; reach for `dyn Trait` when the concrete type varies at runtime and needs to
be stored — exactly `AnalyzerRegistry`'s situation.

**Object safety, briefly:** not every trait can become a `dyn Trait`. A trait is *object-safe*
only if none of its methods take `Self` by value, return `Self`, or use generic type parameters —
the vtable has no way to represent "the concrete type" once erased. `Analyzer` as defined above
(only `&self`, only concrete argument/return types) is object-safe. If this ever bites, the
compiler error names it explicitly ("the trait cannot be made into an object").

---

## 3. `impl Trait` in return position

Two ways to return "something that implements `Analyzer`":

```rust
fn make_keyword() -> impl Analyzer {
    KeywordAnalyzer
}

fn make_analyzer(name: &str) -> Box<dyn Analyzer> {
    match name {
        "keyword" => Box::new(KeywordAnalyzer),
        _ => Box::new(StandardAnalyzer),
    }
}
```

`impl Analyzer` (no `dyn`, no `Box`) commits to **one concrete return type**, decided at compile
time, even though the caller doesn't get to name it — the compiler still monomorphizes. It
compiles only if every `return` path in the function produces the *same* concrete type.
`make_analyzer` above could not use `impl Analyzer` — the two branches return different concrete
types (`KeywordAnalyzer` vs `StandardAnalyzer`), and `impl Trait` requires a single one. `Box<dyn
Analyzer>` is required whenever the concrete type genuinely varies by branch — which is exactly
the `AnalyzerRegistry` construction case.

---

## 4. Default trait methods

A trait method can ship a body; implementers may override it or take it for free:

```rust
trait Analyzer {
    fn analyze(&self, text: &str) -> Vec<String>;

    fn name(&self) -> &str {
        "unnamed"
    }
}
```

`KeywordAnalyzer` and `StandardAnalyzer` both override `name()` (§10–11); a hypothetical
third analyzer that doesn't bother gets `"unnamed"` for free. Same idea as an ABC's concrete
method that subclasses may or may not override — except Rust enforces at compile time that the
*abstract* methods (no body, `analyze` here) are always implemented, while default methods are
optional.

---

## 5. The `Iterator` trait

Every `for` loop, every `.map()`/`.filter()` chain, and Python-generator-style lazy sequences in
Rust all boil down to one trait:

```rust
trait Iterator {
    type Item;
    fn next(&mut self) -> Option<Self::Item>;
    // dozens of default methods (map, filter, ...) built on next()
}
```

`type Item` is an *associated type* — each implementer picks what it iterates over once, rather
than the caller choosing per call (contrast with a generic parameter). `next()` is the only method
that must be implemented; everything else (`.map()`, `.filter()`, `.collect()`, ~70 more) is a
default method built purely in terms of `next()`.

A custom tokenizer stream, implemented directly against the trait:

```rust
struct TokenStream<'a> {
    remaining: &'a str,
}

impl<'a> Iterator for TokenStream<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<&'a str> {
        let trimmed = self.remaining.trim_start();
        let end = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
        if end == 0 {
            return None;
        }
        let (token, rest) = trimmed.split_at(end);
        self.remaining = rest;
        Some(token)
    }
}
```

Once `Iterator` is implemented, the whole adapter toolkit (`.map()`, `.take(3)`, `.collect()`, a
plain `for tok in TokenStream { .. }`) works for free — this is the same deal as Python's
`__next__`/`StopIteration` protocol or a JS iterable's `next()` returning `{done, value}`, except
the "done" signal is `Option::None` instead of a separate flag, and there's no exception involved.

`TokenStream` above is a teaching example (whitespace-only splitting). The real tokenizer (§9,
§11) does not hand-roll this — Unicode word boundaries are not whitespace-shaped.

---

## 6. Iterator adapters, chained without intermediate allocation

```rust
let result: Vec<String> = "The Quick, Brown Fox!!"
    .split_whitespace()
    .map(str::to_lowercase)
    .filter(|w| w.chars().any(|c| c.is_alphanumeric()))
    .collect();
```

Every adapter (`.map`, `.filter`, `.flat_map`, `.chain`, `.take`, `.skip`, `.enumerate`,
`.peekable`) returns a new iterator wrapping the previous one — no `Vec` is built until `.collect()`
runs. This is the same laziness as a Python generator expression chain or a JS `Array.prototype`
chain *without* the JS eager-array-per-step cost — `map`/`filter` in JS on real arrays allocate a
new array every step; Rust's chain does not, it's all one pass at `.collect()` time.

| Adapter | Shape | Use here |
|---|---|---|
| `.map(f)` | `Item -> U` | lowercase each token |
| `.filter(pred)` | keep where `pred(&Item)` | drop empty/punctuation-only tokens |
| `.flat_map(f)` | `Item -> Iterator<Item = U>`, flattened | not needed for single-field analysis, useful once multi-value fields tokenize per value |
| `.chain(other)` | concatenate two iterators | not used yet |
| `.take(n)` / `.skip(n)` | first/last `n` | pagination-adjacent, not analysis |
| `.enumerate()` | `(index, Item)` | debug/position tracking in tests |
| `.peekable()` | adds `.peek()` | tokenizer look-ahead (§8) |

---

## 7. `Peekable<I>` — look-ahead without consuming

```rust
let mut it = "a1 b2".chars().peekable();
while let Some(&c) = it.peek() {
    if c.is_alphabetic() {
        it.next(); // consume it now that the decision is made
    } else {
        break;
    }
}
```

`.peek()` returns `Option<&Self::Item>` — a look at the next item without advancing the iterator.
This is the exact gap Tutorial 09 hit with `.chunks(2)`: bulk NDJSON needed to look at an action
line to decide whether a source line follows, and `.chunks(2)` couldn't express "maybe consume the
next one." `Peekable` is the iterator-native version of that same look-before-you-consume shape,
and it is the standard tool for tokenizers in general (decide where a token ends by peeking at
the next character/grapheme before consuming it). `unicode-segmentation` (§11) does this
internally so this tutorial's code doesn't have to — but recognizing the pattern matters for
Tutorial 11's query parser and Tutorial 12's ES|QL parser, both of which will peek before
consuming tokens.

---

## 8. `char` vs bytes — Rust strings are UTF-8

A Rust `&str`/`String` is a UTF-8 byte buffer, not an array of characters. Indexing it by integer
(`text[3]`) **does not compile** — `str` doesn't implement `Index<usize>` at all, precisely
because "byte 3" is meaningless as "character 3" once anything outside ASCII is in play, and
slicing on a non-boundary panics at runtime (`byte index 3 is not a char boundary`).

```rust
let s = "café";
// s[3] — compile error, str isn't indexed by usize

for c in s.chars() {
    println!("{c}"); // c, a, f, é — 4 chars, 5 bytes (é is 2 bytes in UTF-8)
}

for (byte_offset, c) in s.char_indices() {
    println!("{byte_offset}: {c}");
}
// 0: c
// 1: a
// 2: f
// 3: é   <- byte offset 3, but 'é' occupies bytes 3-4
```

- `char` is always 4 bytes in memory (`u32`-sized) — a Unicode *scalar value*, enough to hold any
  code point. This is not the same as "4 bytes on disk" — UTF-8 encoding is variable-width (1–4
  bytes per character); `char` is the in-memory decoded representation.
- `.chars()` iterates Unicode scalar values (not bytes, not grapheme clusters — a single emoji
  with a skin-tone modifier is *multiple* `char`s, one `Iterator::next()` call each).
- `.char_indices()` gives `(byte_offset, char)` — the byte offset is what's needed for slicing
  back into the original `&str` (`&s[start..end]`), since slice bounds are always byte indices.
- Never build an offset via character count and slice with it directly; always carry byte offsets
  through, which is exactly what `char_indices()` (and `unicode-segmentation`'s APIs, §11) hand
  back.

---

## 9. `Analyzer` trait, finalized

```rust
// es-index/src/analysis/mod.rs
pub trait Analyzer {
    fn analyze(&self, text: &str) -> Vec<String>;

    fn name(&self) -> &str {
        "unnamed"
    }
}
```

---

## 10. `KeywordAnalyzer`

Identity analyzer — one token, the whole input, lowercased. Matches ES's `keyword` analyzer.

```rust
pub struct KeywordAnalyzer;

impl Analyzer for KeywordAnalyzer {
    fn analyze(&self, text: &str) -> Vec<String> {
        vec![text.to_lowercase()]
    }

    fn name(&self) -> &str {
        "keyword"
    }
}
```

Empty input still produces one token (`""`) — matches real ES keyword-analyzer behavior; no
filtering happens for keyword fields.

---

## 11. `StandardAnalyzer` — UAX#29 word segmentation

Real Unicode word boundary detection (UAX#29: Unicode Text Segmentation, Annex 29) is a ~30-page
standard covering scripts, combining marks, emoji sequences, and locale-independent rules for
where a "word" starts and ends. Reimplementing it by hand is a multi-week correctness trap with a
huge edge-case surface (Thai has no spaces between words; CJK segmentation isn't
whitespace-delimited at all; apostrophes inside contractions are word-internal in English but not
universally). This is squarely rung 4 of the ladder: an already-adjacent, purpose-built crate
solves it — don't write it.

```toml
# es-index/Cargo.toml
[dependencies]
unicode-segmentation = { workspace = true }
```

```toml
# root Cargo.toml [workspace.dependencies]
unicode-segmentation = "1.13"
```

Version verified: **unicode-segmentation 1.13.3** (current on crates.io at time of writing).

```rust
use unicode_segmentation::UnicodeSegmentation;

pub struct StandardAnalyzer;

impl Analyzer for StandardAnalyzer {
    fn analyze(&self, text: &str) -> Vec<String> {
        text.unicode_words()
            .map(str::to_lowercase)
            .collect()
    }

    fn name(&self) -> &str {
        "standard"
    }
}
```

**`unicode_words()` vs `split_word_bounds()`:**

| Method | Returns | Includes whitespace/punctuation as tokens? |
|---|---|---|
| `split_word_bounds()` | every segment between word boundaries | yes — `"Hi, Bob!"` → `["Hi", ",", " ", "Bob", "!"]` |
| `unicode_words()` | only segments containing at least one alphanumeric | no — `"Hi, Bob!"` → `["Hi", "Bob"]` |

`unicode_words()` is the correct primitive here — it's `split_word_bounds()` with the
punctuation/whitespace-only segments already filtered, matching what `StandardTokenizer` (the
Lucene component ES's `standard` analyzer is built on) produces before the lowercase filter runs.

**Known divergences from Lucene's `StandardTokenizer`:**

```rust
// ponytail: unicode-segmentation implements UAX#29 word-boundary rules directly and
// matches Lucene's StandardTokenizer for the common cases this crate targets (ASCII,
// CJK, basic punctuation/whitespace splits). Two known gaps, both narrow:
//   1. Lucene's StandardTokenizer additionally recognizes URL/email tokens as single
//      units in some configurations (via its JFlex grammar); unicode-segmentation has
//      no such special-casing and will split on the embedded punctuation.
//   2. Numeric tokens with embedded commas/periods (e.g. "3,000.50") may segment
//      differently at locale-sensitive punctuation depending on Unicode version pinned
//      by each side (ICU behind Lucene vs unicode-segmentation's own tables).
// Corpus tests (below) pin known-good cases; if a real byte-mismatch surfaces against
// the actual Lucene output, add it to KNOWN_DIVERGENCES with the Lucene-observed value
// and treat it as an explicit documented gap, not a silent test skip.
```

This satisfies the EXECUTION.md requirement ("document the divergence") without inventing failure
modes that haven't been observed — the two gaps above are the commonly cited limitations of
UAX#29-only tokenizers relative to Lucene's grammar-based one; if the corpus test (§14) turns up
more, add them here.

---

## 12. Token filtering as an adapter chain

Post-tokenization cleanup — drop empty tokens, drop punctuation-only tokens, lowercase — written
as one chain, not a loop with `if`s:

```rust
pub fn filter_tokens<'a>(tokens: impl Iterator<Item = &'a str>) -> Vec<String> {
    tokens
        .map(str::to_lowercase)
        .filter(|t| !t.is_empty())
        .filter(|t| t.chars().any(|c| c.is_alphanumeric()))
        .collect()
}
```

`unicode_words()` already excludes punctuation-only segments (§11), so `StandardAnalyzer` doesn't
need this chain today — it's included because `KeywordAnalyzer`-style analyzers built on
`split_word_bounds()` (finer-grained splitting, e.g. a future `simple` analyzer) will need exactly
this filter, and it's the idiomatic shape for it: three lazy adapters, one `.collect()`, no
intermediate `Vec`.

---

## 13. Table-driven tests

Same shape as a Go `t.Run` table test or a pytest `@pytest.mark.parametrize` — one array of
`(input, expected)`, one loop, one assertion:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_analyzer_corpus() {
        let cases: &[(&str, &[&str])] = &[
            ("Hello World", &["hello", "world"]),
            ("café", &["café"]),
            ("don't stop", &["don't", "stop"]),
            ("3,000.50 dollars", &["3,000.50", "dollars"]),
            ("你好世界", &["你", "好", "世", "界"]), // each CJK ideograph is its own segment, not merged
            ("  leading  and   trailing  ", &["leading", "and", "trailing"]),
            ("", &[]),
            ("!!!", &[]),
            ("MiXeD CaSe", &["mixed", "case"]),
        ];

        let analyzer = StandardAnalyzer;
        for (input, expected) in cases {
            let actual = analyzer.analyze(input);
            assert_eq!(&actual, expected, "input: {input:?}");
        }
    }

    #[test]
    fn keyword_analyzer_is_identity_lowercased() {
        let cases: &[(&str, &str)] = &[
            ("Hello World", "hello world"),
            ("", ""),
            ("ALREADY-LOWER-ish", "already-lower-ish"),
        ];

        let analyzer = KeywordAnalyzer;
        for (input, expected) in cases {
            assert_eq!(analyzer.analyze(input), vec![expected.to_string()]);
        }
    }
}
```

CJK note: `unicode_words()` (verified against `unicode-segmentation` 1.13.3) splits an
unbroken run of CJK ideographs into one segment **per character** — it does not merge them
into a single word. UAX#29 has no explicit word-break opportunity *between* two CJK ideographs
with no separator, which might suggest they'd stay joined, but the crate's word-break
implementation treats each ideograph as its own boundary-delimited segment regardless. This
happens to match `StandardTokenizer`'s default behavior too — both emit one token per CJK
character rather than attempting dictionary-based word breaking, which is a distinct, heavier
problem than boundary detection and is out of scope for this MVP (real multi-character CJK
word segmentation needs a dedicated tokenizer, e.g. ES's `icu_tokenizer` / `kuromoji` for
Japanese).

The corpus above is a starting set — extend it directly from EXECUTION.md Task 9's required
categories (punctuation, CJK, mixed-case, numbers, English sentences) as more Lucene reference
output becomes available.

---

## 14. `AnalyzerRegistry`

```rust
// es-index/src/analysis/registry.rs
use std::collections::HashMap;

pub struct AnalyzerRegistry {
    analyzers: HashMap<String, Box<dyn Analyzer + Send + Sync>>,
}

impl AnalyzerRegistry {
    pub fn new() -> Self {
        let mut analyzers: HashMap<String, Box<dyn Analyzer + Send + Sync>> = HashMap::new();
        analyzers.insert("keyword".to_string(), Box::new(KeywordAnalyzer));
        analyzers.insert("standard".to_string(), Box::new(StandardAnalyzer));
        Self { analyzers }
    }

    pub fn get(&self, name: &str) -> Option<&(dyn Analyzer + Send + Sync)> {
        self.analyzers.get(name).map(|b| b.as_ref())
    }
}

impl Default for AnalyzerRegistry {
    fn default() -> Self {
        Self::new()
    }
}
```

`+ Send + Sync` on the trait object bound: `AnalyzerRegistry` is going into `AppState` behind an
`Arc` (Tutorial 06's pattern), and axum handlers run across worker threads — any type shared
across threads via `Arc` must be `Send + Sync`. Both analyzer structs are zero-sized/stateless, so
this is automatic; it's called out explicitly here because it's the first `dyn Trait` in
`AppState` and the compiler error if this bound were missing (`` `dyn Analyzer` cannot be sent
between threads safely ``) is worth recognizing on sight later.

Wiring into `AppState` (extends Tutorial 06's struct):

```rust
#[derive(Clone)]
struct AppState {
    cluster_name: String,
    store: Arc<RwLock<IndexStore>>,
    analyzers: Arc<AnalyzerRegistry>,
}
```

`AnalyzerRegistry` itself never mutates after construction — `Arc<AnalyzerRegistry>` alone (no
`RwLock`) is correct, same reasoning as any other read-only shared config.

`es-index/src/analysis/mod.rs` needs `mod registry; pub use registry::AnalyzerRegistry;` added so
`AnalyzerRegistry` is reachable as `es_index::analysis::AnalyzerRegistry`. `es-index/src/lib.rs`
needs `mod analysis; pub use analysis::{Analyzer, AnalyzerRegistry, KeywordAnalyzer,
StandardAnalyzer};` added — the same lib.rs-export step every prior tutorial that introduced a
new module has done (Tutorial 07 §`IndexStoreError`: `mod error; pub use error::IndexStoreError;`).

---

## 15. Where this feeds the inverted index (preview, not implemented yet)

Data flow for Tutorial 11:

```
document._source["title"] = "The Quick Brown Fox"
                │
                ▼
   field's configured analyzer (registry.get("standard"))
                │
                ▼
   analyze() → ["the", "quick", "brown", "fox"]
                │
                ▼
   each term inserted into the inverted index:
   term -> [(doc_id, position), ...]
```

Nothing in `es-index` calls `AnalyzerRegistry` yet outside tests — this tutorial only builds and
proves the analyzers. Tutorial 11 is where a field's mapping (`FieldType::Text`, Tutorial 03)
determines which analyzer runs at index time, and the resulting terms populate the inverted index
structure that `_search` queries against.

---

## Definition of Done

1. `unicode-segmentation` added to `[workspace.dependencies]` (`"1.13"`) and to `es-index`'s
   `[dependencies]` via `{ workspace = true }`.
2. `trait Analyzer` defined in `es-index` with `analyze(&self, text: &str) -> Vec<String>` and a
   default `name(&self) -> &str` method.
3. `KeywordAnalyzer` and `StandardAnalyzer` both `impl Analyzer`, each with a distinct `name()`.
4. `AnalyzerRegistry` (`HashMap<String, Box<dyn Analyzer + Send + Sync>>`) exists with `new()` and
   `get(&self, name: &str) -> Option<&(dyn Analyzer + Send + Sync)>`.
5. `es-index/src/lib.rs` exports `mod analysis; pub use analysis::{Analyzer, AnalyzerRegistry,
   KeywordAnalyzer, StandardAnalyzer};`; `AnalyzerRegistry` wired into `AppState` as
   `Arc<AnalyzerRegistry>` — `cargo build` succeeds with the new field threaded through
   `main.rs`'s state construction.
6. Table-driven test (`standard_analyzer_corpus`) covers at minimum: punctuation, CJK, mixed-case,
   numbers, an English sentence, empty string, whitespace-only input.
7. Every corpus case in (6) passes with byte-exact output as written in the test — no `.contains()`
   or fuzzy assertions.
8. `keyword_analyzer_is_identity_lowercased` table test passes.
9. A `// ponytail:` comment documents the two known `unicode-segmentation`-vs-Lucene divergence
   categories (URL/email special-casing, locale-sensitive numeric punctuation) — not fixed, flagged.
10. `cargo test -p es-index` and `cargo clippy --workspace --all-targets -- -D warnings` both pass.

---

**Skipped:** custom grapheme-cluster-aware tokenizer, locale-specific stemming/stop-word filters,
`simple`/`whitespace`/`icu`-family analyzers beyond `keyword`/`standard`, a real
`from_request`-style HTTP endpoint for testing analysis (`_analyze` API) — none required by
EXECUTION.md Task 9. Add `_analyze` HTTP wiring only if Tutorial 11 or a YAML test needs it.
