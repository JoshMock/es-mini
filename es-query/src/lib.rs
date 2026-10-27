//! JSON query DSL and ES|QL parsing/execution.
//!
//! Term, match and bool queries on an index opened from disk read only the on-disk documents
//! their postings name; the rest scan every document. `_count` reads no documents where postings
//! and doc values answer its query exactly (see [`count_many`]), and a search whose scores do not
//! matter reads only the on-disk hits it returns (see [`search_many`]).
//!
//! Term and match queries score documents from postings frequencies and norms, so a search
//! ordered by score reads only the on-disk hits it returns where postings and doc values
//! answer its query.

mod aggs;
mod dates;
pub mod esql;
mod hll;
mod hll_tables;
mod parse;
mod query_string;
mod regexp;
mod request;
mod tdigest;

use dates::{now_millis, resolve_dates};
pub use es_index::date_millis;
use es_index::json_num as num;
pub use parse::{Query, parse_query};
pub use query_string::parse_query_string;
pub use request::{ErrorKind, MAX_BUCKETS, QueryError, SearchRequest, SortField};

use es_index::postings::{TermFreqs, text, values};
use es_index::{Document, FieldMapping, Index, analysis::Analyzer, disk::DiskPos};
use serde_json::{Map, Value};
use std::{
    borrow::Cow,
    cell::RefCell,
    cmp::Ordering,
    collections::{BTreeSet, HashMap, HashSet},
};

/// One node of Lucene's Explanation, with its value as the float Lucene computes.
fn explanation(value: f64, description: String, details: Vec<Value>) -> Value {
    serde_json::json!({"value": value as f32, "description": description, "details": details})
}

/// Lucene wildcard syntax: `*` any run, `?` one char, `\\` escapes.
pub(crate) fn wildcard_match(p: &[char], t: &[char]) -> bool {
    match p.split_first() {
        None => t.is_empty(),
        Some(('*', rest)) => (0..=t.len()).any(|i| wildcard_match(rest, &t[i..])),
        Some(('?', rest)) => !t.is_empty() && wildcard_match(rest, &t[1..]),
        Some(('\\', [c, rest @ ..])) => t.first() == Some(c) && wildcard_match(rest, &t[1..]),
        Some((c, rest)) => t.first() == Some(c) && wildcard_match(rest, &t[1..]),
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Text,
    Keyword,
    Number,
    Bool,
}

/// The source path a field reads: a multi-field `x.y`, or unmapped `x.keyword`, reads `x`.
fn source_path<'f>(index: &Index, field: &'f str) -> &'f str {
    let props = &index.mappings.properties;
    let multi = |(base, sub): (&'f str, &str)| {
        (props.get(base)?.fields()?)
            .contains_key(sub)
            .then_some(base)
    };
    match props.contains_key(field) {
        true => field,
        false => (field.rsplit_once('.').and_then(multi))
            .or(field.strip_suffix(".keyword"))
            .unwrap_or(field),
    }
}

fn resolve(index: &Index, field: &str, sample: impl Fn(&str) -> Option<Value>) -> Kind {
    // Object paths hold no values of their own, so they read like unmapped fields.
    match (index.mappings.properties.get(field)).filter(|m| !m.is_object()) {
        Some(FieldMapping::Text { .. } | FieldMapping::TextKeyword { .. }) => Kind::Text,
        Some(FieldMapping::Keyword { .. }) => Kind::Keyword,
        Some(FieldMapping::Boolean) => Kind::Bool,
        // terms, ranges and sorts read dates as instants, and prefix and wildcard queries refuse
        // them (see `resolve_dates`, `sort_values`)
        Some(FieldMapping::Date { .. } | FieldMapping::DateNanos { .. }) => Kind::Keyword,
        // ponytail: ips match as their JSON strings; no CIDR or address normalization in queries
        Some(FieldMapping::Ip) => Kind::Keyword,
        // ponytail: queries never match range fields; add intersects/within/contains relations
        Some(FieldMapping::LongRange | FieldMapping::DateRange { .. }) => Kind::Keyword,
        Some(m) if m.is_numeric() => Kind::Number,
        Some(_) => {
            unreachable!("every FieldMapping is text, keyword, boolean, date, ip, range or numeric")
        }
        None if field.ends_with(".keyword") => Kind::Keyword,
        // ponytail: unmapped fields are typed from their first value, mimicking ES dynamic mapping
        None => match sample(field) {
            Some(Value::Number(_)) => Kind::Number,
            Some(Value::Bool(_)) => Kind::Bool,
            _ => Kind::Text,
        },
    }
}

/// Indexed terms of a field in one document (see [`es_index::postings::terms`]).
fn terms(kind: Kind, vals: &[&Value]) -> Vec<String> {
    es_index::postings::terms(kind == Kind::Text, vals)
}

/// Whether a value of a date field with `format` surely cannot be read: es-mini knows every
/// format and none reads it.
pub fn bad_date(format: Option<&str>, locale: Option<&str>, v: &Value) -> bool {
    let known =
        |f: &str| f.starts_with("epoch_") || es_index::date_pattern::DateFormat::new(f).is_some();
    format.is_some_and(|f| f.split("||").all(known)) && date_millis(format, locale, v).is_none()
}

/// Exact integer value, so longs above 2^53 keep their precision.
fn int(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_str()?.parse().ok())
}

/// Whether a value passes a range query's `[gt, gte, lt, lte]` bounds, given how it compares
/// with a bound.
fn in_range(bounds: [Option<&Value>; 4], cmp: impl Fn(&Value) -> Ordering) -> bool {
    range_place(bounds, cmp).is_eq()
}

/// Where a value is against a range query's `[gt, gte, lt, lte]` bounds, given how it compares
/// with a bound: `Less` if it fails a lower bound, else `Greater` if it fails an upper one.
fn range_place(bounds: [Option<&Value>; 4], cmp: impl Fn(&Value) -> Ordering) -> Ordering {
    let [gt, gte, lt, lte] = bounds;
    if !(gt.is_none_or(|b| cmp(b).is_gt()) && gte.is_none_or(|b| cmp(b).is_ge())) {
        Ordering::Less
    } else if !(lt.is_none_or(|b| cmp(b).is_lt()) && lte.is_none_or(|b| cmp(b).is_le())) {
        Ordering::Greater
    } else {
        Ordering::Equal
    }
}

fn scalar_cmp(a: &Value, b: &Value) -> Ordering {
    if let (Some(x), Some(y)) = (int(a), int(b))
        && (!a.is_string() || !b.is_string())
    {
        return x.cmp(&y);
    }
    match (num(a), num(b)) {
        (Some(x), Some(y)) if !a.is_string() || !b.is_string() => x.total_cmp(&y),
        _ => text(a).cmp(&text(b)),
    }
}

/// The named queries in `q`, in the order of ES's MatchedQueriesPhase: a Java HashMap's (by
/// `String.hashCode` bucket).
fn named_queries(q: &Query) -> Vec<(String, Query)> {
    fn walk(q: &Query, out: &mut Vec<(String, Query)>) {
        match q {
            Query::Named { name, query } => {
                out.push((name.clone(), (**query).clone()));
                walk(query, out);
            }
            Query::Bool {
                must,
                should,
                must_not,
                filter,
                ..
            } => {
                (must.iter().chain(should).chain(must_not).chain(filter)).for_each(|c| walk(c, out))
            }
            Query::Boosted { query, .. } => walk(query, out),
            Query::DisMax { queries, .. } => queries.iter().for_each(|c| walk(c, out)),
            _ => {}
        }
    }
    let mut all = vec![];
    walk(q, &mut all);
    // ponytail: names sharing a bucket keep query order (Java's follows Map.copyOf's), and a
    // repeated name keeps its first query (Java's its last)
    let cap = (all.len() * 4).div_ceil(3).next_power_of_two().max(16);
    let bucket = |name: &str| {
        let h = (name.encode_utf16()).fold(0u32, |h, c| h.wrapping_mul(31).wrapping_add(c.into()));
        (h ^ (h >> 16)) as usize & (cap - 1)
    };
    all.sort_by_key(|(name, _)| bucket(name));
    all.dedup_by(|a, b| a.0 == b.0);
    all
}

struct Searcher<'a> {
    index: &'a Index,
    /// Per field: (docs with the field, average length).
    field_stats: HashMap<String, (f64, f64)>,
    doc_freq: HashMap<(String, String), f64>,
    /// Per scored (field, term): its frequencies and field lengths (see [`Index::term_freqs`]).
    freqs: HashMap<(String, String), TermFreqs>,
    /// Resolved field types, so unmapped fields are sampled once per search.
    /// Per field: its kind, and whether a literal `x.keyword` source key overrides `x`.
    kinds: RefCell<HashMap<String, (Kind, bool)>>,
    /// The on-disk documents the query can match (see `candidates`).
    on_disk: Option<BTreeSet<DiskPos>>,
    /// The in-memory documents the query can match (see `memory_candidates`).
    in_memory: Option<HashSet<String>>,
    /// The query's named queries (see `named_queries`).
    named: Vec<(String, Query)>,
    /// The query, for `explain`.
    query: Query,
    /// Runtime fields that emit the document's `_source` as a string.
    runtime_source: Vec<String>,
}

const K1: f64 = 1.2;
const B: f64 = 0.75;

impl<'a> Searcher<'a> {
    fn new(index: &'a Index, q: &Query) -> Self {
        let mut s = Self::unscored(index, q);
        s.gather(q);
        s.on_disk = s.candidates(q, false);
        s.in_memory = s.memory_candidates(q);
        s
    }

    /// A searcher without BM25 statistics or candidates, for queries that only match.
    fn unscored(index: &'a Index, q: &Query) -> Self {
        Searcher {
            index,
            field_stats: HashMap::new(),
            doc_freq: HashMap::new(),
            freqs: HashMap::new(),
            kinds: RefCell::default(),
            on_disk: None,
            in_memory: None,
            named: named_queries(q),
            query: q.clone(),
            runtime_source: Vec::new(),
        }
    }

