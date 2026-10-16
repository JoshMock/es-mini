# Tutorial 12 — Parser Combinators with nom: ES|QL Subset

Phase 2, Task 11. Builds on Tutorials 01–11 (workspace, errors, shared state, CRUD, bulk,
analysis, inverted index + JSON DSL). Tutorial 11's `Document`/`IndexMetadata` storage and its
`Range`-query scan-and-compare/numeric-compare shape are reused as-is for `WHERE` — no new
storage-layer code in this tutorial, only a new **text grammar** and a new **executor** on top
of the same `IndexMetadata`. (`WHERE` scans documents directly, the same way Tutorial 11's
`execute_range` does — it does not go through `InvertedIndex::lookup_term`/`matching_docs`,
since those are keyed on analyzed terms and `WHERE`'s comparisons are against raw field
values.)

Scope: parse a subset of ES|QL (`FROM ... | WHERE ... | LIMIT ... | KEEP/DROP ... | SORT ...`)
with `nom`, build an AST, execute it against the in-memory store, and expose it over
`POST /_query` with the columnar `{columns, values}` response shape.

---

## 1. Why a parser combinator library at all

ES|QL syntax has to become a Rust value (an AST) before anything can execute it. Three ways to
get there, ranked by how this project has approached everything else — laziest option that
actually works:

- **Regex.** Fine for `FROM logs`. Falls over immediately on `WHERE (a > 1 AND b < 2) OR c ==
  "x"` — regex has no concept of nesting, and ES|QL pipelines are inherently sequential-but-
  clause-nested. Not attempted.
- **External grammar + codegen (ANTLR, pest's `.pest` files, LALRPOP).** A separate grammar file,
  a build-time codegen step, a generated parser module checked into (or generated at) build time.
  Real ES's own ES|QL parser is ANTLR-generated. Correct choice at ES's actual grammar size;
  overkill for five clauses.
- **Hand-written recursive descent.** Just functions and `match`. Works, but every clause needs
  its own manual "consume whitespace, check next token, advance the cursor, handle the error
  case" boilerplate — the same shape repeated five times with no way to factor out "the next
  token is one of these keywords" or "one or more of X separated by commas" as reusable pieces.

**Parser combinators** are the missing middle: ordinary functions that take a parser (or several)
and return a *new* parser. No separate grammar file, no codegen step, no build-time dependency —
it's just function composition, in the same sense `functools.partial` composes callables in
Python, or Express middleware chains (`app.use(a).use(b)`) compose request handlers. Each small
parser (`tag("WHERE")`, `alpha1`, `digit1`) is trivially testable in isolation; combinators
(`tuple`, `alt`, `separated_list1`) build bigger parsers out of smaller ones without hand-rolling
the "consume and check" plumbing every time.

Net result for five ES|QL clauses: about a page of composed function calls instead of a
hand-rolled state machine, and every sub-parser gets its own unit test for free (§17).

---

## 2. The `nom` crate

`nom` is the de-facto Rust parser combinator library — the one every "how do I parse X in Rust"
search leads to, and the one already suggested by `EXECUTION.md` alongside `chumsky`.

```toml
# Cargo.toml (root, [workspace.dependencies])
nom = "8"
```

Verified: `nom = "8"` resolves to **8.0.0** (current on crates.io at authoring time).

### nom vs chumsky

| | `nom` | `chumsky` |
|---|---|---|
| Abstraction level | Lower — combinators are plain functions/closures | Higher — dedicated `Parser` DSL with more built-in machinery |
| Copying | Zero-copy by default — parses `&str`/`&[u8]` slices in place | Also supports zero-copy, but more commonly used with owned tokens |
| Error messages | Adequate, requires `nom-language`'s `VerboseError` + `convert_error` for a human-readable string (§13) | Best-in-class out of the box — designed around rich diagnostics |
| Maturity / adoption | Older, extremely widely used (parses HTTP, protocol wire formats, config files across the ecosystem) | Newer, smaller ecosystem, growing fast |

`EXECUTION.md` names either as acceptable. This tutorial uses **`nom`** — more widely used,
battle-tested on other real query languages, and the "just Rust functions calling each other"
character fits this project's existing style (no new DSL syntax to learn, unlike chumsky's
builder API). Trade-off: nom's stock error messages need the extra `nom-language` step for
something worth putting in an HTTP 400 body (§13) — `chumsky` would have saved that step. Noted,
not revisited — five clauses isn't enough grammar to justify it.

### nom 8 vs nom 7 — this matters, don't copy nom 7 examples off the internet

**nom 8 changed the core calling convention.** In nom 7 (what most blog posts and Stack Overflow
answers still show), every combinator returns a plain closure you call directly:
`tag("WHERE")(input)`. In nom 8, combinators return a value that implements the `Parser` trait,
and you call `.parse(input)` on it. `tuple()` as a free function is deprecated — tuples of
parsers implement `Parser` directly now, so `(parser_a, parser_b).parse(input)` replaces
`tuple((parser_a, parser_b))(input)`.

```rust
use nom::Parser; // required to bring `.parse()` into scope

// nom 7 style — does NOT compile against nom 8:
// let (input, _) = tag("WHERE")(input)?;

// nom 8 style:
let (input, _) = tag("WHERE").parse(input)?;
```

Confirmed by building against a real `nom = "8"` dependency: calling a combinator as a bare
function (`alt((tag("x"), tag("y")))("y")`) fails to compile in nom 8 with
`error[E0618]: expected function, found Choice<...>` — the fix is `.parse("y")`, with
`use nom::Parser` in scope. Every example below uses the nom 8 `.parse()` form throughout.

### `IResult` — the core return type

Every nom parser has this shape:

```rust
fn parser(input: &str) -> IResult<&str, Output>
```

