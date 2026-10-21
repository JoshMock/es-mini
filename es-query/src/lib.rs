//! JSON query DSL and ES|QL parsing/execution.
//!
//! ponytail: queries scan every document and compute BM25 statistics per query; replace with
//! postings lookups once the Lucene codec (es-codec) provides an inverted index.

pub mod esql;
mod parse;

pub use parse::{Query, parse_query, parse_query_string};

use es_index::{Document, FieldMapping, Index, analysis::Analyzer, parse_date_millis};
use serde_json::Value;
use std::{borrow::Cow, cell::RefCell, cmp::Ordering, collections::HashMap};

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

/// A JSON scalar as the string ES would index (`5` → "5", `true` → "true").
fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        v => v.to_string(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Text,
    Keyword,
    Number,
    Bool,
}

/// The source path a field reads: unmapped `x.keyword` reads `x`.
fn source_path<'f>(index: &Index, field: &'f str) -> &'f str {
    match index.mappings.properties.contains_key(field) {
        true => field,
        false => field.strip_suffix(".keyword").unwrap_or(field),
    }
}

fn resolve(index: &Index, field: &str, sample: impl Fn(&str) -> Option<Value>) -> Kind {
    match index.mappings.properties.get(field) {
        Some(FieldMapping::Text | FieldMapping::TextKeyword { .. }) => Kind::Text,
        Some(FieldMapping::Keyword { .. }) => Kind::Keyword,
        Some(FieldMapping::Boolean) => Kind::Bool,
        // ponytail: dates match and sort as their JSON values (only range parses instants); no date math
        Some(FieldMapping::Date { .. }) => Kind::Keyword,
        Some(m) if m.is_numeric() => Kind::Number,
        Some(_) => unreachable!("every FieldMapping is text, keyword, boolean, date or numeric"),
        None if field.ends_with(".keyword") => Kind::Keyword,
        // ponytail: unmapped fields are typed from their first value, mimicking ES dynamic mapping
        None => match sample(field) {
            Some(Value::Number(_)) => Kind::Number,
            Some(Value::Bool(_)) => Kind::Bool,
            _ => Kind::Text,
        },
    }
}

/// Scalar values at a dotted path, with arrays flattened.
fn values<'a>(source: &'a Value, path: &str) -> Vec<&'a Value> {
    let mut out = Vec::new();
    collect(source, path, &mut out);
    out
}

fn collect<'a>(v: &'a Value, path: &str, out: &mut Vec<&'a Value>) {
    match v {
        Value::Array(a) => a.iter().for_each(|x| collect(x, path, out)),
        _ if path.is_empty() => {
            if !v.is_null() {
                out.push(v);
            }
        }
        Value::Object(o) => {
            if let Some(x) = o.get(path) {
                collect(x, "", out);
            }
            for (i, _) in path.match_indices('.') {
                if let Some(x) = o.get(&path[..i]) {
                    collect(x, &path[i + 1..], out);
                }
            }
        }
        _ => {}
    }
}

/// Indexed terms of a field in one document.
fn terms(kind: Kind, vals: &[&Value]) -> Vec<String> {
    match kind {
        Kind::Text => vals
            .iter()
            .flat_map(|v| Analyzer::Standard.analyze(&text(v)))
            .collect(),
        _ => vals.iter().map(|v| text(v)).collect(),
    }
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

/// Epoch seconds of `"E, d MMM yyyy HH:mm:ss Z"` with localized month names.
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
    // days from civil (Howard Hinnant)
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some((era * 146097 + doe - 719468) * 86400 + secs - off)
}

/// Exact integer value, so longs above 2^53 keep their precision.
fn int(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_str()?.parse().ok())
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

struct Searcher<'a> {
    index: &'a Index,
    /// Per field: (docs with the field, average length).
    field_stats: HashMap<String, (f64, f64)>,
    doc_freq: HashMap<(String, String), f64>,
    /// Resolved field types, so unmapped fields are sampled once per search.
    /// Per field: its kind, and whether a literal `x.keyword` source key overrides `x`.
    kinds: RefCell<HashMap<String, (Kind, bool)>>,
}

const K1: f64 = 1.2;
const B: f64 = 0.75;

impl<'a> Searcher<'a> {
    fn new(index: &'a Index, q: &Query) -> Self {
        let mut s = Searcher {
            index,
            field_stats: HashMap::new(),
            doc_freq: HashMap::new(),
            kinds: RefCell::default(),
        };
        s.gather(q);
        s
    }