    fn kind<'f>(&self, field: &'f str) -> (Kind, &'f str) {
        let path = |literal| match literal {
            true => field,
            false => source_path(self.index, field),
        };
        if let Some(&(k, literal)) = self.kinds.borrow().get(field) {
            return (k, path(literal));
        }
        // a literal `x.keyword` source key wins over the multi-field fallback of an unmapped `x`
        let base = source_path(self.index, field);
        let literal = base != field
            && !self.index.mappings.properties.contains_key(base)
            && self
                .index
                .docs()
                .any(|(_, d)| d.source.get(field).is_some());
        let sample = |f: &str| {
            self.index
                .docs()
                .find_map(|(_, d)| values(&d.source, f).first().map(|v| (*v).clone()))
        };
        let kind = resolve(self.index, field, sample);
        self.kinds
            .borrow_mut()
            .insert(field.to_owned(), (kind, literal));
        (kind, path(literal))
    }

    /// The terms a match query on `field` looks up: a keyword mapped with
    /// `split_queries_on_whitespace` reads each word as a term.
    fn match_terms(&self, field: &str, query: &str) -> Vec<String> {
        match self.index.mappings.properties.get(field) {
            Some(FieldMapping::Keyword {
                split_queries_on_whitespace: true,
                ..
            }) => query.split_whitespace().map(str::to_owned).collect(),
            _ => query_terms(self.kind(field).0, query),
        }
    }

    /// Precomputes BM25 statistics for every scored (field, term) in the query.
    fn gather(&mut self, q: &Query) {
        let (field, wanted) = match q {
            Query::Match { field, query, .. } => (field, self.match_terms(field, query)),
            Query::MatchPhrase { .. } | Query::MatchPhrasePrefix { .. } => {
                let Some((field, terms, _)) = self.phrase(q) else {
                    return;
                };
                (field, terms.concat())
            }
            Query::Term { field, value } => (field, vec![text(value)]),
            Query::Bool { must, should, .. } => {
                must.iter().chain(should).for_each(|c| self.gather(c));
                return;
            }
            Query::DisMax { queries, .. } => return queries.iter().for_each(|c| self.gather(c)),
            Query::Named { query, .. } | Query::Boosted { query, .. } => return self.gather(query),
            _ => return,
        };
        let (kind, path) = self.kind(field);
        // they score 1.0 (see score_terms)
        if matches!(kind, Kind::Number | Kind::Bool) {
            return;
        }
        for w in &wanted {
            let key = (field.clone(), w.clone());
            if let Some(f) = self.index.term_freqs(field, path, kind == Kind::Text, w) {
                self.freqs.insert(key, f);
            }
        }
        if let Some(m) = (self.index).field_stats(field, path, kind == Kind::Text, &wanted) {
            let avg = if m.docs > 0.0 {
                m.total_terms / m.docs
            } else {
                1.0
            };
            self.field_stats.insert(field.clone(), (m.docs, avg));
            for (w, df) in m.doc_freq {
                self.doc_freq.insert((field.clone(), w), df);
            }
            return;
        }
        let (mut docs, mut len) = (0.0, 0.0);
        let mut df: HashMap<&str, f64> = HashMap::new();
        let live = self.index.docs().map(|(_, d)| match d {
            Cow::Borrowed(d) => Cow::Borrowed(&d.source),
            Cow::Owned(d) => Cow::Owned(d.source),
        });
        for source in live.chain(
            self.index
                .superseded()
                .iter()
                .map(|d| Cow::Borrowed(&d.source)),
        ) {
            let t = terms(kind, &values(&source, path));
            if t.is_empty() {
                continue;
            }
            docs += 1.0;
            len += t.len() as f64;
            for w in &wanted {
                if t.contains(w) {
                    *df.entry(w).or_default() += 1.0;
                }
            }
        }
        self.field_stats.insert(
            field.clone(),
            (docs, if docs > 0.0 { len / docs } else { 1.0 }),
        );
        for w in &wanted {
            self.doc_freq.insert(
                (field.clone(), w.clone()),
                df.get(w.as_str()).copied().unwrap_or(0.0),
            );
        }
    }

    /// The on-disk documents `q` can match, from postings; `None` if it cannot narrow them.
    /// With `exact`, for a leaf query, only those it matches, or `None` if postings and doc
    /// values cannot tell.
    fn candidates(&self, q: &Query, exact: bool) -> Option<BTreeSet<DiskPos>> {
        match q {
            Query::Term { field, value } if self.kind(field).0 == Kind::Number => {
                self.number_docs(field, [None, Some(value), None, Some(value)], exact)
            }
            Query::Term { field, value } => self.term_docs(field, &[text(value)], true, exact),
            Query::Range {
                field,
                gt,
                gte,
                lt,
                lte,
                ..
            } => self.number_docs(field, [gt, gte, lt, lte].map(Option::as_deref), exact),
            Query::Match { field, query, and } => {
                self.term_docs(field, &self.match_terms(field, query), *and, exact)
            }
            // documents with a term of each position, whatever their positions
            Query::MatchPhrase { .. } | Query::MatchPhrasePrefix { .. } if !exact => {
                let (field, terms, _) = self.phrase(q)?;
                (terms.iter())
                    .filter_map(|t| self.term_docs(field, t, false, false))
                    .reduce(|a, b| &a & &b)
            }
            Query::Named { query, .. } | Query::Boosted { query, .. } => {
                self.candidates(query, exact)
            }
            Query::DisMax { queries, .. } => (queries.iter())
                .map(|c| self.candidates(c, exact))
                .collect::<Option<Vec<_>>>()
                .map(|s| s.into_iter().flatten().collect()),
            Query::Bool {
                must,
                should,
                filter,
                min_should,
                ..
            } => {
                let required = must
                    .iter()
                    .chain(filter)
                    .filter_map(|c| self.candidates(c, exact))
                    .reduce(|a, b| &a & &b);
                let any = match min_should {
                    0 => None,
                    _ => should
                        .iter()
                        .map(|c| self.candidates(c, exact))
                        .collect::<Option<Vec<_>>>()
                        .map(|s| s.into_iter().flatten().collect()),
                };
                match (required, any) {
                    (Some(a), Some(b)) => Some(&a & &b),
                    (a, b) => a.or(b),
                }
            }
            _ => None,
        }
    }

    /// The in-memory documents `q` can match, from in-memory postings and numeric indices;
    /// `None` if it cannot narrow them. Exact for term and match queries on other fields.
    fn memory_candidates(&self, q: &Query) -> Option<HashSet<String>> {
        let lookup = |field: &str, terms: &[String], all: bool| {
            let (kind, path) = self.kind(field);
            match kind {
                Kind::Number => None,
                _ => self.index.memory_docs(path, kind == Kind::Text, terms, all),
            }
        };
        let numbers = |field: &str, bounds| {
            let (_, path) = self.kind(field);
            self.index.memory_numbers(field, path, bounds)
        };
        match q {
            // An `_id` term matches the document's id, not a source field.
            Query::Term { field, value } if field == "_id" => Some(HashSet::from([text(value)])),
            Query::Term { field, value } if self.kind(field).0 == Kind::Number => {
                numbers(field, [None, Some(value), None, Some(value)])
            }
            Query::Term { field, value } => lookup(field, &[text(value)], true),
            Query::Range {
                field,
                gt,
                gte,
                lt,
                lte,
                ..
            } => numbers(field, [gt, gte, lt, lte].map(Option::as_deref)),
            Query::Match { field, query, and } => {
                lookup(field, &self.match_terms(field, query), *and)
            }
            Query::MatchPhrase { .. } | Query::MatchPhrasePrefix { .. } => {
                let (field, terms, _) = self.phrase(q)?;
                (terms.iter())
                    .filter_map(|t| lookup(field, t, false))
                    .reduce(|a, b| &a & &b)
            }
            Query::Named { query, .. } | Query::Boosted { query, .. } => {
                self.memory_candidates(query)
            }
            Query::DisMax { queries, .. } => (queries.iter())
                .map(|c| self.memory_candidates(c))
                .collect::<Option<Vec<_>>>()
                .map(|s| s.into_iter().flatten().collect()),
            Query::Bool {
                must,
                should,
                filter,
                min_should,
                ..
            } => {
                let required = must
                    .iter()
                    .chain(filter)
                    .filter_map(|c| self.memory_candidates(c))
                    .reduce(|a, b| &a & &b);
                let any = match min_should {
                    0 => None,
                    _ => should
                        .iter()
                        .map(|c| self.memory_candidates(c))
                        .collect::<Option<Vec<_>>>()
                        .map(|s| s.into_iter().flatten().collect()),
                };
                match (required, any) {
                    (Some(a), Some(b)) => Some(&a & &b),
                    (a, b) => a.or(b),
                }
            }
            _ => None,
        }
    }

    /// The on-disk documents indexing all (or any) of `terms` in `field` (see
    /// [`Index::disk_term`]).
    fn term_docs(
        &self,
        field: &str,
        terms: &[String],
        all: bool,
        exact: bool,
    ) -> Option<BTreeSet<DiskPos>> {
        let (kind, _) = self.kind(field);
        let mut sets = terms.iter().map(|t| {
            // Booleans index as `T` and `F`.
            let t = match (kind, t.as_str()) {
                (Kind::Bool, "true") => "T",
                (Kind::Bool, "false") => "F",
                (_, t) => t,
            };
            self.index.disk_term(field, t, exact)
        });
        let first = sets.next().unwrap_or(Some(BTreeSet::new()))?;
        sets.try_fold(first, |a, b| Some(if all { &a & &b? } else { &a | &b? }))
    }

    /// The on-disk documents whose values of numeric or date `field` are within `bounds`
    /// (see [`range_place`] and [`Index::disk_numbers`]).
    fn number_docs(
        &self,
        field: &str,
        bounds: [Option<&Value>; 4],
        exact: bool,
    ) -> Option<BTreeSet<DiskPos>> {
        if let Some(FieldMapping::Float) = self.index.mappings.field(field) {
            // ponytail: a superset, so exact answers read sources; exact once scoring rounds too
            return if exact {
                None
            } else {
                self.float_docs(field, bounds)
            };
        }
        let instant = self.instant(field);
        self.index.disk_numbers(
            field,
            |v| {
                range_place(bounds, |b| match &instant {
                    None => scalar_cmp(v, b),
                    // as scoring does, a bound that is not a date is above every value
                    Some(instant) => {
                        let bound = b.as_i64().or_else(|| instant(b));
                        (v.as_i64().zip(bound)).map_or(Ordering::Less, |(x, y)| x.cmp(&y))
                    }
                })
            },
            exact,
        )
    }

    /// The on-disk documents a range of float `field` may match, by its f32 doc values: a
    /// source value within `bounds` rounds to a float within them rounded, strict ones included.
    fn float_docs(&self, field: &str, bounds: [Option<&Value>; 4]) -> Option<BTreeSet<DiskPos>> {
        let round = |b: &Value| num(b).map_or(b.clone(), |x| Value::from(f64::from(x as f32)));
        let [gt, gte, lt, lte] = bounds.map(|b| b.map(round));
        let place = |v: &Value| {
            let cmp = |b: &Value| scalar_cmp(v, b);
            match range_place([None, gt.as_ref(), None, lt.as_ref()], cmp) {
                Ordering::Equal => range_place([None, gte.as_ref(), None, lte.as_ref()], cmp),
                place => place,
            }
        };
        self.index.disk_numbers(field, place, false)
    }

    /// For a date field, how it reads a value as epoch millis.
    fn instant(&self, field: &str) -> Option<impl Fn(&Value) -> Option<i64> + 'a> {
        match self.index.mappings.field(field)? {
            FieldMapping::Date { format, locale, .. }
            | FieldMapping::DateNanos { format, locale } => {
                Some(move |v: &Value| date_millis(format.as_deref(), locale.as_deref(), v))
            }
            _ => None,
        }
    }

    /// The documents the query may match: all in memory, and on disk those its postings allow.
    fn docs(&self) -> impl Iterator<Item = (&'a str, Cow<'a, Document>)> + '_ {
        self.index
            .docs_at(self.in_memory.as_ref(), self.on_disk.as_ref())
    }

    /// BM25's inputs and parts for a term of a field `len` terms long:
    /// `[df, docs with the field, norm length, average length, idf, tf]`.
    fn bm25_parts(&self, field: &str, term: &str, freq: f64, len: f64) -> [f64; 6] {
        let (n, avg) = self.field_stats.get(field).copied().unwrap_or((1.0, 1.0));
        let df = self
            .doc_freq
            .get(&(field.to_owned(), term.to_owned()))
            .copied()
            .unwrap_or(0.0);
        let len = f64::from(es_index::disk::norm_len(es_index::disk::norm_byte(
            len as u32,
        )));
        let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
        let tf = freq / (freq + K1 * (1.0 - B + B * len / avg));
        [df, n, len, avg, idf, tf]
    }

    fn bm25(&self, field: &str, term: &str, freq: f64, len: f64) -> f64 {
        let [.., idf, tf] = self.bm25_parts(field, term, freq, len);
        // ES's LegacyBM25Similarity scales Lucene's BM25 by (1 + k1).
        idf * (1.0 + K1) * tf
    }

    /// A matching document's score as Lucene's Explanation, like ES's `explain`.
    fn explain(&self, id: &str, d: &Document) -> Value {
        self.explain_query(&self.query, id, d)
    }

    fn explain_query(&self, q: &Query, id: &str, d: &Document) -> Value {
        let score = self.score(q, id, d).unwrap_or(0.0);
        let mut details = match q {
            Query::Match { field, query, .. } => {
                self.explain_terms(field, query_terms, query, id, d)
            }
            Query::Term { field, value } if field != "_id" => {
                self.explain_terms(field, |_, v| vec![v.to_owned()], &text(value), id, d)
            }
            Query::Bool { must, should, .. } => {
                // ponytail: filter/must_not clauses are left out of the explanation
                let details = (must.iter().chain(should))
                    .filter(|c| self.score(c, id, d).is_some())
                    .map(|c| self.explain_query(c, id, d));
                return explanation(score, "sum of:".into(), details.collect());
            }
            Query::Named { query, .. } => return self.explain_query(query, id, d),
            Query::Boosted { query, .. } => {
                let inner = self.explain_query(query, id, d);
                return explanation(score, "boost".into(), vec![inner]);
            }
            Query::MatchAll => return explanation(score, "*:*".into(), vec![]),
            _ => vec![],
        };
        match details.len() {
            1 => details.remove(0),
            // ponytail: constant-score queries describe themselves generically, not as
            // Lucene's query strings
            0 => explanation(score, "ConstantScore".into(), vec![]),
            _ => explanation(score, "sum of:".into(), details),
        }
    }

    /// Each matched term's BM25 weight, like Lucene's TermWeight and BM25Similarity explain.
    fn explain_terms(
        &self,
        field: &str,
        analyze: fn(Kind, &str) -> Vec<String>,
        query: &str,
        id: &str,
        d: &Document,
    ) -> Vec<Value> {
        let (kind, path) = self.kind(field);
        if matches!(kind, Kind::Number | Kind::Bool) {
            return vec![];
        }
        let doc_terms = terms(kind, &values(&d.source, path));
        let mut out = vec![];
        for w in analyze(kind, query) {
            let (freq, len) = match self.posting(field, &w, id) {
                Some(at) => at.map_or((0.0, 0.0), |(f, l)| (f.into(), l.into())),
                None => {
                    let freq = doc_terms.iter().filter(|t| **t == w).count();
                    (freq as f64, doc_terms.len() as f64)
                }
            };
            if freq == 0.0 {
                continue;
            }
            let [df, n, len, avg, idf, tf] = self.bm25_parts(field, &w, freq, len);
            let leaf = |v: f64, desc: &str| explanation(v, desc.into(), vec![]);
            let idf = explanation(
                idf,
                "idf, computed as log(1 + (N - n + 0.5) / (n + 0.5)) from:".into(),
                vec![
                    leaf(df, "n, number of documents containing term"),
                    leaf(n, "N, total number of documents with field"),
                ],
            );
            let tf = explanation(
                tf,
                "tf, computed as freq / (freq + k1 * (1 - b + b * dl / avgdl)) from:".into(),
                vec![
                    leaf(freq, "freq, occurrences of term within document"),
                    leaf(K1, "k1, term saturation parameter"),
                    leaf(B, "b, length normalization parameter"),
                    leaf(len, "dl, length of field"),
                    leaf(avg, "avgdl, average length of field"),
                ],
            );
            let weight = self.bm25(field, &w, freq, len);
            let score = explanation(
                weight,
                format!("score(freq={freq:?}), computed as boost * idf * tf from:"),
                vec![leaf(1.0 + K1, "boost"), idf, tf],
            );
            // ponytail: Lucene's doc id is segment-local; the sequence number stands in
            let desc = format!(
                "weight({field}:{w} in {}) [PerFieldSimilarity], result of:",
                d.seq_no
            );
            out.push(explanation(weight, desc, vec![score]));
        }
        out
    }

    /// `None` if the document does not match; otherwise its score.
    fn score(&self, q: &Query, id: &str, d: &Document) -> Option<f64> {
        match q {
            Query::MatchAll => Some(1.0),
            Query::Match { field, query, and } => {
                let wanted = self.match_terms(field, query);
                self.score_terms(field, &wanted, *and, id, d)
            }
            Query::Term { field, value } if field == "_id" => (text(value) == id).then_some(1.0),
            Query::Term { field, value } => self.score_terms(field, &[text(value)], true, id, d),
            Query::MatchPhrase { .. } | Query::MatchPhrasePrefix { .. } => {
                let (field, terms, slop) = self.phrase(q)?;
                let (kind, path) = self.kind(field);
                let wanted = terms.concat();
                // Lucene rewrites a one-position phrase to term queries, any of which match
                if terms.len() < 2 || kind != Kind::Text {
                    return self.score_terms(field, &wanted, false, id, d);
                }
                let texts: Vec<String> = values(&d.source, path).into_iter().map(text).collect();
                let tokens = es_index::postings::positions(&texts);
                let len = tokens.len();
                // a position's terms' positions merged, as MultiPhraseQuery's UnionPostingsEnum
                let lists: Vec<Vec<u32>> = (terms.iter())
                    .map(|t| {
                        let at = tokens.iter().filter(|(tok, _)| t.contains(tok));
                        at.map(|&(_, p)| p).collect()
                    })
                    .collect();
                let freq = match slop {
                    _ if lists.iter().any(Vec::is_empty) => return None,
                    0 => exact_phrase_freq(&lists),
                    _ => sloppy_phrase_freq(&lists, slop),
                };
                if freq == 0.0 {
                    return None;
                }
                // PhraseWeight: one BM25 score with the idfs of the terms in the index summed
                let idf: f64 = (wanted.iter())
                    .map(|w| self.bm25_parts(field, w, freq, len as f64))
                    .filter(|p| p[0] > 0.0)
                    .map(|p| p[4])
                    .sum();
                let [.., tf] = self.bm25_parts(field, &wanted[0], freq, len as f64);
                Some(idf * (1.0 + K1) * tf)
            }
            Query::Prefix { field, prefix } => {
                let (kind, path) = self.kind(field);
                terms(kind, &values(&d.source, path))
                    .iter()
                    .any(|t| t.starts_with(prefix.as_str()))
                    .then_some(1.0)
            }
            Query::Wildcard {
                field,
                pattern,
                case_insensitive,
            } => {
                let (kind, path) = self.kind(field);
                let fold = |s: &str| match case_insensitive {
                    true => s.to_lowercase().chars().collect::<Vec<_>>(),
                    false => s.chars().collect(),
                };
                let pat = fold(pattern);
                terms(kind, &values(&d.source, path))
                    .iter()
                    .any(|t| wildcard_match(&pat, &fold(t)))
                    .then_some(1.0)
            }
            Query::Exists { field } => {
                let (_, path) = self.kind(field);
                (!values(&d.source, path).is_empty()).then_some(1.0)
            }
            Query::Range {
                field,
                gt,
                gte,
                lt,
                lte,
                ..
            } => {
                let (kind, path) = self.kind(field);
                let bounds = [gt, gte, lt, lte].map(Option::as_deref);
                if kind == Kind::Text {
                    // a text field's indexed terms, compared as Lucene's TermRangeQuery does
                    return (terms(kind, &values(&d.source, path)).iter())
                        .any(|t| in_range(bounds, |b| t.as_str().cmp(&text(b))))
                        .then_some(1.0);
                }
                let instant = self.instant(field);
                let cmp = |a: &Value, b: &Value| match &instant {
                    None => scalar_cmp(a, b),
                    // date bounds are epoch millis once resolved (see `resolve_dates`)
                    Some(instant) => match (instant(a), b.as_i64().or_else(|| instant(b))) {
                        (Some(x), Some(y)) => x.cmp(&y),
                        _ => Ordering::Less,
                    },
                };
                (values(&d.source, path).into_iter())
                    .any(|v| in_range(bounds, |b| cmp(v, b)))
                    .then_some(1.0)
            }
            Query::Bool {
                must,
                should,
                must_not,
                filter,
                min_should,
            } => {
                if must_not.iter().any(|c| self.score(c, id, d).is_some())
                    || filter.iter().any(|c| self.score(c, id, d).is_none())
                {
                    return None;
                }
                let mut total = 0.0;
                for c in must {
                    total += self.score(c, id, d)?;
                }
                let hits: Vec<f64> = should.iter().filter_map(|c| self.score(c, id, d)).collect();
                (hits.len() >= *min_should).then(|| total + hits.iter().sum::<f64>())
            }
            Query::DisMax {
                queries,
                tie_breaker,
            } => {
                let hits = queries.iter().filter_map(|c| self.score(c, id, d));
                dis_max(hits, *tie_breaker)
            }
            Query::Named { query, .. } => self.score(query, id, d),
            Query::Boosted { boost, query } => self.score(query, id, d).map(|s| s * boost),
            // `resolve_dates` unwraps it before a search
            Query::Normalized(query) => self.score(query, id, d),
        }
    }

    /// The `_name`s of the query's named queries a matching document matches.
    fn matched_queries(&self, id: &str, d: &Document) -> Vec<String> {
        (self.named.iter())
            .filter(|(_, q)| self.score(q, id, d).is_some())
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// The terms of `q`'s clauses on `field` (but not its `must_not` ones) that a document
    /// highlights.
    // ponytail: only match, term and phrase terms; prefix, wildcard and range queries highlight nothing
    fn highlight_terms(&self, q: &Query, field: &str, out: &mut Vec<String>) {
        match q {
            Query::Match {
                field: f, query, ..
            }
            | Query::MatchPhrase {
                field: f, query, ..
            } if f == field => out.extend(self.match_terms(field, query)),
            Query::Term { field: f, value } if f == field => out.push(text(value)),
            Query::Bool {
                must,
                should,
                filter,
                ..
            } => (must.iter().chain(should).chain(filter))
                .for_each(|c| self.highlight_terms(c, field, out)),
            Query::DisMax { queries, .. } => queries
                .iter()
                .for_each(|c| self.highlight_terms(c, field, out)),
            Query::Named { query, .. } | Query::Boosted { query, .. } => {
                self.highlight_terms(query, field, out)
            }
            _ => {}
        }
    }

    /// The values of `field` in a document with the terms of `q` on it wrapped in `<em>`, like
    /// ES's plain highlighter; none if no value has a term.
    // ponytail: whole values, not 100-character fragments; no custom tags or encoders
    fn highlight(&self, q: &Query, field: &str, d: &Document) -> Vec<String> {
        let mut wanted = vec![];
        self.highlight_terms(q, field, &mut wanted);
        let (kind, path) = self.kind(field);
        let mut out = vec![];
        for value in values(&d.source, path)
            .into_iter()
            .filter_map(Value::as_str)
        {
            // each token's byte range, wrapped if it is a wanted term
            let tokens = match kind {
                Kind::Text => es_index::analysis::standard_tokenize(value)
                    .into_iter()
                    .map(|t| {
                        let start = t.as_ptr() as usize - value.as_ptr() as usize;
                        (
                            start..start + t.len(),
                            Analyzer::Standard.analyze(t).concat(),
                        )
                    })
                    .collect(),
                _ => vec![(0..value.len(), value.to_owned())],
            };
            let mut marked = String::new();
            let mut done = 0;
            for (range, _) in tokens.into_iter().filter(|(_, t)| wanted.contains(t)) {
                marked += &format!(
                    "{}<em>{}</em>",
                    &value[done..range.start],
                    &value[range.clone()]
                );
                done = range.end;
            }
            if done > 0 {
                out.push(marked + &value[done..]);
            }
        }
        out
    }

    /// A document's frequency of `term` in `field` and the field's length, from postings
    /// (`Some(None)` if it does not hold the term); `None` without postings (see `gather`).
    fn posting(&self, field: &str, term: &str, id: &str) -> Option<Option<(u32, u32)>> {
        let f = self.freqs.get(&(field.to_owned(), term.to_owned()))?;
        let on_disk = || f.disk.get(&self.index.disk_pos(id)?);
        Some(f.memory.get(id).or_else(on_disk).copied())
    }

    /// A document's score for `wanted` terms of `field` from their postings (`Some(None)` if
    /// it does not match); `None` without postings for one of them (see `gather`).
    fn postings_terms(
        &self,
        field: &str,
        wanted: &[String],
        all: bool,
        id: &str,
    ) -> Option<Option<f64>> {
        let mut score = 0.0;
        let mut n = 0;
        for w in wanted {
            if let Some((freq, len)) = self.posting(field, w, id)? {
                n += 1;
                score += self.bm25(field, w, f64::from(freq), f64::from(len));
            }
        }
        Some((n > 0 && (!all || n == wanted.len())).then_some(score))
    }

    /// A document's score from postings alone (`Some(None)` if it does not match), given
    /// whether it is known to match `q`; `None` if postings cannot tell.
    fn postings_score(&self, q: &Query, id: &str, matched: bool) -> Option<Option<f64>> {
        let terms = |field: &str, wanted: &[String], all| match self.kind(field).0 {
            Kind::Number | Kind::Bool => matched.then_some(Some(1.0)),
            _ => self.postings_terms(field, wanted, all, id),
        };
        match q {
            Query::MatchAll => Some(Some(1.0)),
            Query::Term { field, value } if field == "_id" => {
                Some((text(value) == id).then_some(1.0))
            }
            Query::Term { field, value } => terms(field, &[text(value)], true),
            Query::Match { field, query, and } => {
                terms(field, &self.match_terms(field, query), *and)
            }
            // ponytail: postings positions are not read; scores come from the source
            Query::MatchPhrase { .. } | Query::MatchPhrasePrefix { .. } => None,
            Query::DisMax {
                queries,
                tie_breaker,
            } => {
                let mut scores = vec![];
                for c in queries {
                    scores.extend(self.postings_score(c, id, false)?);
                }
                Some(dis_max(scores.into_iter(), *tie_breaker))
            }
            Query::Named { query, .. } => self.postings_score(query, id, matched),
            Query::Boosted { boost, query } => self
                .postings_score(query, id, matched)
                .map(|s| s.map(|s| s * boost)),
            // Unless the document is known to match, its filters would need checking.
            Query::Bool {
                must_not, filter, ..
            } if !matched && !(must_not.is_empty() && filter.is_empty()) => None,
            Query::Bool {
                must,
                should,
                min_should,
                ..
            } => {
                let mut total = 0.0;
                for c in must {
                    match self.postings_score(c, id, matched)? {
                        Some(s) => total += s,
                        None => return Some(None),
                    }
                }
                let mut n = 0;
                for c in should {
                    if let Some(s) = self.postings_score(c, id, false)? {
                        total += s;
                        n += 1;
                    }
                }
                Some((n >= *min_should).then_some(total))
            }
            _ if matched => Some(Some(1.0)),
            _ => None,
        }
    }

    /// A phrase query's field, its terms at each position (those a prefix expands to, last),
    /// and its slop.
    fn phrase<'q>(&self, q: &'q Query) -> Option<(&'q String, Vec<Vec<String>>, u32)> {
        let (field, query, slop, expansions) = match q {
            Query::MatchPhrase { field, query, slop } => (field, query, slop, None),
            Query::MatchPhrasePrefix {
                field,
                query,
                slop,
                expansions,
                ..
            } => (field, query, slop, Some(expansions)),
            _ => return None,
        };
        let mut terms: Vec<Vec<String>> = (query_terms(self.kind(field).0, query).into_iter())
            .map(|t| vec![t])
            .collect();
        if let (Some(last), Some(e)) = (terms.last_mut(), expansions) {
            last.clone_from(e);
        }
        Some((field, terms, *slop))
    }

    fn score_terms(
        &self,
        field: &str,
        wanted: &[String],
        all: bool,
        id: &str,
        d: &Document,
    ) -> Option<f64> {
        let (kind, path) = self.kind(field);
        let vals = values(&d.source, path);
        let matched = |w: &String| match kind {
            Kind::Number => {
                let w = Value::String(w.clone());
                vals.iter().any(|v| match (int(v), int(&w)) {
                    (Some(a), Some(b)) => a == b,
                    _ => num(v).is_some_and(|a| num(&w) == Some(a)),
                })
            }
            Kind::Bool => vals.iter().any(|v| text(v) == *w),
            _ => false,
        };
        if matches!(kind, Kind::Number | Kind::Bool) {
            let n = wanted.iter().filter(|w| matched(w)).count();
            return (n > 0 && (!all || n == wanted.len())).then_some(1.0);
        }
        if let Some(score) = self.postings_terms(field, wanted, all, id) {
            return score;
        }
        let doc_terms = terms(kind, &vals);
        let mut score = 0.0;
        let mut n = 0;
        for w in wanted {
            let freq = doc_terms.iter().filter(|t| *t == w).count() as f64;
            if freq > 0.0 {
                n += 1;
                score += self.bm25(field, w, freq, doc_terms.len() as f64);
            }
        }
        (n > 0 && (!all || n == wanted.len())).then_some(score)
    }
}