`IResult<I, O, E = nom::error::Error<I>>` is a type alias for
`Result<(I, O), nom::Err<E>>`. Read it as: *either* `Ok((remaining_input, parsed_value))`, *or*
`Err(...)`. The "remaining input" half is the whole trick that makes composition work — each
parser consumes a prefix and hands the rest to the next one:

```rust
use nom::{IResult, Parser, bytes::complete::tag};

fn parse_hello(input: &str) -> IResult<&str, &str> {
    tag("hello").parse(input)
}

fn main() {
    assert_eq!(parse_hello("hello world"), Ok((" world", "hello")));
    assert!(parse_hello("goodbye").is_err());
}
```

`"hello world"` in → `Ok((" world", "hello"))` out: `"hello"` was consumed, `" world"` remains
for the next parser in the chain.

---

## 3. nom primitives

The leaf-level parsers — no composition, just "recognize this class of input starting here."

| Primitive | Signature | Matches |
|---|---|---|
| `tag("WHERE")` | `fn(&str) -> IResult<&str, &str>` | The literal string `"WHERE"`, case-sensitive |
| `tag_no_case("WHERE")` | same | `"WHERE"`, `"where"`, `"Where"` — anything case-insensitively equal |
| `alpha1` | `fn(&str) -> IResult<&str, &str>` | One or more ASCII alphabetic characters |
| `digit1` | `fn(&str) -> IResult<&str, &str>` | One or more ASCII digit characters |
| `alphanumeric1` | `fn(&str) -> IResult<&str, &str>` | One or more alphanumeric characters (field names) |
| `multispace0` | `fn(&str) -> IResult<&str, &str>` | Zero or more whitespace characters (spaces, tabs, newlines) |
| `multispace1` | `fn(&str) -> IResult<&str, &str>` | One or more whitespace characters |

The `0`/`1` suffix is the recurring nom naming convention: `*_0` = "zero or more" (never fails),
`*_1` = "one or more" (fails on zero matches). Same distinction as regex `*` vs `+`.

```rust
use nom::{Parser, character::complete::{alpha1, multispace1}};

assert_eq!(alpha1::<_, nom::error::Error<&str>>.parse("logs | WHERE"), Ok((" | WHERE", "logs")));
assert_eq!(multispace1::<_, nom::error::Error<&str>>.parse("   x"), Ok(("x", "   ")));
```

(The explicit `::<_, nom::error::Error<&str>>` turbofish above is only needed because these are
called standalone with no surrounding function signature to infer the error type from — every
real parser function below declares its own `IResult<&str, T>` return type, so the error type is
inferred and no turbofish is needed there.)

---

## 4. Combinator functions

Combinators take parser(s) as arguments and return a new, bigger parser.