    fn kind<'f>(&self, field: &'f str) -> (Kind, &'f str) {
        let path = |literal| match literal {
            true => field,
            false => source_path(self.index, field),
        };
        if let Some(&(k, literal)) = self.kinds.borrow().get(field) {
            return (k, path(literal));
        }
        // a literal `x.keyword` source key wins over the multi-field fallback
        let literal = source_path(self.index, field) != field
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
            _ => return,
        };
        let (kind, path) = self.kind(field);
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
        if let Some((docs, ttf)) = self.index.stats.as_ref().and_then(|s| s.fields.get(field)) {
            self.field_stats.insert(field.clone(), (*docs, ttf / docs));
            for w in &wanted {
                let df = self
                    .index
                    .stats
                    .as_ref()
                    .and_then(|s| s.doc_freq.get(&(field.clone(), w.clone())));
                self.doc_freq
                    .insert((field.clone(), w.clone()), df.copied().unwrap_or(0.0));
            }
        }
    }

    fn bm25(&self, field: &str, term: &str, freq: f64, len: f64) -> f64 {
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
        // ES's LegacyBM25Similarity scales Lucene's BM25 by (1 + k1).
        idf * (1.0 + K1) * freq / (freq + K1 * (1.0 - B + B * len / avg))
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
                let date = match self.index.mappings.properties.get(field) {
                    Some(FieldMapping::Date { format, locale }) => Some((format, locale)),
                    _ => None,
                };
                let instant = |v: &Value| match date {
                    Some((Some(_), locale)) => parse_dated(v, locale.as_deref().unwrap_or("en")),
                    _ => parse_date_millis(v),
                };
                let cmp = |a: &Value, b: &Value| match date {
                    None => scalar_cmp(a, b),
                    Some(_) => match (instant(a), instant(b)) {
                        (Some(x), Some(y)) => x.cmp(&y),
                        _ => Ordering::Less,
                    },
                };
                let scalar_cmp = cmp;
                let ok = |v: &Value| {
                    gt.as_ref().is_none_or(|b| scalar_cmp(v, b).is_gt())
                        && gte.as_ref().is_none_or(|b| scalar_cmp(v, b).is_ge())
                        && lt.as_ref().is_none_or(|b| scalar_cmp(v, b).is_lt())
                        && lte.as_ref().is_none_or(|b| scalar_cmp(v, b).is_le())
                };
                values(&d.source, path).into_iter().any(ok).then_some(1.0)
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
        }
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
}

/// Which Elasticsearch error type a query failure maps to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// `parsing_exception`
    Parsing,
    /// `illegal_argument_exception`
    IllegalArgument,
}

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
                !["query", "from", "size", "sort", "track_total_hits"].contains(&k.as_str())
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
        let sort = match body.get("sort") {
            None => vec![],
            Some(Value::Array(a)) => a.iter().map(parse_sort).collect::<Result<_, _>>()?,
            Some(s) => vec![parse_sort(s)?],
        };
        let track_total_hits = match body.get("track_total_hits") {
            None => None,
            Some(v) if v.is_boolean() || v.is_i64() => Some(v.to_string()),
            Some(_) => return Err("[track_total_hits] must be a boolean or a number".into()),
        };
        Ok(Self {
            query,
            track_total_hits,
            from: usize_of("from", 0)?,
            size: usize_of("size", 10)?,
            sort,
        })
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
}

#[derive(Debug)]
pub struct SearchResult<'a> {
    pub total: usize,
    pub max_score: Option<f32>,
    pub hits: Vec<Hit<'a>>,
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
            if let Some(FieldMapping::Text | FieldMapping::TextKeyword { .. }) =
                index.mappings.properties.get(&f.field)
            {
                return Err(QueryError::illegal(format!(
                    "Text fields are not optimised for operations that require per-document field data like aggregations and sorting, so these operations are disabled by default. Please use a keyword field instead. Alternatively, set fielddata=true on [{}] in order to load field data by uninverting the inverted index. Note that this can use significant memory.",
                    f.field
                )));
            }
        }
    }
    Ok(())
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
    for (pos, (name, index, filter)) in targets.iter().enumerate() {
        let query = with_filter(&req.query, *filter)?;
        let s = Searcher::new(index, &query);
        for (id, doc) in index.docs().filter(|(_, d)| index.searchable(d)) {
            let Some(score) = s.score(&query, id, &doc) else {
                continue;
            };
            let sort: Vec<Value> = req
                .sort
                .iter()
                .map(|f| match f.field.as_str() {
                    "_score" => Value::from(score as f32),
                    field => {
                        let (_, path) = s.kind(field);
                        let mut v = values(&doc.source, path);
                        v.sort_by(|a, b| scalar_cmp(a, b));
                        let pick = if f.desc { v.last() } else { v.first() };
                        pick.map_or_else(
                            || match &f.missing {
                                Some(m) if !matches!(m.as_str(), Some("_first" | "_last")) => {
                                    m.clone()
                                }
                                _ => Value::Null,
                            },
                            |v| (*v).clone(),
                        )
                    }
                })
                .collect();
            hits.push((
                pos,
                Hit {
                    index: name,
                    id,
                    doc,
                    score: Some(score as f32),
                    sort,
                },
                score,
            ));
        }
    }
    hits.sort_by(|(pa, a, sa), (pb, b, sb)| {
        let fields = if req.sort.is_empty() {
            sb.total_cmp(sa)
        } else {
            req.sort.iter().zip(a.sort.iter().zip(&b.sort)).fold(
                Ordering::Equal,
                |acc, (f, (x, y))| {
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
                },
            )
        };
        // ponytail: ties fall back to index order then write order, standing in for shard/doc order
        fields
            .then(pa.cmp(pb))
            .then(a.doc.seq_no.cmp(&b.doc.seq_no))
    });
    let max_score = by_score
        .then(|| hits.iter().map(|h| h.2 as f32).reduce(f32::max))
        .flatten();
    Ok(SearchResult {
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

pub fn count_many(targets: &[Target], q: &Query) -> Result<usize, QueryError> {
    let mut n = 0;
    for (_, index, filter) in targets {
        let q = with_filter(q, *filter)?;
        let s = Searcher::new(index, &q);
        n += index
            .docs()
            .filter(|(id, d)| s.score(&q, id, d).is_some())
            .count();
    }
    Ok(n)
}

/// Terms a match query looks up: analyzed for text fields, verbatim otherwise.
fn query_terms(kind: Kind, query: &str) -> Vec<String> {
    match kind {
        Kind::Text => Analyzer::Standard.analyze(query),
        _ => vec![query.to_owned()],
    }
}