#[derive(Debug)]
pub struct Hit<'a> {
    /// The concrete index the hit came from.
    pub index: &'a str,
    pub id: &'a str,
    pub doc: Cow<'a, Document>,
    /// `None` when sorting by a field (ES reports `_score: null`).
    pub score: Option<f32>,
    pub sort: Vec<Value>,
    /// The `_name`s of the named queries the hit matches.
    pub matched_queries: Vec<String>,
    /// The requested `fields` that have values.
    pub fields: Map<String, Value>,
}

#[derive(Debug)]
pub struct SearchResult<'a> {
    pub total: usize,
    pub max_score: Option<f32>,
    pub hits: Vec<Hit<'a>>,
    /// The rendered `aggregations`, if the request had any.
    pub aggregations: Option<Value>,
    /// With `profile: true`, each searched target's `aggregations` profile (an array), in
    /// target order.
    pub profile: Option<Vec<Value>>,
    /// The targets whose shard failed, with why; their documents are left out.
    pub failures: Vec<(&'a str, QueryError)>,
}

/// One index to search, with the filter of the alias it was reached through.
pub type Target<'a> = (&'a str, &'a Index, Option<&'a Value>);

/// The request query, narrowed by an alias filter.
fn with_filter(
    index: &Index,
    q: &Query,
    filter: Option<&Value>,
    now: i64,
) -> Result<Query, QueryError> {
    let q = match filter {
        None => q.clone(),
        Some(f) => Query::Bool {
            must: vec![q.clone()],
            should: vec![],
            must_not: vec![],
            filter: vec![parse_query(f)?],
            min_should: 0,
        },
    };
    resolve_dates(index, q, now)
}