| Combinator | Shape | Meaning |
|---|---|---|
| `(p1, p2, p3).parse(input)` (tuple `Parser` impl, replaces nom 7's `tuple(...)`) | sequence | Run each in order; fails if any fails; output is a tuple of all outputs |
| `preceded(p1, p2)` | sequence, discard first | Run `p1` then `p2`; keep only `p2`'s output (used for "skip a keyword, keep the value") |
| `delimited(open, inner, close)` | sequence, discard edges | Run `open`, then `inner`, then `close`; keep only `inner`'s output (used for `(...)`, `"..."`) |
| `separated_list1(sep, item)` | repetition | One or more `item`, separated by `sep`; fails on zero items |
| `alt((p1, p2, ...))` | choice | Try `p1`; if it fails, try `p2`; ...; fails only if all fail |
| `opt(p)` | optional | `Some(value)` if `p` succeeds, `None` if it fails — **never itself fails** |
| `many0(p)` | repetition | Zero or more repetitions of `p`, collected into a `Vec` |

```rust
use nom::{IResult, Parser, bytes::complete::tag, sequence::{preceded, delimited}};

// preceded: skip "LIMIT ", keep whatever comes after
fn skip_limit_keyword(input: &str) -> IResult<&str, &str> {
    preceded(tag("LIMIT "), nom::character::complete::digit1).parse(input)
}
assert_eq!(skip_limit_keyword("LIMIT 10"), Ok(("", "10")));

// delimited: keep only the inside of parens
fn parenthesized_word(input: &str) -> IResult<&str, &str> {
    delimited(tag("("), nom::character::complete::alpha1, tag(")")).parse(input)
}
assert_eq!(parenthesized_word("(abc)"), Ok(("", "abc")));
```

---

## 5. Case-insensitive keywords

ES|QL keywords are case-insensitive: `FROM logs | WHERE level > 3` and
`from logs | where level > 3` must both parse. Every keyword parser in this tutorial uses
`tag_no_case`, never `tag`, for the fixed keyword strings (`FROM`, `WHERE`, `LIMIT`, `KEEP`,
`DROP`, `SORT`, `ASC`, `DESC`, and the comparison operators are symbols so case doesn't apply to
them).

```rust
use nom::{IResult, Parser, bytes::complete::tag_no_case};

fn parse_where_keyword(input: &str) -> IResult<&str, &str> {
    tag_no_case("WHERE").parse(input)
}
assert_eq!(parse_where_keyword("WHERE x"), Ok((" x", "WHERE")));
assert_eq!(parse_where_keyword("where x"), Ok((" x", "where")));
```

Field names and string values themselves stay case-sensitive — only the fixed keyword vocabulary
gets the insensitive treatment. `KEEP Level` and `KEEP level` refer to different fields.

---

## 6. Building up incrementally: the AST first

Grammar and AST get designed together — the AST is *why* the grammar exists. Every type here
gets `#[derive(Debug, Clone, PartialEq)]`: `Debug` for error messages and test failure output,
`Clone` because the executor (§14) needs to hand out copies of values into response rows,
`PartialEq` because every unit test in this tutorial ends in `assert_eq!` against a parsed AST
value (Tutorial 10's table-driven pattern) — without it, `assert_eq!(parse_where(...), Ok(...))`
doesn't compile at all.

```rust
// es-query/src/esql/ast.rs

#[derive(Debug, Clone, PartialEq)]
pub struct EsqlQuery {
    pub source: SourceCommand,
    pub commands: Vec<EsqlCommand>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SourceCommand {
    pub index: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum EsqlCommand {
    Where(WhereExpr),
    Limit(u64),
    Keep(Vec<String>),
    Drop(Vec<String>),
    Sort(Vec<SortField>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct WhereExpr {
    pub field: String,
    pub op: CompareOp,
    pub value: EsqlValue,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CompareOp {
    Eq,
    Ne,
    Gt,
    Lt,
    Gte,
    Lte,
}

#[derive(Debug, Clone, PartialEq)]
pub enum EsqlValue {
    Str(String),
    Num(f64),
}

#[derive(Debug, Clone, PartialEq)]
pub struct SortField {
    pub field: String,
    pub descending: bool,
}
```

`EsqlCommand` mirrors Tutorial 11's `Query` enum decision: a closed, spec-fixed set of pipeline
stages, not a `dyn Trait` — ES|QL's command vocabulary isn't third-party-extensible any more than
the JSON DSL's query types were. Same reasoning, same shape, no new design decision needed here.

---

## 7. `FROM source` — the simplest possible query

Every ES|QL query starts with a source command. Build the smallest useful parser first, then add
clauses on top of it.

```rust
// es-query/src/esql/parser.rs

use nom::{
    IResult, Parser,
    bytes::complete::tag_no_case,
    character::complete::{alphanumeric1, multispace1},
};
use super::ast::SourceCommand;

fn parse_source(input: &str) -> IResult<&str, SourceCommand> {
    let (input, _) = tag_no_case("FROM").parse(input)?;
    let (input, _) = multispace1(input)?;
    let (input, index) = alphanumeric1(input)?;
    Ok((input, SourceCommand { index: index.to_string() }))
}
```

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_from_clause() {
        let (rest, source) = parse_source("FROM logs").unwrap();
        assert_eq!(rest, "");
        assert_eq!(source, SourceCommand { index: "logs".to_string() });
    }

    #[test]
    fn is_case_insensitive() {
        let (_, source) = parse_source("from logs").unwrap();
        assert_eq!(source.index, "logs");
    }
}
```

`index.to_string()` converts the borrowed `&str` slice into an owned `String` — the lifetime
note in §18 explains why that conversion happens right here, at the AST boundary, and nowhere
else in the parser.

---

## 8. Adding `WHERE`

`WHERE field OPERATOR value`. Operators: `==`, `!=`, `>`, `<`, `>=`, `<=`. Values: string
literals (`"..."`) or numbers.

```rust
use nom::{
    IResult, Parser,
    branch::alt,
    bytes::complete::{tag, tag_no_case},
    character::complete::{alphanumeric1, digit1, multispace0, multispace1},
    combinator::recognize,
    sequence::delimited,
};
use super::ast::{CompareOp, EsqlValue, WhereExpr};

fn parse_compare_op(input: &str) -> IResult<&str, CompareOp> {
    // longest-match-first: ">=" must be tried before ">" or ">=" would never be reachable
    // (">" would consume the ">" and leave a dangling "=" for the next parser to choke on)
    alt((
        tag("==").map(|_| CompareOp::Eq),
        tag("!=").map(|_| CompareOp::Ne),
        tag(">=").map(|_| CompareOp::Gte),
        tag("<=").map(|_| CompareOp::Lte),
        tag(">").map(|_| CompareOp::Gt),
        tag("<").map(|_| CompareOp::Lt),
    ))
    .parse(input)
}

fn parse_string_literal(input: &str) -> IResult<&str, EsqlValue> {
    delimited(tag("\""), recognize(nom::bytes::complete::is_not("\"")), tag("\""))
        .map(|s: &str| EsqlValue::Str(s.to_string()))
        .parse(input)
}

fn parse_number_literal(input: &str) -> IResult<&str, EsqlValue> {
    // digit1 only — no decimals/negatives in this MVP subset; see §7 gap note in DoD
    digit1
        .map(|s: &str| EsqlValue::Num(s.parse::<f64>().expect("digit1 output always parses")))
        .parse(input)
}

fn parse_value(input: &str) -> IResult<&str, EsqlValue> {
    alt((parse_string_literal, parse_number_literal)).parse(input)
}

pub fn parse_where(input: &str) -> IResult<&str, WhereExpr> {
    let (input, _) = tag_no_case("WHERE").parse(input)?;
    let (input, _) = multispace1(input)?;
    let (input, field) = alphanumeric1(input)?;
    let (input, _) = multispace0(input)?;
    let (input, op) = parse_compare_op(input)?;
    let (input, _) = multispace0(input)?;
    let (input, value) = parse_value(input)?;
    Ok((input, WhereExpr { field: field.to_string(), op, value }))
}
```

`.map(...)` on a `Parser` — same `Iterator::map` idea from Tutorial 10, applied to a parser
instead of an iterator: run the parser, then transform its output value, without touching the
remaining-input half of the `IResult`. `recognize(p)` discards `p`'s *parsed value* and instead
returns the *matched slice* — used here so `is_not("\"")` (which itself just recognizes a span,
without producing a meaningful typed value) yields the raw string content between the quotes.

```rust
#[cfg(test)]
mod where_tests {
    use super::*;

    #[test]
    fn parses_numeric_comparison() {
        let (rest, expr) = parse_where("WHERE age > 30").unwrap();
        assert_eq!(rest, "");
        assert_eq!(expr, WhereExpr {
            field: "age".to_string(),
            op: CompareOp::Gt,
            value: EsqlValue::Num(30.0),
        });
    }

    #[test]
    fn parses_string_equality() {
        let (_, expr) = parse_where("WHERE level == \"error\"").unwrap();
        assert_eq!(expr.op, CompareOp::Eq);
        assert_eq!(expr.value, EsqlValue::Str("error".to_string()));
    }

    #[test]
    fn rejects_incomplete_where() {
        assert!(parse_where("WHERE").is_err());
        assert!(parse_where("WHERE age").is_err()); // missing operator + value
        assert!(parse_where("WHERE age >").is_err()); // missing value
    }
}
```

---

## 9. Adding `LIMIT`

The simplest clause in the grammar — one keyword, one integer.

```rust
use nom::{IResult, Parser, bytes::complete::tag_no_case, character::complete::{digit1, multispace1}};

pub fn parse_limit(input: &str) -> IResult<&str, u64> {
    let (input, _) = tag_no_case("LIMIT").parse(input)?;
    let (input, _) = multispace1(input)?;
    let (input, n) = digit1(input)?;
    Ok((input, n.parse::<u64>().expect("digit1 output always parses as u64")))
}
```

`.expect(...)` here is the same "genuinely unreachable branch" case Tutorial 05 called out as an
acceptable `unwrap`/`expect` use: `digit1` only ever returns ASCII digit characters, which always
parse as a `u64` (barring overflow of `u64::MAX`, a `LIMIT` value nobody in the target test suite
writes — not a handler-facing untrusted-input panic risk, since the failure mode here is "digit1
guarantees only digit characters," not "arbitrary attacker-controlled bytes").

```rust
#[test]
fn parses_limit() {
    assert_eq!(parse_limit("LIMIT 10"), Ok(("", 10)));
}

#[test]
fn rejects_limit_without_number() {
    assert!(parse_limit("LIMIT").is_err());
    assert!(parse_limit("LIMIT abc").is_err());
}
```

---

## 10. Adding `KEEP` and `DROP`

Comma-separated field lists. `separated_list1` earns its keep here — this is exactly the
"one or more X separated by Y" shape it exists for.

```rust
use nom::{
    IResult, Parser,
    branch::alt,
    bytes::complete::tag_no_case,
    character::complete::{alphanumeric1, multispace0, multispace1},
    multi::separated_list1,
    sequence::preceded,
};

fn parse_field_list(input: &str) -> IResult<&str, Vec<String>> {
    let comma_sep = preceded(multispace0, nom::bytes::complete::tag(","));
    let field = preceded(multispace0, alphanumeric1);
    separated_list1(comma_sep, field)
        .map(|fields: Vec<&str>| fields.into_iter().map(str::to_string).collect())
        .parse(input)
}

pub fn parse_keep(input: &str) -> IResult<&str, Vec<String>> {
    let (input, _) = tag_no_case("KEEP").parse(input)?;
    let (input, _) = multispace1(input)?;
    parse_field_list(input)
}

pub fn parse_drop(input: &str) -> IResult<&str, Vec<String>> {
    let (input, _) = tag_no_case("DROP").parse(input)?;
    let (input, _) = multispace1(input)?;
    parse_field_list(input)
}
```

```rust
#[test]
fn parses_keep_with_multiple_fields() {
    let (_, fields) = parse_keep("KEEP name, level, age").unwrap();
    assert_eq!(fields, vec!["name".to_string(), "level".to_string(), "age".to_string()]);
}

#[test]
fn parses_drop_single_field() {
    let (_, fields) = parse_drop("DROP internal_id").unwrap();
    assert_eq!(fields, vec!["internal_id".to_string()]);
}

#[test]
fn rejects_empty_field_list() {
    assert!(parse_keep("KEEP").is_err());
}
```

`separated_list1` (the `1` suffix, same convention as `alpha1`/`digit1`) requires at least one
field — `KEEP` with nothing after it fails to parse rather than silently producing an empty
`Vec`, matching real ES|QL rejecting a bare `KEEP`.

---

## 11. Adding `SORT`

`SORT field ASC` or `SORT field DESC`, comma-separated for multiple fields, direction optional
(defaults to ascending — real ES|QL's own default).

```rust
use nom::{
    IResult, Parser,
    branch::alt,
    bytes::complete::tag_no_case,
    character::complete::{alphanumeric1, multispace0, multispace1},
    combinator::opt,
    multi::separated_list1,
    sequence::preceded,
};
use super::ast::SortField;

fn parse_sort_direction(input: &str) -> IResult<&str, bool> {
    // true = descending
    alt((
        tag_no_case("DESC").map(|_| true),
        tag_no_case("ASC").map(|_| false),
    ))
    .parse(input)
}

fn parse_sort_field(input: &str) -> IResult<&str, SortField> {
    let (input, field) = alphanumeric1(input)?;
    let (input, direction) = opt(preceded(multispace1, parse_sort_direction)).parse(input)?;
    Ok((input, SortField { field: field.to_string(), descending: direction.unwrap_or(false) }))
}

pub fn parse_sort(input: &str) -> IResult<&str, Vec<SortField>> {
    let (input, _) = tag_no_case("SORT").parse(input)?;
    let (input, _) = multispace1(input)?;
    let comma_sep = preceded(multispace0, nom::bytes::complete::tag(","));
    separated_list1(comma_sep, preceded(multispace0, parse_sort_field)).parse(input)
}
```

`opt(...)` is what makes `ASC`/`DESC` optional without a separate "has direction / doesn't have
direction" branch — it always succeeds, producing `None` rather than failing, when the direction
keyword isn't there. `direction.unwrap_or(false)` turns the absent case into the documented
default (ascending).

```rust
#[test]
fn defaults_to_ascending() {
    let (_, fields) = parse_sort("SORT timestamp").unwrap();
    assert_eq!(fields, vec![SortField { field: "timestamp".to_string(), descending: false }]);
}

#[test]
fn parses_explicit_desc() {
    let (_, fields) = parse_sort("SORT timestamp DESC").unwrap();
    assert!(fields[0].descending);
}

#[test]
fn parses_multiple_sort_fields() {
    let (_, fields) = parse_sort("SORT level DESC, timestamp ASC").unwrap();
    assert_eq!(fields.len(), 2);
    assert!(fields[0].descending);
    assert!(!fields[1].descending);
}
```

---

## 12. Assembling the full pipeline parser

`SOURCE | COMMAND | COMMAND | ...` — the source command is always first, pipe (`|`) separates
every stage after it.

```rust
use nom::{
    IResult, Parser,
    branch::alt,
    bytes::complete::tag,
    character::complete::multispace0,
    multi::many0,
    sequence::preceded,
};
use super::ast::{EsqlCommand, EsqlQuery};

fn parse_command(input: &str) -> IResult<&str, EsqlCommand> {
    alt((
        parse_where.map(EsqlCommand::Where),
        parse_limit.map(EsqlCommand::Limit),
        parse_keep.map(EsqlCommand::Keep),
        parse_drop.map(EsqlCommand::Drop),
        parse_sort.map(EsqlCommand::Sort),
    ))
    .parse(input)
}

fn parse_pipe_stage(input: &str) -> IResult<&str, EsqlCommand> {
    let (input, _) = multispace0(input)?;
    let (input, _) = tag("|")(input)?;
    let (input, _) = multispace0(input)?;
    parse_command(input)
}

pub fn parse_esql(input: &str) -> IResult<&str, EsqlQuery> {
    let (input, source) = parse_source(input)?;
    let (input, _) = multispace0(input)?;
    let (input, commands) = many0(parse_pipe_stage).parse(input)?;
    let (input, _) = multispace0(input)?;
    Ok((input, EsqlQuery { source, commands }))
}
```

`alt((...))` in `parse_command` tries each clause parser in order until one succeeds — this is
the direct payoff of every clause parser above returning a plain `IResult`: they compose into a
single "any of these five" parser with zero glue code beyond listing them. `.map(EsqlCommand::X)`
wraps each clause's own AST type into the shared `EsqlCommand` enum — reusing tuple-struct
variants as functions (`EsqlCommand::Where` is itself a valid `fn(WhereExpr) -> EsqlCommand`, no
closure needed).

```rust
#[test]
fn parses_full_pipeline() {
    let (rest, query) = parse_esql(
        "FROM logs | WHERE level > 3 | SORT timestamp DESC | LIMIT 10"
    ).unwrap();
    assert_eq!(rest, "");
    assert_eq!(query.source, SourceCommand { index: "logs".to_string() });
    assert_eq!(query.commands.len(), 3);
    assert!(matches!(query.commands[0], EsqlCommand::Where(_)));
    assert!(matches!(query.commands[1], EsqlCommand::Sort(_)));
    assert!(matches!(query.commands[2], EsqlCommand::Limit(10)));
}

#[test]
fn is_case_insensitive_end_to_end() {
    let (rest, query) = parse_esql("from logs | where level > 3 | limit 10").unwrap();
    assert_eq!(rest, "");
    assert_eq!(query.commands.len(), 2);
}

#[test]
fn source_only_query_is_valid() {
    let (rest, query) = parse_esql("FROM logs").unwrap();
    assert_eq!(rest, "");
    assert!(query.commands.is_empty());
}
```

---

## 13. Error handling: turning a nom failure into a readable message

Every parser above returns plain `IResult<&str, T>` — the default error type,
`nom::error::Error<&str>`, only records the byte offset where parsing gave up, not *why*. Good
enough for unit tests (`.is_err()`), not good enough for an HTTP 400 body a human has to read.

`nom-language`'s `VerboseError` accumulates context as the parse unwinds (which combinator was
active, what it expected), and `convert_error` renders that into a line-pointer-style message —
the same shape rustc's own compiler errors use.

```toml
# Cargo.toml (root, [workspace.dependencies])
nom-language = "0.1"
```

`VerboseError`/`convert_error` moved out of `nom` core into this separate `nom-language` crate as
of nom 8 — confirmed by trying `nom::error::VerboseError` against a real `nom = "8"` dependency:
`error[E0432]: unresolved imports nom::error::VerboseError, nom::error::convert_error`. Any nom 7
tutorial showing `use nom::error::VerboseError` is stale against nom 8.

```rust
use nom::Parser;
use nom_language::error::{convert_error, VerboseError};

pub fn parse_esql_verbose(input: &str) -> Result<EsqlQuery, String> {
    // re-run the same parse_esql logic, but with VerboseError as the error type instead of
    // the default `nom::error::Error` — same parser functions, different error accumulation
    match all_consuming(parse_esql::<VerboseError<&str>>).parse(input) {
        Ok((_, query)) => Ok(query),
        Err(nom::Err::Error(e)) | Err(nom::Err::Failure(e)) => Err(convert_error(input, e)),
        Err(nom::Err::Incomplete(_)) => Err("incomplete input".to_string()),
    }
}
```

`all_consuming(p)` (`nom::combinator::all_consuming`) fails the whole parse if there's leftover
input after `p` succeeds — without it, a typo like `FROM logs | WHERE age >>> 30` would happily
parse `FROM logs | WHERE age` and silently discard `>>> 30` as "remaining input" nobody checked.
This is exactly the "unsupported clause returns an error rather than silently mis-parsing"
requirement from `EXECUTION.md` §11 — `all_consuming` is what makes trailing garbage a hard
failure instead of a quiet truncation.

(Making the clause parsers themselves generic over the error type, `IResult<&str, T, E>` instead
of the default `IResult<&str, T>`, is the mechanical change needed to make the snippet above
compile as written — each `fn parse_x<'a, E: ParseError<&'a str>>(input: &'a str) -> IResult<&'a
str, T, E>`. Left as the one piece of real generics-plumbing this tutorial gestures at rather
than spells out line-by-line; the shape is identical across all five clause parsers, so writing
one and mechanically repeating it is the actual task, not a new concept.)

**Wrong-input example** — what a caller actually sees:

```
input:  "FROM logs | WHERE age >>> 30"
error:
0: at line 1, in Tag:
FROM logs | WHERE age >>> 30
                      ^

1: at line 1, in Alt:
FROM logs | WHERE age >>> 30
                  ^
```

Mapped to `AppError::InvalidRequest(reason)` (Tutorial 05's error enum, extended with zero new
variants — `InvalidRequest` already exists and already means "400,
`action_request_validation_exception`"):

```rust
// es-mini/src/error.rs
impl From<String> for AppError {
    // only used at the ES|QL parse-error call site — not a blanket bridge for arbitrary Strings
    fn from(parse_error: String) -> Self {
        AppError::InvalidRequest(format!("esql parse error: {parse_error}"))
    }
}
```

---

## 14. Executing ES|QL

The executor walks `EsqlQuery.commands` in order, threading a result set through each stage —
the same "pipeline" mental model as ES|QL's own `|` syntax, just running in Rust instead of
being parsed as text.

```rust
// es-query/src/esql/execute.rs

use es_index::{FieldType, IndexMetadata};
use super::ast::{CompareOp, EsqlCommand, EsqlQuery, EsqlValue, SortField};
use crate::error::QueryError;

pub struct EsqlResponse {
    pub columns: Vec<EsqlColumn>,
    pub values: Vec<Vec<serde_json::Value>>,
}

pub struct EsqlColumn {
    pub name: String,
    pub column_type: String, // "keyword" | "long" | "double" | "boolean" | "date"
}

pub fn execute_esql(query: &EsqlQuery, index: &IndexMetadata) -> Result<EsqlResponse, QueryError> {
    // Start from every document in the source index (Tutorial 11's Document type, unchanged)
    let mut rows: Vec<&es_index::Document> = index.documents.values().collect();

    for command in &query.commands {
        match command {
            EsqlCommand::Where(expr) => {
                rows.retain(|doc| eval_where(expr, doc));
            }
            EsqlCommand::Limit(n) => {
                rows.truncate(*n as usize);
            }
            EsqlCommand::Sort(fields) => {
                apply_sort(&mut rows, fields);
            }
            // Keep/Drop are handled at the column-projection step below, not here — they don't
            // change *which* documents are in the result set, only which fields get serialized.
            EsqlCommand::Keep(_) | EsqlCommand::Drop(_) => {}
        }
    }

    let keep_drop = query.commands.iter().find_map(|c| match c {
        EsqlCommand::Keep(fields) => Some(Projection::Keep(fields.clone())),
        EsqlCommand::Drop(fields) => Some(Projection::Drop(fields.clone())),
        _ => None,
    });

    Ok(project_columns(&rows, index, keep_drop))
}

fn eval_where(expr: &super::ast::WhereExpr, doc: &es_index::Document) -> bool {
    let Some(field_value) = doc.source.get(&expr.field) else { return false };
    compare(field_value, &expr.op, &expr.value)
}

fn compare(field_value: &serde_json::Value, op: &CompareOp, target: &EsqlValue) -> bool {
    // Reuses Tutorial 11's Range-query numeric-compare approach verbatim — same
    // partial_cmp/NaN-handling shape, no new comparison logic invented here.
    match (field_value, target) {
        (serde_json::Value::Number(n), EsqlValue::Num(v)) => {
            let lhs = n.as_f64().unwrap_or(f64::NAN);
            match op {
                CompareOp::Eq => lhs == *v,
                CompareOp::Ne => lhs != *v,
                CompareOp::Gt => lhs > *v,
                CompareOp::Lt => lhs < *v,
                CompareOp::Gte => lhs >= *v,
                CompareOp::Lte => lhs <= *v,
            }
        }
        (serde_json::Value::String(s), EsqlValue::Str(v)) => match op {
            CompareOp::Eq => s == v,
            CompareOp::Ne => s != v,
            _ => false, // ordering comparisons on strings are out of scope for this MVP
        },
        _ => false, // type mismatch (e.g. comparing a string field to a numeric literal): no match
    }
}

enum Projection {
    Keep(Vec<String>),
    Drop(Vec<String>),
}

/// ES|QL's own type vocabulary differs from the mapping-side `FieldType` names (Tutorial 03):
/// `integer`/`float` become `long`/`double`; everything else matches.
fn esql_type_name(field_type: FieldType) -> &'static str {
    match field_type {
        FieldType::Text => "text",
        FieldType::Keyword => "keyword",
        FieldType::Integer => "long",
        FieldType::Float => "double",
        FieldType::Boolean => "boolean",
        FieldType::Date => "date",
    }
}

/// Column set is every field name seen across the row set, in first-seen order, narrowed by
/// `KEEP`/`DROP` if present. Column types come from `IndexMetadata::mappings` (Tutorial 11),
/// not from re-inspecting each row's JSON shape — a field with no recorded mapping (e.g. an
/// unmapped array/object field, §4's dynamic-mapping gap) falls back to `"keyword"`.
fn project_columns(
    rows: &[&es_index::Document],
    index: &IndexMetadata,
    projection: Option<Projection>,
) -> EsqlResponse {
    let mut field_order: Vec<String> = Vec::new();
    for doc in rows {
        if let serde_json::Value::Object(fields) = &doc.source {
            for field_name in fields.keys() {
                if !field_order.contains(field_name) {
                    field_order.push(field_name.clone());
                }
            }
        }
    }

    let field_order = match projection {
        Some(Projection::Keep(fields)) => fields,
        Some(Projection::Drop(fields)) => {
            field_order.into_iter().filter(|f| !fields.contains(f)).collect()
        }
        None => field_order,
    };

    let columns = field_order
        .iter()
        .map(|name| EsqlColumn {
            name: name.clone(),
            column_type: index
                .mappings
                .field_type(name)
                .map(esql_type_name)
                .unwrap_or("keyword")
                .to_string(),
        })
        .collect();

    let values = rows
        .iter()
        .map(|doc| {
            field_order
                .iter()
                .map(|name| doc.source.get(name).cloned().unwrap_or(serde_json::Value::Null))
                .collect()
        })
        .collect();

    EsqlResponse { columns, values }
}
```

`Keep`/`Drop` deliberately handled once, after the row-filtering loop, not inside it — filtering
which documents survive (`Where`, `Limit`, `Sort`) and choosing which columns to serialize
(`Keep`/`Drop`) are independent concerns; folding them into the same loop would make "does `KEEP`
before `WHERE` behave differently from `KEEP` after `WHERE`" a real question when it shouldn't
be one for this MVP subset (real ES|QL does let column-shape changes affect what a later `WHERE`
can even reference — explicitly out of scope here, flagged in the DoD).

---

## 15. Columnar response format

ES|QL's `_query` response is fundamentally different from `_search`'s row-per-hit shape
(Tutorial 11): **columnar**, not row-of-objects. One `columns` array describing field name +
type, one `values` array of arrays (one inner array per matching document, in column order).

```json
{
  "columns": [
    { "name": "level", "type": "keyword" },
    { "name": "message", "type": "text" }
  ],
  "values": [
    ["error", "disk full"],
    ["error", "connection refused"]
  ]
}
```

```rust
// es-mini/src/esql.rs

use es_query::esql::execute::{EsqlColumn, EsqlResponse};
use serde::Serialize;

#[derive(Serialize)]
pub struct EsqlResponseBody {
    pub columns: Vec<EsqlColumnBody>,
    pub values: Vec<Vec<serde_json::Value>>,
}

#[derive(Serialize)]
pub struct EsqlColumnBody {
    pub name: String,
    #[serde(rename = "type")]
    pub column_type: String,
}

impl From<EsqlResponse> for EsqlResponseBody {
    fn from(r: EsqlResponse) -> Self {
        EsqlResponseBody {
            columns: r.columns.into_iter()
                .map(|c| EsqlColumnBody { name: c.name, column_type: c.column_type })
                .collect(),
            values: r.values,
        }
    }
}
```

`project_columns` (§14) maps each column's `FieldType` (Tutorial 03, via `IndexMetadata::mappings`
from Tutorial 11) to ES|QL's own type vocabulary, not `FieldType`'s own serialized name —
`integer`/`float` become `long`/`double`; `text`-typed fields report `type: "text"` in the column
list even though ES|QL itself treats them as opaque strings for filtering purposes, matching real
ES|QL's own column metadata behavior.

---

## 16. `POST /_query`

```rust
// es-mini/src/handlers/esql.rs

use axum::{extract::State, Json};
use es_query::esql::parser::parse_esql_verbose;
use es_query::esql::execute::execute_esql;
use crate::{AppState, AppError, esql::EsqlResponseBody};
use serde::Deserialize;

#[derive(Deserialize)]
pub struct EsqlRequest {
    query: String,
}

pub async fn esql_query(
    State(state): State<AppState>,
    Json(req): Json<EsqlRequest>,
) -> Result<Json<EsqlResponseBody>, AppError> {
    let ast = parse_esql_verbose(&req.query).map_err(AppError::from)?;

    let store = state.store.read().unwrap_or_else(|p| p.into_inner());
    let index_meta = store
        .get(&ast.source.index)
        .map_err(|_| AppError::IndexNotFound(ast.source.index.clone()))?;

    let response = execute_esql(&ast, index_meta).map_err(AppError::from)?;
    Ok(Json(response.into()))
}
```

Request body: `{"query": "FROM logs | WHERE level > 3 | LIMIT 10"}`. Route:
`.route("/_query", post(esql_query))`.

**Full pipeline example**, against an index with three documents (`{"level": "error", "count":
5}`, `{"level": "info", "count": 1}`, `{"level": "error", "count": 12}`):

```
POST /_query
{"query": "FROM logs | WHERE level == \"error\" | SORT count DESC | LIMIT 10 | KEEP level, count"}

200 OK
{
  "columns": [
    {"name": "level", "type": "keyword"},
    {"name": "count", "type": "long"}
  ],
  "values": [
    ["error", 12],
    ["error", 5]
  ]
}
```

---

## 17. Unsupported clauses fail loudly

`EXECUTION.md` §11 requires this explicitly: an unsupported command must produce
`AppError::InvalidRequest`, never a silent mis-parse. `all_consuming` (§13) already forces this
for garbage *within* a clause; the same discipline needs to hold for a clause keyword this
grammar has never heard of, e.g. `EVAL`:

```rust
// parse_command's `alt((...))` (§12) only knows Where/Limit/Keep/Drop/Sort — "EVAL x = y" fails
// every branch, so parse_command itself fails, so parse_pipe_stage fails, so all_consuming's
// wrapper in parse_esql_verbose returns Err — never a silently-truncated partial parse.

#[test]
fn unsupported_command_is_a_parse_error() {
    let result = parse_esql_verbose("FROM logs | EVAL x = level * 2");
    assert!(result.is_err());
}
```

No separate "known unsupported keywords" allowlist needed — `alt`'s exhaustive-failure behavior
plus `all_consuming` already gives the "fail loudly, don't guess" guarantee for free. Worth
naming explicitly in code review/tests anyway, since it's a hard requirement, not an incidental
side effect.

---

## 18. Lifetimes, briefly and practically

Every nom parser above has an implicit lifetime relationship: parsers over `&str` return `&str`
*slices of that same input*. Spelled out explicitly (elided everywhere above via lifetime
elision rules — the compiler fills this in automatically when there's exactly one input
reference):

```rust
fn parse_identifier<'a>(input: &'a str) -> IResult<&'a str, &'a str> {
    alphanumeric1(input)
}
```

Read `'a` as a compile-time-checked promise: *"the slice returned lives exactly as long as the
input it was sliced from — no longer."* This is the same idea as a Python slice or a JS
`substring()` returning a view into the original — except Rust's borrow checker verifies the
promise holds, rather than trusting the caller not to use the slice after the original string is
gone.

