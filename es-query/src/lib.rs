//! JSON query DSL and ES|QL parsing/execution.
//!
//! Term, match and bool queries on an index opened from disk read only the on-disk documents
//! their postings name; the rest scan every document. `_count` reads no documents where postings
//! and doc values answer its query exactly (see [`count_many`]).
//!
//! ponytail: matched documents are still scored from their sources; score from postings and
//! norms if loading the hits shows up.

mod aggs;
pub mod esql;
mod hll;
mod hll_tables;
mod parse;
mod tdigest;

pub use parse::{Query, parse_query, parse_query_string};

use es_index::postings::{text, values};
use es_index::{
    Document, FieldMapping, Index, analysis::Analyzer, disk::DiskPos, parse_date_millis,
};
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
fn wildcard_match(p: &[char], t: &[char]) -> bool {
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
    match index.mappings.properties.get(field) {
        Some(FieldMapping::Text { .. } | FieldMapping::TextKeyword { .. }) => Kind::Text,
        Some(FieldMapping::Keyword { .. }) => Kind::Keyword,
        Some(FieldMapping::Boolean) => Kind::Bool,
        // ponytail: dates match and sort as their JSON values (only range parses instants); no date math
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

fn num(v: &Value) -> Option<f64> {
    v.as_f64().or_else(|| v.as_str()?.parse().ok())
}

const MONTHS: [(&str, [&str; 12]); 2] = [
    (
        "fr",
        [
            "janv.", "févr.", "mars", "avr.", "mai", "juin", "juil.", "août", "sept.", "oct.",
            "nov.", "déc.",
        ],
    ),
    (
        "en",
        [
            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
        ],
    ),
];

/// Epoch millis of `"E, d MMM yyyy HH:mm:ss Z"` with localized month names.
// ponytail: only this one custom format; generalize to a token-driven parser when another is needed.
fn parse_dated(v: &Value, locale: &str) -> Option<i64> {
    let months = MONTHS.iter().find(|(l, _)| *l == locale)?.1;
    let mut p = v.as_str()?.split_once(", ")?.1.split(' ');
    let d: i64 = p.next()?.parse().ok()?;
    let name = p.next()?;
    let m = months.iter().position(|n| *n == name)? as i64 + 1;
    let y: i64 = p.next()?.parse().ok()?;
    let mut t = p.next()?.split(':').map(|x| x.parse::<i64>());
    let secs = t.next()?.ok()? * 3600 + t.next()?.ok()? * 60 + t.next()?.ok()?;
    let z = p.next()?;
    let off = (z[1..3].parse::<i64>().ok()? * 3600 + z[3..5].parse::<i64>().ok()? * 60)
        * if z.starts_with('-') { -1 } else { 1 };
    Some((es_index::days_from_civil(y, m, d) * 86400 + secs - off) * 1000)
}

/// Epoch millis of a value of a date field with `format` and `locale`.
pub fn date_millis(format: Option<&str>, locale: Option<&str>, v: &Value) -> Option<i64> {
    let Some(format) = format else {
        return parse_date_millis(v);
    };
    (format.split("||"))
        .find_map(|f| match f {
            "epoch_millis" => num(v).map(|x| x as i64),
            "epoch_second" => num(v).map(|x| (x * 1000.0) as i64),
            // ponytail: other named formats (`strict_date_time`, ...) parse as the default format
            _ if es_index::format_pattern(f, 0).is_none() => {
                v.as_str().and_then(|_| parse_date_millis(v))
            }
            _ => es_index::parse_pattern(f, v.as_str()?),
        })
        .or_else(|| parse_dated(v, locale.unwrap_or("en")))
}

/// Whether a value of a date field with `format` surely cannot be read: every format is an
/// epoch or a pattern and none reads it.
// ponytail: named formats only parse as the default format, so they never reject values
pub fn bad_date(format: Option<&str>, locale: Option<&str>, v: &Value) -> bool {
    let known = |f: &str| f.starts_with("epoch_") || es_index::format_pattern(f, 0).is_some();
    format.is_some_and(|f| f.split("||").all(known)) && date_millis(format, locale, v).is_none()
}

/// Exact integer value, so longs above 2^53 keep their precision.
fn int(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_str()?.parse().ok())
}

/// Whether a value passes a range query's `[gt, gte, lt, lte]` bounds, given how it compares
/// with a bound.
fn in_range(bounds: [Option<&Value>; 4], cmp: impl Fn(&Value) -> Ordering) -> bool {
    let [gt, gte, lt, lte] = bounds;
    gt.is_none_or(|b| cmp(b).is_gt())
        && gte.is_none_or(|b| cmp(b).is_ge())
        && lt.is_none_or(|b| cmp(b).is_lt())
        && lte.is_none_or(|b| cmp(b).is_le())
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
            kinds: RefCell::default(),
            on_disk: None,
            in_memory: None,
            named: named_queries(q),
            query: q.clone(),
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

    /// Precomputes BM25 statistics for every scored (field, term) in the query.
    fn gather(&mut self, q: &Query) {
        let (field, wanted) = match q {
            Query::Match { field, query, .. } => {
                let (kind, _) = self.kind(field);
                let t = query_terms(kind, query);
                (field, t)
            }
            Query::Term { field, value } => (field, vec![text(value)]),
            Query::Bool { must, should, .. } => {
                must.iter().chain(should).for_each(|c| self.gather(c));
                return;
            }
            Query::Named { query, .. } => return self.gather(query),
            _ => return,
        };
        let (kind, path) = self.kind(field);
        // they score 1.0 (see score_terms)
        if matches!(kind, Kind::Number | Kind::Bool) {
            return;
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
        for source in live.chain(self.index.superseded().iter().map(Cow::Borrowed)) {
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
            } => self.number_docs(field, [gt, gte, lt, lte].map(Option::as_ref), exact),
            Query::Match { field, query, and } => {
                let (kind, _) = self.kind(field);
                self.term_docs(field, &query_terms(kind, query), *and, exact)
            }
            Query::Named { query, .. } => self.candidates(query, exact),
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

    /// The in-memory documents `q` can match, from in-memory postings; `None` if it cannot
    /// narrow them (numbers and ranges scan).
    fn memory_candidates(&self, q: &Query) -> Option<HashSet<String>> {
        let lookup = |field: &str, terms: &[String], all: bool| {
            let (kind, path) = self.kind(field);
            match kind {
                Kind::Number => None,
                _ => self.index.memory_docs(path, kind == Kind::Text, terms, all),
            }
        };
        match q {
            // An `_id` term matches the document's id, not a source field.
            Query::Term { field, value } if field == "_id" => Some(HashSet::from([text(value)])),
            Query::Term { field, value } => lookup(field, &[text(value)], true),
            Query::Match { field, query, and } => {
                lookup(field, &query_terms(self.kind(field).0, query), *and)
            }
            Query::Named { query, .. } => self.memory_candidates(query),
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

    /// The on-disk documents whose doc values of numeric or date `field` are within `bounds`
    /// (see [`in_range`] and [`Index::disk_numbers`]).
    fn number_docs(
        &self,
        field: &str,
        bounds: [Option<&Value>; 4],
        exact: bool,
    ) -> Option<BTreeSet<DiskPos>> {
        let instant = self.instant(field);
        self.index.disk_numbers(
            field,
            |v| {
                in_range(bounds, |b| match &instant {
                    None => scalar_cmp(v, b),
                    // as scoring does, a bound that is not a date is above every value
                    Some(instant) => {
                        (v.as_i64().zip(instant(b))).map_or(Ordering::Less, |(x, y)| x.cmp(&y))
                    }
                })
            },
            exact,
        )
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
            Query::Match { field, query, .. } => self.explain_terms(field, query_terms, query, d),
            Query::Term { field, value } if field != "_id" => {
                self.explain_terms(field, |_, v| vec![v.to_owned()], &text(value), d)
            }
            Query::Bool { must, should, .. } => {
                // ponytail: filter/must_not clauses are left out of the explanation
                let details = (must.iter().chain(should))
                    .filter(|c| self.score(c, id, d).is_some())
                    .map(|c| self.explain_query(c, id, d));
                return explanation(score, "sum of:".into(), details.collect());
            }
            Query::Named { query, .. } => return self.explain_query(query, id, d),
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
        d: &Document,
    ) -> Vec<Value> {
        let (kind, path) = self.kind(field);
        if matches!(kind, Kind::Number | Kind::Bool) {
            return vec![];
        }
        let doc_terms = terms(kind, &values(&d.source, path));
        let mut out = vec![];
        for w in analyze(kind, query) {
            let freq = doc_terms.iter().filter(|t| **t == w).count() as f64;
            if freq == 0.0 {
                continue;
            }
            let [df, n, len, avg, idf, tf] =
                self.bm25_parts(field, &w, freq, doc_terms.len() as f64);
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
            let weight = self.bm25(field, &w, freq, doc_terms.len() as f64);
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
                let (kind, path) = self.kind(field);
                let wanted = query_terms(kind, query);
                self.score_terms(field, kind, path, &wanted, *and, d)
            }
            Query::Term { field, value } if field == "_id" => (text(value) == id).then_some(1.0),
            Query::Term { field, value } => {
                let (kind, path) = self.kind(field);
                self.score_terms(field, kind, path, &[text(value)], true, d)
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
            } => {
                let (_, path) = self.kind(field);
                let instant = self.instant(field);
                let cmp = |a: &Value, b: &Value| match &instant {
                    None => scalar_cmp(a, b),
                    Some(instant) => match (instant(a), instant(b)) {
                        (Some(x), Some(y)) => x.cmp(&y),
                        _ => Ordering::Less,
                    },
                };
                let bounds = [gt, gte, lt, lte].map(Option::as_ref);
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
            Query::Named { query, .. } => self.score(query, id, d),
        }
    }

    /// The `_name`s of the query's named queries a matching document matches.
    fn matched_queries(&self, id: &str, d: &Document) -> Vec<String> {
        (self.named.iter())
            .filter(|(_, q)| self.score(q, id, d).is_some())
            .map(|(name, _)| name.clone())
            .collect()
    }

    fn score_terms(
        &self,
        field: &str,
        kind: Kind,
        path: &str,
        wanted: &[String],
        all: bool,
        d: &Document,
    ) -> Option<f64> {
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

#[derive(Debug, Clone, PartialEq)]
pub struct SortField {
    pub field: String,
    pub desc: bool,
    /// `unmapped_type` was given, so a missing mapping is not an error.
    pub unmapped_ok: bool,
    /// `missing`: `_first`, `_last` (the default) or a value to sort absent fields as.
    pub missing: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SearchRequest {
    pub query: Query,
    pub from: usize,
    pub size: usize,
    pub sort: Vec<SortField>,
    /// Body `track_total_hits` (`true`, `false` or a number) as the URL-parameter spelling.
    pub track_total_hits: Option<String>,
    pub aggs: Vec<aggs::Agg>,
    /// The `typed_keys` URL parameter: aggregation names are prefixed with their type.
    pub typed_keys: bool,
    /// Body `profile`: report where each shard's aggregation time went.
    pub profile: bool,
    /// Body `fields`: the mapped fields each hit fetches.
    pub fields: Vec<String>,
}

/// Which Elasticsearch error type a query failure maps to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// `parsing_exception`
    Parsing,
    /// `illegal_argument_exception`
    IllegalArgument,
    /// `x_content_parse_exception`
    XContentParse,
    /// `too_many_buckets_exception`, past `search.max_buckets`
    TooManyBuckets,
}

/// ES's `search.max_buckets`: a search fails rather than render more aggregation buckets.
pub const MAX_BUCKETS: usize = 65_536;

/// A query failure: its Elasticsearch error type and reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryError {
    pub kind: ErrorKind,
    pub msg: String,
}

impl QueryError {
    pub fn illegal(msg: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::IllegalArgument,
            msg: msg.into(),
        }
    }

    pub fn x_content(msg: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::XContentParse,
            msg: msg.into(),
        }
    }
}

/// A bare message is a `parsing_exception`.
impl From<String> for QueryError {
    fn from(msg: String) -> Self {
        Self {
            kind: ErrorKind::Parsing,
            msg,
        }
    }
}

impl From<&str> for QueryError {
    fn from(msg: &str) -> Self {
        msg.to_string().into()
    }
}

impl std::fmt::Display for QueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.msg)
    }
}

impl SearchRequest {
    pub fn parse(body: &Value) -> Result<Self, QueryError> {
        if let Some(k) = body.as_object().and_then(|o| {
            o.keys().find(|k| {
                ![
                    "query",
                    "from",
                    "size",
                    "sort",
                    "track_total_hits",
                    "aggs",
                    "aggregations",
                    "profile",
                    "fields",
                ]
                .contains(&k.as_str())
            })
        }) {
            return Err(format!("Unknown key for a START_OBJECT in [{k}].").into());
        }
        let query = body.get("query").map_or(Ok(Query::MatchAll), parse_query)?;
        let usize_of = |k: &str, d| match body.get(k) {
            None => Ok(d),
            Some(v) => match v.as_i64() {
                Some(n) if n < 0 => Err(QueryError::illegal(format!(
                    "[{k}] parameter cannot be negative, found [{n}]"
                ))),
                Some(n) => Ok(n as usize),
                None => Err(format!("[{k}] must be a number").into()),
            },
        };
        let sort = body.get("sort").map_or(Ok(vec![]), parse_sorts)?;
        let track_total_hits = match body.get("track_total_hits") {
            None => None,
            Some(v) if v.is_boolean() || v.is_i64() => Some(v.to_string()),
            Some(_) => return Err("[track_total_hits] must be a boolean or a number".into()),
        };
        let mut aggs = match (body.get("aggs"), body.get("aggregations")) {
            (Some(_), Some(_)) => {
                return Err("Found two aggregation definitions: [aggs] and [aggregations]".into());
            }
            (Some(a), _) | (_, Some(a)) => aggs::parse(a)?,
            (None, None) => vec![],
        };
        let fields = match body.get("fields") {
            None => vec![],
            Some(v) => aggs::field_names(v).ok_or("[fields] must be field names")?,
        };
        aggs::inherit_fields(&mut aggs, &fields);
        Ok(Self {
            fields,
            aggs,
            typed_keys: false,
            profile: body.get("profile").and_then(Value::as_bool) == Some(true),
            query,
            track_total_hits,
            from: usize_of("from", 0)?,
            size: usize_of("size", 10)?,
            sort,
        })
    }
}

/// A `sort`: one sort field or an array of them.
fn parse_sorts(v: &Value) -> Result<Vec<SortField>, QueryError> {
    match v {
        Value::Array(a) => a.iter().map(parse_sort).collect(),
        s => Ok(vec![parse_sort(s)?]),
    }
}

fn parse_sort(v: &Value) -> Result<SortField, QueryError> {
    let desc_of = |o: &Value| match o.as_str() {
        Some("desc") => Ok(true),
        Some("asc") => Ok(false),
        _ => Err(format!("invalid sort order [{o}]")),
    };
    match v {
        Value::String(f) => Ok(SortField {
            desc: f == "_score",
            field: f.clone(),
            unmapped_ok: false,
            missing: None,
        }),
        Value::Object(m) if m.len() == 1 => {
            let (field, p) = m.iter().next().expect("len checked");
            let order = match p {
                Value::Object(o) => o.get("order"),
                o => Some(o),
            };
            let desc = match order {
                Some(o) => desc_of(o)?,
                None => field == "_score",
            };
            Ok(SortField {
                field: field.clone(),
                desc,
                unmapped_ok: p.get("unmapped_type").is_some(),
                missing: p.get("missing").cloned(),
            })
        }
        _ => Err(format!("malformed sort [{v}]").into()),
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
fn with_filter(q: &Query, filter: Option<&Value>) -> Result<Query, QueryError> {
    Ok(match filter {
        None => q.clone(),
        Some(f) => Query::Bool {
            must: vec![q.clone()],
            should: vec![],
            must_not: vec![],
            filter: vec![parse_query(f)?],
            min_should: 0,
        },
    })
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
                })
                // ponytail: object fields are not mapped here, so accept any path present in a source
                || index
                    .docs()
                    .any(|(_, d)| !values(&d.source, &f.field).is_empty());
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

impl Searcher<'_> {
    /// A matching document's values for `sort` (`score` its query score).
    fn sort_values(&self, sort: &[SortField], score: f64, doc: &Document) -> Vec<Value> {
        sort.iter()
            .map(|f| match f.field.as_str() {
                "_score" => Value::from(score as f32),
                field => {
                    let (_, path) = self.kind(field);
                    let mut v = values(&doc.source, path);
                    v.sort_by(|a, b| scalar_cmp(a, b));
                    let pick = if f.desc { v.last() } else { v.first() };
                    pick.map_or_else(
                        || match &f.missing {
                            Some(m) if !matches!(m.as_str(), Some("_first" | "_last")) => m.clone(),
                            _ => Value::Null,
                        },
                        |v| (*v).clone(),
                    )
                }
            })
            .collect()
    }
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

pub fn search<'a>(index: &'a Index, req: &SearchRequest) -> SearchResult<'a> {
    search_many(&[("", index, None)], req).expect("no alias filter to parse")
}