/// ES refuses to sort on an unmapped field (without `unmapped_type`) or a `text` field
/// (fielddata is off by default).
pub fn check_sortable(targets: &[Target], req: &SearchRequest) -> Result<(), QueryError> {
    for f in &req.sort {
        for (_, index, _) in targets {
            let known = f.unmapped_ok
                || f.field.starts_with('_')
                || index.mappings.properties.contains_key(&f.field)
                || f.field.strip_suffix(".keyword").is_some_and(|b| {
                    matches!(
                        index.mappings.properties.get(b),
                        Some(FieldMapping::TextKeyword { .. })
                    )
                });
            if !known {
                return Err(QueryError::illegal(format!(
                    "No mapping found for [{}] in order to sort on",
                    f.field
                )));
            }
            if let Some(FieldMapping::Text { .. } | FieldMapping::TextKeyword { .. }) =
                index.mappings.properties.get(&f.field)
            {
                return Err(fielddata_disabled(&f.field));
            }
        }
    }
    Ok(())
}

/// ES refuses to sort or aggregate on a `text` field: fielddata is off by default.
fn fielddata_disabled(field: &str) -> QueryError {
    QueryError::illegal(format!(
        "Text fields are not optimised for operations that require per-document field data like aggregations and sorting, so these operations are disabled by default. Please use a keyword field instead. Alternatively, set fielddata=true on [{field}] in order to load field data by uninverting the inverted index. Note that this can use significant memory."
    ))
}