This is why every AST type in §6 stores `String`, not `&str`: `EsqlQuery` needs to outlive the
raw input string it was parsed from (it gets returned from `parse_esql`, stored, matched against
in tests, passed to `execute_esql` — all after the original `&str` may have gone out of scope).
Trying to store a borrowed `&'a str` in `EsqlQuery` would infect the *entire* AST with that
lifetime parameter (`EsqlQuery<'a>`, `WhereExpr<'a>`, all the way down) — every function that
touches the AST would need to thread `'a` through its own signature. The actual fix, applied
throughout this tutorial, is simpler: convert to an owned `String` at the exact moment a parser
produces its final value (`field.to_string()`, `s.to_string()`), and never again — lifetimes stop
being a concern the instant ownership is taken.

Rule of thumb carried forward to Tutorial 14 (binary Lucene format reading, which faces the exact
same "parse a slice, decide what to keep" shape at the byte level instead of the `&str` level):
borrow while parsing, own at the boundary where the result needs to outlive the buffer it came
from.

---

## 19. Definition of Done

1. `nom = "8"` and `nom-language = "0.1"` added to root `[workspace.dependencies]`; `es-query`
   depends on both via `{ workspace = true }`.
2. `EsqlQuery`, `SourceCommand`, `EsqlCommand`, `WhereExpr`, `CompareOp`, `EsqlValue`,
   `SortField` defined in `es-query/src/esql/ast.rs`, all with
   `#[derive(Debug, Clone, PartialEq)]`.