/// Searches several indices and merges the hits. Each index is scored with its own statistics,
/// like ES's default query_then_fetch over one shard per index.
pub fn search_many<'a>(
    targets: &[Target<'a>],
    req: &SearchRequest,
) -> Result<SearchResult<'a>, QueryError> {
    let by_score = req.sort.is_empty() || req.sort.iter().any(|f| f.field == "_score");
    // (target position, hit, score, sort values)
    let mut hits = Vec::new();
    let mut aggs = vec![aggs::Stats::default(); req.aggs.len()];
    let mut profile = req.profile.then(Vec::new);
    let mut failures = Vec::new();
    for (pos, (name, index, filter)) in targets.iter().enumerate() {
        let query = with_filter(&req.query, *filter)?;
        let s = Searcher::new(index, &query);
        let mut collector = match aggs::Collector::new(&req.aggs, &s, name, req.profile) {
            Ok(c) => c,
            Err(e) => {
                failures.push((*name, e));
                continue;
            }
        };
        for (id, doc) in s.docs().filter(|(_, d)| index.searchable(d)) {
            let Some(score) = s.score(&query, id, &doc) else {
                continue;
            };
            collector.collect(id, &doc, score);
            let sort = s.sort_values(&req.sort, score, &doc);
            let matched_queries = s.matched_queries(id, &doc);
            let fields = aggs::hit_fields(&s, &req.fields, &doc, true);
            hits.push((
                pos,
                Hit {
                    index: name,
                    id,
                    doc,
                    score: Some(score as f32),
                    sort,
                    matched_queries,
                    fields,
                },
                score,
            ));
        }
        let (stats, p) = collector.finish(matches!(query, Query::MatchAll));
        aggs::reduce(&mut aggs, stats);
        if let (Some(all), Some(p)) = (&mut profile, p) {
            all.push(p);
        }
    }
    hits.sort_by(|(pa, a, sa), (pb, b, sb)| {
        // ponytail: ties fall back to index order then write order, standing in for shard/doc order
        cmp_sorted(&req.sort, (&a.sort, *sa), (&b.sort, *sb))
            .then(pa.cmp(pb))
            .then(a.doc.seq_no.cmp(&b.doc.seq_no))
    });
    if !failures.is_empty() && failures.len() == targets.len() {
        // ponytail: ES wraps this in an all-shards-failed search_phase_execution_exception
        return Err(failures.swap_remove(0).1);
    }
    // Elasticsearch collects no top docs for size 0, so it reports no max_score.
    let max_score = (by_score && req.size > 0)
        .then(|| hits.iter().map(|h| h.2 as f32).reduce(f32::max))
        .flatten();
    Ok(SearchResult {
        failures,
        aggregations: (!req.aggs.is_empty())
            .then(|| aggs::render(&req.aggs, &mut aggs, req.typed_keys))
            .transpose()?,
        profile,
        total: hits.len(),
        max_score,
        hits: hits
            .into_iter()
            .skip(req.from)
            .take(req.size)
            .map(|(_, mut h, _)| {
                if !by_score {
                    h.score = None;
                }
                h
            })
            .collect(),
    })
}