impl<'a> Searcher<'a> {
    /// A matching document's values for `sort` (`score` its query score).
    fn sort_values(&self, sort: &[SortField], score: f64, doc: &Document) -> Vec<Value> {
        sort.iter()
            .map(|f| match f.field.as_str() {
                "_score" => Value::from(score as f32),
                field => {
                    let vals = values(&doc.source, self.kind(field).1);
                    // dates sort by epoch millis, date_nanos by epoch nanos
                    let instant: fn(Option<&str>, Option<&str>, &Value) -> Option<i64>;
                    let (format, locale) = match self.index.mappings.field(field) {
                        Some(FieldMapping::Date { format, locale, .. }) => {
                            instant = date_millis;
                            (format, locale)
                        }
                        Some(FieldMapping::DateNanos { format, locale }) => {
                            instant = |f, l, v| es_index::disk::date_nanos(f, l, v).flatten();
                            (format, locale)
                        }
                        _ => return sort_value(f, vals),
                    };
                    let instants: Vec<Value> = (vals.into_iter())
                        .filter_map(|v| instant(format.as_deref(), locale.as_deref(), v))
                        .map(Value::from)
                        .collect();
                    sort_value(f, instants.iter().collect())
                }
            })
            .collect()
    }

    /// The on-disk documents a leaf query matches, from postings and doc values; `None` if
    /// they cannot tell (see [`exact`]).
    fn disk_leaf(&self, q: &Query) -> Option<HashSet<DiskPos>> {
        match q {
            Query::Term { .. } | Query::Match { .. } | Query::Range { .. } => {
                Some(self.candidates(q, true)?.into_iter().collect())
            }
            _ => None,
        }
    }

    /// The searchable on-disk documents `q` matches, as hits of `target` ordered by `sort`
    /// values (and scores, if hits are ordered `by_score`), from postings and doc values alone;
    /// `None` if they cannot tell.
    fn select_on_disk(
        &self,
        target: usize,
        q: &Query,
        sort: &[SortField],
        by_score: bool,
    ) -> Option<Vec<Found<'a>>> {
        use FieldMapping as M;
        let constant = if by_score {
            constant_score(q)
        } else {
            Some(0.0)
        };
        let mut columns = (sort.iter())
            .map(|f| match self.index.mappings.properties.get(&f.field) {
                _ if f.field == "_score" => Some(None),
                // types whose doc values order like their sources
                Some(
                    M::Long
                    | M::Integer
                    | M::Short
                    | M::Byte
                    | M::Double
                    | M::Boolean
                    | M::Keyword {
                        ignore_above: None,
                        split_queries_on_whitespace: false,
                    },
                ) => Some(Some(self.index.disk_values(&f.field))),
                _ => None,
            })
            .collect::<Option<Vec<_>>>()?;
        let live: HashMap<_, _> = self.index.disk_live().collect();
        let matched = exact(q, &live.keys().copied().collect(), &|c| self.disk_leaf(c))?;
        let mut out = vec![];
        for (id, seq_no) in matched.iter().map(|p| live[p]) {
            if !self.index.searchable(seq_no) {
                continue;
            }
            let score = match constant {
                Some(score) => score,
                None => self.postings_score(q, id, true)??,
            };
            let sort = (sort.iter().zip(&mut columns))
                .map(|(f, column)| match column {
                    None => Some(Value::from(score as f32)),
                    Some(read) => Some(sort_value(f, read(id)?.iter().collect())),
                })
                .collect::<Option<_>>()?;
            out.push(Found {
                target,
                id,
                seq_no,
                doc: None,
                score,
                sort,
            });
        }
        Some(out)
    }
}

/// A document's value for sort field `f` among its `values`, or the `missing` one.
fn sort_value(f: &SortField, mut values: Vec<&Value>) -> Value {
    values.sort_by(|a, b| scalar_cmp(a, b));
    let pick = if f.desc {
        values.last()
    } else {
        values.first()
    };
    pick.map_or_else(
        || match &f.missing {
            Some(m) if !matches!(m.as_str(), Some("_first" | "_last")) => m.clone(),
            _ => Value::Null,
        },
        |v| (*v).clone(),
    )
}

/// The score of every document `q` matches, if it does not vary (see [`Searcher::score`]).
fn constant_score(q: &Query) -> Option<f64> {
    match q {
        Query::Match { .. } | Query::MatchPhrase { .. } | Query::MatchPhrasePrefix { .. } => None,
        Query::Term { field, .. } if field != "_id" => None,
        Query::Named { query, .. } => constant_score(query),
        Query::Boosted { boost, query } => constant_score(query).map(|s| s * boost),
        Query::Bool { must, should, .. } if should.is_empty() => {
            must.iter().map(constant_score).sum()
        }
        Query::Bool { .. } | Query::DisMax { .. } => None,
        _ => Some(1.0),
    }
}

/// A matching document, before hits are paged and read.
struct Found<'a> {
    /// The position of its index among the searched targets.
    target: usize,
    id: &'a str,
    seq_no: u64,
    /// `None` until read, for one selected without reading it.
    doc: Option<Cow<'a, Document>>,
    score: f64,
    /// Its values to order hits by, as [`Searcher::sort_values`] gives them.
    sort: Vec<Value>,
}

/// Orders two hits, given as their `sort_values` and score, by `sort` (by score if empty).
fn cmp_sorted(sort: &[SortField], (a, sa): (&[Value], f64), (b, sb): (&[Value], f64)) -> Ordering {
    if sort.is_empty() {
        return sb.total_cmp(&sa);
    }
    sort.iter()
        .zip(a.iter().zip(b))
        .fold(Ordering::Equal, |acc, (f, (x, y))| {
            let first = f.missing.as_ref().and_then(Value::as_str) == Some("_first");
            acc.then_with(|| match (x.is_null(), y.is_null()) {
                // Missing values sort last in both directions unless `missing: _first`.
                (true, false) if first => Ordering::Less,
                (false, true) if first => Ordering::Greater,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                _ if f.desc => scalar_cmp(y, x),
                _ => scalar_cmp(x, y),
            })
        })
}

/// [`search_many`] of one index; panics where it fails (e.g. on a date that does not parse).
pub fn search<'a>(index: &'a Index, req: &SearchRequest) -> SearchResult<'a> {
    search_many(&[("", index, None)], req).expect("search_many failed")
}