3. Sub-parsers exist and are independently unit-tested: `parse_source`, `parse_where`,
   `parse_limit`, `parse_keep`, `parse_drop`, `parse_sort` — each with at least one success case
   and one failure/rejection case (`cargo test -p es-query` green).
4. `parse_esql` composes the above into a full pipeline parser; `parse_esql_verbose` wraps it
   with `all_consuming` + `VerboseError` + `convert_error` for a human-readable error string.
5. Case-insensitivity verified by test: `FROM logs | WHERE level > 3 | LIMIT 10` and
   `from logs | where level > 3 | limit 10` both parse to equal ASTs.
6. A deliberately malformed query (`FROM logs | WHERE age >>> 30` or similar) produces a non-`OK`
   parse result with a readable `convert_error` message — asserted in a test, not just eyeballed.
7. An unsupported clause (`EVAL`, or any keyword outside the five supported commands) produces a
   parse error, never a truncated/silently-mis-parsed AST — covered by
   `unsupported_command_is_a_parse_error`-style test (§17).
8. `execute_esql` implemented in `es-query/src/esql/execute.rs`: `Where` filters, `Limit`
   truncates, `Sort` orders (reusing Tutorial 11's stable-vs-unstable sort reasoning where it
   applies), `Keep`/`Drop` project columns.