pub fn count(index: &Index, q: &Query) -> usize {
    count_many(&[("", index, None)], q).expect("no alias filter to parse")
}

/// Counts from postings and doc values where they answer the query exactly, on disk and in
/// memory separately; the documents of the other are read and matched.
pub fn count_many(targets: &[Target], q: &Query) -> Result<usize, QueryError> {
    let mut n = 0;
    for (_, index, filter) in targets {
        let q = with_filter(q, *filter)?;
        let mut s = Searcher::unscored(index, &q);
        let (disk, memory) = index.live();
        let on_disk = exact(&q, &disk, &|c| match c {
            Query::Term { .. } | Query::Match { .. } | Query::Range { .. } => {
                Some(s.candidates(c, true)?.into_iter().collect())
            }
            _ => None,
        });
        let in_memory = exact(&q, &memory, &|c| match c {
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
        Query::Named { query, .. } => exact(query, all, leaf),
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
    fn queries_on_an_opened_index_read_only_the_docs_their_postings_hold() {
        let dir = std::env::temp_dir().join(format!("esm-candidates-{}", std::process::id()));
        let mut ix = Index::default();
        let props = &mut ix.mappings.properties;
        props.insert("title".into(), FieldMapping::Text { store: false });
        props.insert("tag".into(), FieldMapping::Keyword { ignore_above: None });
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
        props.insert("tag".into(), FieldMapping::Keyword { ignore_above: None });
        props.insert("n".into(), FieldMapping::Long);
        props.insert("ok".into(), FieldMapping::Boolean);
        for i in 0..30 {
            let tag = if i % 3 == 0 { "three" } else { "other" };
            let doc = json!({"title": format!("doc {i}"), "tag": tag, "n": i, "ok": i % 2 == 0});
            ix.put(&i.to_string(), doc);
        }
        disk::save(&dir, &Indices::from([("i".to_owned(), ix)])).unwrap();
        let mut opened = disk::open(&dir).unwrap();
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
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