/// Searches several indices and merges the hits. Each index is scored with its own statistics,
/// like ES's default query_then_fetch over one shard per index.
pub fn search_many<'a>(
    targets: &[Target<'a>],
    req: &SearchRequest,
) -> Result<SearchResult<'a>, QueryError> {
    let now = now_millis();
    let by_score = req.sort.is_empty() || req.sort.iter().any(|f| f.field == "_score");
    let mut hits = Vec::new();
    let mut searchers = HashMap::new();
    let mut aggs = vec![aggs::Stats::default(); req.aggs.len()];
    let mut profile = req.profile.then(Vec::new);
    let mut failures = Vec::new();
    for (pos, (name, index, filter)) in targets.iter().enumerate() {
        let query = with_filter(index, &req.query, *filter, now)?;
        let mut s = Searcher::unscored(index, &query);
        s.runtime_source = req.runtime_source.clone();
        if by_score || !req.aggs.is_empty() {
            s.gather(&query);
        }
        // Without aggregations to feed, the documents on disk are selected and ordered from
        // postings and doc values where they can be; only the hits returned are read.
        let selected = (req.aggs.is_empty())
            .then(|| s.select_on_disk(pos, &query, &req.sort, by_score))
            .flatten();
        s.on_disk = match selected {
            Some(_) => Some(BTreeSet::new()),
            None => s.candidates(&query, false),
        };
        s.in_memory = s.memory_candidates(&query);
        let index_aggs = aggs::resolve_dates(&req.aggs, index, now)?;
        let mut collector = match aggs::Collector::new(&index_aggs, &s, name, req.profile) {
            Ok(c) => c,
            Err(e) => {
                failures.push((*name, e));
                continue;
            }
        };
        hits.extend(selected.into_iter().flatten());
        for (id, doc) in s.docs().filter(|(_, d)| index.searchable(d.seq_no)) {
            let Some(score) = s.score(&query, id, &doc) else {
                continue;
            };
            collector.collect(id, &doc, score);
            hits.push(Found {
                target: pos,
                id,
                seq_no: doc.seq_no,
                sort: s.sort_values(&req.sort, score, &doc),
                doc: Some(doc),
                score,
            });
        }
        let (stats, p) = collector.finish(matches!(query, Query::MatchAll));
        aggs::reduce(&mut aggs, stats);
        if let (Some(all), Some(p)) = (&mut profile, p) {
            all.push(p);
        }
        searchers.insert(pos, s);
    }
    hits.sort_by(|a, b| {
        // ponytail: ties fall back to index order then write order, standing in for shard/doc order
        cmp_sorted(&req.sort, (&a.sort, a.score), (&b.sort, b.score))
            .then(a.target.cmp(&b.target))
            .then(a.seq_no.cmp(&b.seq_no))
    });
    if !failures.is_empty() && failures.len() == targets.len() {
        // ponytail: ES wraps this in an all-shards-failed search_phase_execution_exception
        return Err(failures.swap_remove(0).1);
    }
    // Elasticsearch collects no top docs for size 0, so it reports no max_score.
    let max_score = (by_score && req.size > 0)
        .then(|| hits.iter().map(|h| h.score as f32).reduce(f32::max))
        .flatten();
    Ok(SearchResult {
        failures,
        aggregations: (!req.aggs.is_empty())
            .then(|| aggs::render(&req.aggs, &req.pipelines, &mut aggs, req.typed_keys))
            .transpose()?,
        profile,
        total: hits.len(),
        max_score,
        hits: hits
            .into_iter()
            .skip(req.from)
            .take(req.size)
            .filter_map(|h| {
                let s = &searchers[&h.target];
                // ponytail: a hit whose segment cannot be read is left out
                let doc = h.doc.or_else(|| s.index.get(h.id))?;
                Some(Hit {
                    index: targets[h.target].0,
                    id: h.id,
                    score: by_score.then_some(h.score as f32),
                    sort: s.sort_values(&req.sort, h.score, &doc),
                    matched_queries: s.matched_queries(h.id, &doc),
                    fields: aggs::hit_fields(s, &req.fields, &doc, true),
                    doc,
                })
            })
            .collect(),
    })
}

/// [`count_many`] of one index; panics where it fails (e.g. on a date that does not parse).
pub fn count(index: &Index, q: &Query) -> usize {
    count_many(&[("", index, None)], q).expect("count_many failed")
}

/// Counts from postings and doc values where they answer the query exactly, on disk and in
/// memory separately; the documents of the other are read and matched.
pub fn count_many(targets: &[Target], q: &Query) -> Result<usize, QueryError> {
    let now = now_millis();
    let mut n = 0;
    for (_, index, filter) in targets {
        let q = with_filter(index, q, *filter, now)?;
        let mut s = Searcher::unscored(index, &q);
        let (disk, memory) = index.live();
        let on_disk = exact(&q, &disk, &|c| s.disk_leaf(c));
        let in_memory = exact(&q, &memory, &|c| match c {
            Query::Term { field, .. } if s.kind(field).0 == Kind::Number => None,
            Query::Term { .. } | Query::Match { .. } => {
                let ids = s.memory_candidates(c)?;
                Some(
                    ids.iter()
                        .filter_map(|id| memory.get(id.as_str()).copied())
                        .collect(),
                )
            }
            _ => None,
        });
        n += on_disk.as_ref().map_or(0, HashSet::len) + in_memory.as_ref().map_or(0, HashSet::len);
        if on_disk.is_some() && in_memory.is_some() {
            continue;
        }
        s.on_disk = match on_disk {
            Some(_) => Some(BTreeSet::new()),
            None => s.candidates(&q, false),
        };
        s.in_memory = match in_memory {
            Some(_) => Some(HashSet::new()),
            None => s.memory_candidates(&q),
        };
        n += (s.docs())
            .filter(|(id, d)| s.score(&q, id, d).is_some())
            .count();
    }
    Ok(n)
}

/// The documents of `all` that `q` matches, from the exact matches `leaf` gives for leaf
/// queries; `None` if it gives none for one.
fn exact<K: Copy + Eq + std::hash::Hash>(
    q: &Query,
    all: &HashSet<K>,
    leaf: &dyn Fn(&Query) -> Option<HashSet<K>>,
) -> Option<HashSet<K>> {
    match q {
        Query::MatchAll => Some(all.clone()),
        Query::Named { query, .. } | Query::Boosted { query, .. } => exact(query, all, leaf),
        Query::Bool {
            must,
            should,
            must_not,
            filter,
            min_should,
        } => {
            let mut out = all.clone();
            for c in must.iter().chain(filter) {
                out = &out & &exact(c, all, leaf)?;
            }
            for c in must_not {
                out = &out - &exact(c, all, leaf)?;
            }
            if *min_should > 0 {
                let mut hits = HashMap::new();
                for c in should {
                    (exact(c, all, leaf)?)
                        .into_iter()
                        .for_each(|k| *hits.entry(k).or_insert(0) += 1);
                }
                out.retain(|k| hits.get(k).is_some_and(|n| n >= min_should));
            }
            Some(out)
        }
        q => Some(&leaf(q)? & all),
    }
}

/// DisjunctionMaxQuery's score of its clauses' `scores`; `None` if there are none.
fn dis_max(scores: impl Iterator<Item = f64>, tie_breaker: f64) -> Option<f64> {
    let (max, sum) = scores.fold(None, |acc: Option<(f64, f64)>, s| {
        Some(acc.map_or((s, s), |(m, t)| (m.max(s), t + s)))
    })?;
    Some(max + (sum - max) * tie_breaker)
}

/// How often the terms with positions `lists` occur in a row: Lucene's ExactPhraseMatcher.
fn exact_phrase_freq(lists: &[Vec<u32>]) -> f64 {
    let at = |i: usize, p: u32| lists[i].binary_search(&(p + i as u32)).is_ok();
    let n = lists[0]
        .iter()
        .filter(|&&p| (1..lists.len()).all(|i| at(i, p)));
    n.count() as f64
}

/// The summed `1 / (1 + match length)` of the phrase's matches within `slop`: Lucene's
/// SloppyPhraseMatcher, with each term's positions moved back by its place in the phrase.
// ponytail: no repeated-term handling (Lucene's rptGroups), so a term repeated in a sloppy
// phrase may match one position twice
fn sloppy_phrase_freq(lists: &[Vec<u32>], slop: u32) -> f64 {
    let mut cur = vec![0; lists.len()];
    let pos = |cur: &[usize], i: usize| i64::from(lists[i][cur[i]]) - i as i64;
    let mut end = (0..lists.len()).map(|i| pos(&cur, i)).max().unwrap_or(0);
    let mut queue: Vec<usize> = (0..lists.len()).collect();
    // the queue is ordered by position, then by place in the phrase
    let pop = |queue: &mut Vec<usize>, cur: &[usize]| {
        let k = (0..queue.len()).min_by_key(|&k| (pos(cur, queue[k]), queue[k]))?;
        Some(queue.swap_remove(k))
    };
    let top = |queue: &[usize], cur: &[usize]| queue.iter().map(|&i| pos(cur, i)).min();
    let mut freq = 0.0;
    let slop = i64::from(slop);
    'matches: while let Some(mut pp) = pop(&mut queue, &cur) {
        let mut next = top(&queue, &cur).unwrap_or(i64::MAX);
        let mut len = end - pos(&cur, pp);
        loop {
            cur[pp] += 1;
            if cur[pp] == lists[pp].len() {
                if len <= slop {
                    freq += 1.0 / (1.0 + len as f32);
                }
                break 'matches;
            }
            end = end.max(pos(&cur, pp));
            if pos(&cur, pp) > next {
                queue.push(pp);
                if len <= slop {
                    freq += 1.0 / (1.0 + len as f32);
                    continue 'matches;
                }
                pp = pop(&mut queue, &cur).unwrap_or(pp);
                next = top(&queue, &cur).unwrap_or(i64::MAX);
                len = end - pos(&cur, pp);
            } else {
                len = len.min(end - pos(&cur, pp));
            }
        }
    }
    f64::from(freq)
}