9. `EsqlResponse`/`EsqlResponseBody` produce the columnar `{"columns": [...], "values": [...]}`
   shape — verified against a realistic multi-document, multi-clause pipeline in an integration
   test.
10. `POST /_query` wired in `es-mini`, returning `AppError::IndexNotFound` for an unknown source
    index and `AppError::InvalidRequest` for any parse failure (both routed through the existing
    `IntoResponse` machinery from Tutorial 05 — no new error-envelope code).
11. Integration test (`es-mini/tests/esql.rs`) covers: a full happy-path pipeline
    (`FROM`+`WHERE`+`SORT`+`LIMIT`+`KEEP`), a parse-error case (400), and an unknown-index case
    (404).

**Known scope gaps, documented not fixed** (real ES|QL is far larger than this subset):
`EVAL`, `STATS`/aggregations, `RENAME`, `DISSECT`/`GROK`, multi-source `FROM a,b`, boolean
`AND`/`OR` combinators inside `WHERE`, negative/decimal number literals, and string ordering
comparisons are all out of scope — any of them hitting the parser produces a clean 400, per
§17, rather than a wrong answer.

---

## Notes for Tutorial 13 — Integration Testing and YAML REST Test Harness

- `POST /_query` is now the fourth "parse untrusted text/JSON, dispatch, respond" endpoint family
  (alongside index management, document CRUD, `_search`) — the YAML test harness will need an ES|QL
  `do: {esql.query: {body: {query: "..."}}}` stanza mapping, distinct from `do: {search: {...}}`.
- The five-clause grammar subset (§19's gap list) should become the first entry in whatever
  "known out-of-scope YAML tests" exclusion manifest Tutorial 13 builds — any vendored YAML test
  exercising `EVAL`/`STATS`/etc. is excluded by construction, not by a runtime skip.
- `all_consuming` + `VerboseError` (§13) is a reusable pattern for validating *any* future
  hand-written grammar in this codebase — flag it if a later tutorial reaches for regex or manual
  string splitting on something that turns out to have real nested structure.