/// Terms a match query looks up: analyzed for text fields, verbatim otherwise.
fn query_terms(kind: Kind, query: &str) -> Vec<String> {
    match kind {
        Kind::Text => Analyzer::Standard.analyze(query),
        _ => vec![query.to_owned()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use es_index::{Indices, disk};
    use serde_json::json;

    #[test]
    fn date_formats_named_formats_reject_values_they_cannot_read() {
        let f = Some("strict_date_time");
        assert!(bad_date(f, None, &json!("2021-05-01")));
        assert!(!bad_date(f, None, &json!("2021-05-01T00:00:00Z")));
        assert!(!bad_date(Some("basic_date"), None, &json!("20210501")));
        // a format es-mini lacks reads values as the default format, so none are refused
        assert!(!bad_date(Some("week_date"), None, &json!("2021-05-01")));
    }

    #[test]
    fn date_formats_dates_past_a_long_of_millis_are_illegal() {
        let mut ix = Index::default();
        let date = FieldMapping::Date {
            format: None,
            locale: None,
            fields: Default::default(),
        };
        ix.mappings.properties.insert("d".into(), date);
        let range = |bound: &str, v: Value| {
            let q = json!({"range": {"d": {bound: v, "format": "date_optional_time"}}});
            resolve_dates(&ix, parse_query(&q).unwrap(), 0)
        };
        let too_far = |when: &str, date: &str| {
            QueryError::illegal(format!(
                "date[{date}] is too far in the {when} to be represented in a long milliseconds variable"
            ))
        };
        // Elasticsearch 9.5.5: the long's range ends in 292278994 and starts in -292275055
        assert!(range("gte", json!("292278994-01-01")).is_ok());
        assert!(range("gte", json!("-292275054-01-01")).is_ok());
        assert_eq!(
            range("gte", json!("-292278994-01-01")),
            Err(too_far("past", "-292278994-01-01T00:00:00Z"))
        );
        // bare numbers are years in this format (search/500_date_range.yml)
        assert_eq!(
            range("gte", json!(-522_000_000)),
            Err(too_far("past", "-522000000-01-01T00:00:00Z"))
        );
        assert_eq!(
            range("gte", json!("292278995-03-04T05:06:07.1Z")),
            Err(too_far("future", "+292278995-03-04T05:06:07.100Z"))
        );
        let up = range("lte", json!(522_000_000)).unwrap_err();
        assert_eq!(up.kind, ErrorKind::IllegalArgument, "{up:?}");
    }

    #[test]
    fn date_formats_date_nanos_sort_by_epoch_nanos() {
        let mut ix = Index::default();
        let nanos = FieldMapping::DateNanos {
            format: None,
            locale: None,
        };
        ix.mappings.properties.insert("n".into(), nanos);
        // as text, 2's value sorts last; as an instant it is the day before 1's
        ix.put("1", json!({"n": "2016-01-03T00:00:00.123456789Z"}));
        ix.put("2", json!({"n": "2016-01-03T01:00:00+02:00"}));
        ix.put("3", json!({"n": "1970-01-01T00:00:00.000000001Z"}));
        let body = json!({"sort": [{"n": "asc"}]});
        let r = search(&ix, &SearchRequest::parse(&body).unwrap());
        // epoch nanos, as Elasticsearch 9.5.5 returns them
        let sorts: Vec<_> = r.hits.iter().map(|h| (h.id, h.sort[0].clone())).collect();
        assert_eq!(
            sorts,
            [
                ("3", json!(1)),
                ("2", json!(1_451_775_600_000_000_000_i64)),
                ("1", json!(1_451_779_200_123_456_789_i64)),
            ]
        );
    }

    #[test]
    fn queries_on_an_opened_index_read_only_the_docs_their_postings_hold() {
        let dir = std::env::temp_dir().join(format!("esm-candidates-{}", std::process::id()));
        let mut ix = Index::default();
        let props = &mut ix.mappings.properties;
        props.insert("title".into(), FieldMapping::Text { store: false });
        props.insert(
            "tag".into(),
            FieldMapping::Keyword {
                ignore_above: None,
                split_queries_on_whitespace: false,
            },
        );
        props.insert("ok".into(), FieldMapping::Boolean);
        for i in 0..50 {
            let tag = if i % 10 == 0 { "ten" } else { "other" };
            ix.put(
                &i.to_string(),
                json!({"title": format!("doc {i}"), "tag": tag, "ok": i == 4}),
            );
        }
        disk::save(&dir, &Indices::from([("i".to_owned(), ix)])).unwrap();
        let opened = disk::open(&dir).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        let ix = &opened["i"];
        let read = |q: Value| {
            let q = parse_query(&q).unwrap();
            let on_disk = Searcher::new(ix, &q)
                .candidates(&q, false)
                .expect("narrowed");
            let mut ids: Vec<_> = ix.docs_at(None, Some(&on_disk)).map(|(id, _)| id).collect();
            ids.sort_by_key(|id| id.parse::<u32>().unwrap());
            ids.join(",")
        };
        assert_eq!(read(json!({"match": {"title": "7"}})), "7");
        assert_eq!(read(json!({"match": {"title": "7 8"}})), "7,8");
        assert_eq!(read(json!({"term": {"tag": "ten"}})), "0,10,20,30,40");
        assert_eq!(read(json!({"term": {"ok": true}})), "4");
        assert_eq!(read(json!({"term": {"_id": "3"}})), "3");
        let bool = json!({"bool": {
            "filter": [{"term": {"tag": "ten"}}],
            "should": [{"match": {"title": "20 30 31"}}],
            "minimum_should_match": 1,
        }});
        assert_eq!(read(bool), "20,30");
    }

    #[test]
    fn range_and_number_queries_on_an_opened_index_read_only_the_docs_their_doc_values_hold() {
        let dir = std::env::temp_dir().join(format!("esm-ranges-{}", std::process::id()));
        let mut ix = Index::default();
        let props = &mut ix.mappings.properties;
        props.insert("n".into(), FieldMapping::Long);
        props.insert("price".into(), FieldMapping::Double);
        let date = FieldMapping::Date {
            format: None,
            locale: None,
            fields: Default::default(),
        };
        props.insert("when".into(), date);
        for i in 0..50 {
            let doc = json!({"n": i, "price": i as f64 * 0.5, "when": format!("20{i:02}-01-01")});
            ix.put(&i.to_string(), doc);
        }
        disk::save(&dir, &Indices::from([("i".to_owned(), ix)])).unwrap();
        let opened = disk::open(&dir).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        let ix = &opened["i"];
        let read = |q: Value| {
            let q = parse_query(&q).unwrap();
            let on_disk = Searcher::new(ix, &q)
                .candidates(&q, false)
                .expect("narrowed");
            let mut ids: Vec<_> = ix.docs_at(None, Some(&on_disk)).map(|(id, _)| id).collect();
            ids.sort_by_key(|id| id.parse::<u32>().unwrap());
            ids.join(",")
        };
        assert_eq!(
            read(json!({"range": {"n": {"gte": 45, "lt": 48}}})),
            "45,46,47"
        );
        assert_eq!(
            read(json!({"range": {"price": {"gt": 2, "lte": "3.0"}}})),
            "5,6"
        );
        assert_eq!(
            read(json!({"range": {"when": {"gte": "2047-06-01"}}})),
            "48,49"
        );
        assert_eq!(read(json!({"term": {"n": 7}})), "7");
        assert_eq!(read(json!({"term": {"price": "1.5"}})), "3");
        let bool = json!({"bool": {"filter": [
            {"range": {"n": {"lt": 10}}},
            {"range": {"price": {"gte": 4}}},
        ]}});
        assert_eq!(read(bool), "8,9");
        // number terms score 1.0: no BM25 statistics, which would read every source
        let q = parse_query(&json!({"match": {"n": 7}})).unwrap();
        assert!(Searcher::new(ix, &q).field_stats.is_empty());
    }

    /// Doc values hold dates as their mapping's `format` reads them, so ranges counted from
    /// them agree with the sources.
    #[test]
    fn date_ranges_on_an_opened_index_read_the_mapping_format() {
        let dir = std::env::temp_dir().join(format!("esm-date-format-{}", std::process::id()));
        let mut ix = Index::default();
        let mapping = json!({"type": "date", "format": "epoch_second"});
        let mapping = serde_json::from_value(mapping).unwrap();
        ix.mappings.properties.insert("d".into(), mapping);
        ix.put("1", json!({"d": 1_704_067_200}));
        disk::save(&dir, &Indices::from([("i".to_owned(), ix)])).unwrap();
        let opened = disk::open(&dir).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        let n = |b: Value| {
            let q = json!({"range": {"d": {"gte": b, "lte": b}}});
            count(&opened["i"], &parse_query(&q).unwrap())
        };
        assert_eq!(n(json!("1704067200")), 1);
        // as in Elasticsearch, number bounds are epoch millis whatever the mapping's format
        assert_eq!(n(json!(1_704_067_200)), 0);
        assert_eq!(n(json!(1_704_067_200_000_i64)), 1);
    }

    #[test]
    fn float_ranges_on_an_opened_index_narrow_by_their_f32_doc_values() {
        let dir = std::env::temp_dir().join(format!("esm-float-ranges-{}", std::process::id()));
        let mut ix = Index::default();
        (ix.mappings.properties).insert("f".into(), FieldMapping::Float);
        for i in 0..20 {
            ix.put(&i.to_string(), json!({"f": f64::from(i) / 10.0}));
        }
        disk::save(&dir, &Indices::from([("i".to_owned(), ix)])).unwrap();
        let opened = disk::open(&dir).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        let ix = &opened["i"];
        let read = |q: Value| {
            let q = parse_query(&q).unwrap();
            let s = Searcher::new(ix, &q);
            // values a strict bound excludes may round onto it, so floats only narrow
            assert_eq!(s.candidates(&q, true), None);
            let on_disk = s.candidates(&q, false).expect("narrowed");
            let mut ids: Vec<_> = ix.docs_at(None, Some(&on_disk)).map(|(id, _)| id).collect();
            ids.sort_by_key(|id| id.parse::<u32>().unwrap());
            ids.join(",")
        };
        // 1.1 is above 1.1 as an f32, as 0.3 is below 0.3
        let range = json!({"range": {"f": {"gte": 0.3, "lte": 1.1}}});
        assert_eq!(read(range), "3,4,5,6,7,8,9,10,11");
        let range = json!({"range": {"f": {"gt": 0.3, "lt": "1.1"}}});
        assert_eq!(read(range), "3,4,5,6,7,8,9,10,11");
        assert_eq!(read(json!({"term": {"f": 1.1}})), "11");
        let q = parse_query(&json!({"range": {"f": {"gt": 0.3, "lte": 1.1}}})).unwrap();
        assert_eq!(count(ix, &q), 8);
    }

    /// Zeroes the stored fields (`.fdt`) in every segment of every shard under `dir`.
    fn zero_stored_fields(dir: &std::path::Path) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                zero_stored_fields(&path);
                continue;
            }
            if !path
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("segments_")
            {
                continue;
            }
            for seg in es_codec::SegmentInfos::read_latest(dir).unwrap().segments {
                let seg = es_codec::SegmentReader::open(dir, seg).unwrap();
                let fdt = seg.cfs.file(".fdt").unwrap().unwrap();
                let cfs = dir.join(format!("{}.cfs", seg.seg.name));
                let mut bytes = std::fs::read(&cfs).unwrap();
                let at = (bytes.windows(fdt.len()).position(|w| w == fdt)).unwrap();
                bytes[at..at + fdt.len()].fill(0);
                std::fs::write(&cfs, bytes).unwrap();
            }
        }
    }

    #[test]
    fn count_reads_no_sources() {
        let dir = std::env::temp_dir().join(format!("esm-count-{}", std::process::id()));
        let mut ix = Index::default();
        let props = &mut ix.mappings.properties;
        props.insert("title".into(), FieldMapping::Text { store: false });
        props.insert(
            "tag".into(),
            FieldMapping::Keyword {
                ignore_above: None,
                split_queries_on_whitespace: false,
            },
        );
        props.insert("n".into(), FieldMapping::Long);
        props.insert("ok".into(), FieldMapping::Boolean);
        for i in 0..30 {
            let tag = if i % 3 == 0 { "three" } else { "other" };
            let doc = json!({"title": format!("doc {i}"), "tag": tag, "n": i, "ok": i % 2 == 0});
            ix.put(&i.to_string(), doc);
        }
        disk::save(&dir, &Indices::from([("i".to_owned(), ix)])).unwrap();
        let mut opened = disk::open(&dir).unwrap();
        let search = |ix: &Index, body: Value| {
            let r = search(ix, &SearchRequest::parse(&body).unwrap());
            (
                r.total,
                r.hits.iter().map(|h| h.id.to_owned()).collect::<Vec<_>>(),
            )
        };
        let three = json!({"constant_score": {"filter": {"term": {"tag": "three"}}}});
        let by_n = |order: &str, size: usize| json!({"query": three, "sort": [{"n": order}], "size": size});
        // sorted from doc values, then only the hits returned are read
        let first = search(&opened["i"], by_n("asc", 3));
        assert_eq!(first, (10, vec!["0".into(), "3".into(), "6".into()]));
        zero_stored_fields(&dir);
        let ix = opened.get_mut("i").unwrap();
        assert!(ix.get("3").is_none(), "on-disk sources are unreadable");
        // writes after opening stay in memory
        ix.delete("0");
        ix.put(
            "6",
            json!({"title": "doc six", "tag": "other", "n": 6, "ok": true}),
        );
        ix.put(
            "30",
            json!({"title": "doc 30", "tag": "three", "n": 30, "ok": true}),
        );
        let ix = &*ix;
        let n = |q: Value| count(ix, &parse_query(&q).unwrap());
        assert_eq!(n(json!({"match_all": {}})), 30);
        assert_eq!(n(json!({"term": {"tag": "three"}})), 9);
        assert_eq!(n(json!({"match": {"title": "doc"}})), 30);
        assert_eq!(n(json!({"match": {"title": "six 7"}})), 2);
        assert_eq!(n(json!({"term": {"ok": false}})), 15);
        assert_eq!(n(json!({"term": {"_id": "4"}})), 1);
        assert_eq!(n(json!({"terms": {"tag": ["three", "none"]}})), 9);
        let filtered = json!({"bool": {
            "filter": [{"term": {"tag": "three"}}],
            "must_not": [{"term": {"ok": true}}],
        }});
        assert_eq!(n(filtered), 5);
        let should = json!({"bool": {
            "should": [{"term": {"tag": "three"}}, {"term": {"ok": true}}],
            "minimum_should_match": 2,
        }});
        assert_eq!(n(should), 4);
        let constant = json!({"constant_score": {"filter": {"term": {"tag": "other"}}}});
        assert_eq!(n(constant), 21);
        // on disk from doc values; the documents in memory are scanned
        assert_eq!(n(json!({"range": {"n": {"gte": 5, "lt": 10}}})), 5);
        // searches that need no scores select from postings and doc values too
        let filter_only = json!({"query": {"bool": {"filter": {"term": {"tag": "three"}}}}});
        assert_eq!(search(ix, filter_only), (9, vec!["30".into()]));
        assert_eq!(search(ix, by_n("desc", 1)), (9, vec!["30".into()]));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn scores_read_no_sources() {
        let dir = std::env::temp_dir().join(format!("esm-scores-{}", std::process::id()));
        let mut ix = Index::default();
        let props = &mut ix.mappings.properties;
        props.insert("title".into(), FieldMapping::Text { store: false });
        props.insert(
            "tag".into(),
            FieldMapping::Keyword {
                ignore_above: None,
                split_queries_on_whitespace: false,
            },
        );
        props.insert("n".into(), FieldMapping::Long);
        for i in 0..30 {
            let title = format!("doc {i} {}", "fox ".repeat(i % 4));
            let tag = ["a", "b"][i % 2];
            let doc = json!({"title": title, "tag": tag, "n": i});
            ix.put(&i.to_string(), doc);
        }
        disk::save(&dir, &Indices::from([("i".to_owned(), ix)])).unwrap();
        let opened = disk::open(&dir).unwrap();
        let queries = [
            json!({"match": {"title": "fox 7"}}),
            json!({"term": {"tag": "a"}}),
            json!({"bool": {
                "must": [{"match": {"title": "fox"}}],
                "should": [{"term": {"tag": "b"}}, {"match": {"title": "3 4"}}],
                "filter": [{"range": {"n": {"lt": 20}}}],
                "must_not": [{"term": {"title": "9"}}],
            }}),
        ];
        let search = |ix: &Index, q: &Value, size: usize| {
            let body = json!({"query": q, "size": size});
            let r = search(ix, &SearchRequest::parse(&body).unwrap());
            let hits = r.hits.iter().map(|h| (h.id.to_owned(), h.score));
            (r.total, r.max_score, hits.collect::<Vec<_>>())
        };
        let before: Vec<_> = queries.iter().map(|q| search(&opened["i"], q, 3)).collect();
        zero_stored_fields(&dir);
        let ix = &opened["i"];
        assert!(ix.get("3").is_none(), "on-disk sources are unreadable");
        // scored without reading a source; the hit returned cannot be read, so is left out
        for (q, (total, max, _)) in queries.iter().zip(before) {
            assert!(total > 0 && max.is_some(), "{q}");
            assert_eq!(search(ix, q, 1), (total, max, vec![]), "{q}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Scores come from postings in memory too: a keyword counts once per document and has
    /// no norms, as in Elasticsearch, however often a document repeats it.
    #[test]
    fn keyword_scores_count_a_term_once() {
        let mut ix = Index::default();
        let keyword = FieldMapping::Keyword {
            ignore_above: None,
            split_queries_on_whitespace: false,
        };
        ix.mappings.properties.insert("tag".into(), keyword);
        ix.put("1", json!({"tag": ["a", "a", "b"]}));
        ix.put("2", json!({"tag": "a"}));
        let body = json!({"query": {"term": {"tag": "a"}}});
        let r = search(&ix, &SearchRequest::parse(&body).unwrap());
        assert_eq!(r.hits[0].score, r.hits[1].score);
    }

    /// The score of the only document a query matches.
    fn boost_score(q: Value) -> f64 {
        let mut ix = Index::default();
        let props = &mut ix.mappings.properties;
        props.insert("title".into(), FieldMapping::Text { store: false });
        props.insert(
            "tag".into(),
            FieldMapping::Keyword {
                ignore_above: None,
                split_queries_on_whitespace: false,
            },
        );
        ix.put("1", json!({"title": "a b", "tag": "x", "n": 5}));
        ix.put("2", json!({"title": "c", "tag": "y"}));
        let r = search(&ix, &SearchRequest::parse(&json!({"query": q})).unwrap());
        assert_eq!(r.hits.len(), 1, "{r:?}");
        f64::from(r.hits[0].score.unwrap())
    }

    /// `kind` with `boost: 3` scores 3 times as much as without.
    fn boost_triples(plain: Value, boosted: Value) {
        let (a, b) = (boost_score(plain), boost_score(boosted));
        assert!(a > 0.0 && (b - 3.0 * a).abs() < 1e-5 * b, "{a} {b}");
    }

    #[test]
    fn boost_constant_score() {
        let q =
            |b: Value| json!({"constant_score": {"filter": {"term": {"tag": "x"}}, "boost": b}});
        assert_eq!(boost_score(q(json!(2.5))), 2.5);
        assert_eq!(boost_score(q(json!(1.0))), 1.0);
    }

    #[test]
    fn boost_nested_multiplies() {
        let inner = json!({"constant_score": {"filter": {"term": {"tag": "x"}}, "boost": 2.0}});
        let q = json!({"bool": {"must": [inner], "boost": 3.0}});
        assert_eq!(boost_score(q), 6.0);
    }

    #[test]
    fn boost_term() {
        boost_triples(
            json!({"term": {"title": "a"}}),
            json!({"term": {"title": {"value": "a", "boost": 3}}}),
        );
    }

    #[test]
    fn boost_terms() {
        boost_triples(
            json!({"terms": {"tag": ["x"]}}),
            json!({"terms": {"tag": ["x"], "boost": 3}}),
        );
    }

    #[test]
    fn boost_match() {
        boost_triples(
            json!({"match": {"title": "a"}}),
            json!({"match": {"title": {"query": "a", "boost": 3}}}),
        );
    }

    #[test]
    fn boost_range() {
        boost_triples(
            json!({"range": {"n": {"gte": 1}}}),
            json!({"range": {"n": {"gte": 1, "boost": 3}}}),
        );
    }

    #[test]
    fn boost_ids() {
        boost_triples(
            json!({"ids": {"values": ["1"]}}),
            json!({"ids": {"values": ["1"], "boost": 3}}),
        );
    }

    #[test]
    fn boost_exists() {
        boost_triples(
            json!({"exists": {"field": "n"}}),
            json!({"exists": {"field": "n", "boost": 3}}),
        );
    }

    #[test]
    fn boost_wildcard() {
        boost_triples(
            json!({"wildcard": {"tag": "x*"}}),
            json!({"wildcard": {"tag": {"value": "x*", "boost": 3}}}),
        );
    }

    #[test]
    fn boost_prefix() {
        boost_triples(
            json!({"prefix": {"tag": "x"}}),
            json!({"prefix": {"tag": {"value": "x", "boost": 3}}}),
        );
    }

    #[test]
    fn boost_bool() {
        boost_triples(
            json!({"bool": {"must": [{"match": {"title": "a"}}]}}),
            json!({"bool": {"must": [{"match": {"title": "a"}}], "boost": 3}}),
        );
    }
}
