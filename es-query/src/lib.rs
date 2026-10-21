//! JSON query DSL and ES|QL parsing/execution.
//!
//! ponytail: queries scan every document and compute BM25 statistics per query; replace with
//! postings lookups once the Lucene codec (es-codec) provides an inverted index.

pub mod esql;

use es_index::{Document, FieldMapping, Index, analysis::Analyzer};
use serde_json::Value;
use std::{cell::RefCell, cmp::Ordering, collections::HashMap};

#[derive(Debug, Clone, PartialEq)]
pub enum Query {
    MatchAll,
    Match {
        field: String,
        query: String,
        and: bool,
    },
    Term {
        field: String,
        value: Value,
    },
    Prefix {
        field: String,
        prefix: String,
    },
    Range {
        field: String,
        gt: Option<Value>,
        gte: Option<Value>,
        lt: Option<Value>,
        lte: Option<Value>,
    },
    Bool {
        must: Vec<Query>,
        should: Vec<Query>,
        must_not: Vec<Query>,
        filter: Vec<Query>,
        min_should: usize,
    },
}

/// The single `{field: params}` entry of a leaf query.
fn single_field<'a>(kind: &str, v: &'a Value) -> Result<(&'a String, &'a Value), String> {
    match v
        .as_object()
        .filter(|m| m.len() == 1)
        .and_then(|m| m.iter().next())
    {
        Some(entry) => Ok(entry),
        None => Err(format!("[{kind}] query malformed, expected a single field")),
    }
}

const MATCH_KEYS: &[&str] = &[
    "query",
    "operator",
    "minimum_should_match",
    "analyzer",
    "fuzziness",
    "prefix_length",
    "max_expansions",
    "fuzzy_transpositions",
    "fuzzy_rewrite",
    "lenient",
    "zero_terms_query",
    "auto_generate_synonyms_phrase_query",
    "cutoff_frequency",
    "boost",
    "_name",
];

/// ES rejects parameters a query does not know: `[kind] query does not support [k]`.
fn check_keys(
    kind: &str,
    o: &serde_json::Map<String, Value>,
    known: &[&str],
) -> Result<(), String> {
    match o.keys().find(|k| !known.contains(&k.as_str())) {
        Some(k) => Err(format!("[{kind}] query does not support [{k}]")),
        None => Ok(()),
    }
}

/// Port of ES `Queries.calculateMinShouldMatch`: `3`, `-1`, `75%`, `-25%` or `2<-1 5<50%`.
fn min_should_match(optional: usize, spec: &str) -> Result<usize, String> {
    let n = optional as i64;
    let err = || format!("failed to parse minimum_should_match [{spec}]");
    let int = |s: &str| s.parse::<i32>().map(i64::from).map_err(|_| err());
    if spec.contains('<') {
        let spec = spec.split('<').map(str::trim).collect::<Vec<_>>().join("<");
        let mut result = optional;
        for cond in spec.split_whitespace() {
            let (upper, s) = cond.split_once('<').ok_or_else(err)?;
            if n <= int(upper)? {
                return Ok(result);
            }
            result = min_should_match(optional, s)?;
        }
        return Ok(result);
    }
    let spec = spec.trim();
    let result = match spec.strip_suffix('%') {
        Some(p) => {
            let calc = (n * int(p)?) as f32 * (1.0 / 100.0);
            if calc < 0.0 {
                n + calc as i64
            } else {
                calc as i64
            }
        }
        None => match int(spec)? {
            c if c < 0 => n + c,
            c => c,
        },
    };
    Ok(result.max(0) as usize)
}

fn clauses(v: Option<&Value>) -> Result<Vec<Query>, String> {
    match v {
        None => Ok(vec![]),
        Some(Value::Array(a)) => a.iter().map(parse_query).collect(),
        Some(q) => Ok(vec![parse_query(q)?]),
    }
}

pub fn parse_query(v: &Value) -> Result<Query, String> {
    let (kind, body) = single_field("query", v)
        .map_err(|_| "query malformed, expected a single query type".to_owned())?;
    match kind.as_str() {
        "match_all" => Ok(Query::MatchAll),
        "match" => {
            let (field, p) = single_field(kind, body)?;
            let (query, op) = match p {
                Value::Object(o) => {
                    check_keys("match", o, MATCH_KEYS)?;
                    (
                        o.get("query").ok_or("[match] requires query")?,
                        o.get("operator"),
                    )
                }
                q => (q, None),
            };
            let and = match op.and_then(Value::as_str) {
                None => false,
                Some(o) if o.eq_ignore_ascii_case("and") => true,
                Some(o) if o.eq_ignore_ascii_case("or") => false,
                Some(o) => {
                    return Err(format!(
                        "Unsupported defaultOperator [{o}], can either be 'and' or 'or'"
                    ));
                }
            };
            Ok(Query::Match {
                field: field.clone(),
                query: text(query),
                and,
            })
        }
        "term" => {
            let (field, p) = single_field(kind, body)?;
            let value = match p {
                Value::Object(o) => {
                    check_keys("term", o, &["value", "boost", "_name", "case_insensitive"])?;
                    o.get("value").ok_or("[term] requires value")?.clone()
                }
                v => v.clone(),
            };
            Ok(Query::Term {
                field: field.clone(),
                value,
            })
        }
        "range" => {
            let (field, p) = single_field(kind, body)?;
            let p = p.as_object().ok_or("[range] query malformed")?;
            check_keys(
                "range",
                p,
                &[
                    "gt",
                    "gte",
                    "lt",
                    "lte",
                    "format",
                    "boost",
                    "_name",
                    // ponytail: accepted but ignored; dates compare without a time zone
                    "time_zone",
                    "relation",
                ],
            )?;
            // ponytail: `uuuu` bounds become ISO strings (compared lexicographically); upper bounds
            // round up to the end of Jan 1 as ES does. Real date parsing would replace this.
            let year = p.get("format").and_then(Value::as_str) == Some("uuuu");
            let get = |k: &str| {
                let v = p.get(k)?;
                let Some(y) = v.as_i64().filter(|_| year) else {
                    return Some(v.clone());
                };
                let end = matches!(k, "gt" | "lte");
                let t = if end {
                    "23:59:59.999999999Z"
                } else {
                    "00:00:00"
                };
                Some(Value::String(format!("{y:04}-01-01T{t}")))
            };
            if p.get("format").and_then(Value::as_str) == Some("date_optional_time") {
                // ponytail: only bare-number years are checked; ES's max year is 292278993
                if let Some(n) = ["gt", "gte", "lt", "lte"]
                    .iter()
                    .filter_map(|k| p.get(*k)?.as_i64())
                    .find(|n| n.abs() > 292_278_993)
                {
                    return Err(format!(
                        "failed to parse date field [{n}] with format [date_optional_time]"
                    ));
                }
            }
            Ok(Query::Range {
                field: field.clone(),
                gt: get("gt"),
                gte: get("gte"),
                lt: get("lt"),
                lte: get("lte"),
            })
        }
        "bool" => {
            let o = body.as_object().ok_or("[bool] query malformed")?;
            check_keys(
                "bool",
                o,
                &[
                    "must",
                    "should",
                    "filter",
                    "must_not",
                    "minimum_should_match",
                    "boost",
                    "_name",
                    "adjust_pure_negative",
                ],
            )?;
            let must = clauses(body.get("must"))?;
            let should = clauses(body.get("should"))?;
            let filter = clauses(body.get("filter"))?;
            let default = if must.is_empty() && filter.is_empty() && !should.is_empty() {
                1
            } else {
                0
            };
            let min_should = match body.get("minimum_should_match") {
                None | Some(Value::Null) => 0,
                Some(Value::String(m)) => min_should_match(should.len(), m)?,
                Some(m) => min_should_match(should.len(), &m.to_string())?,
            };
            Ok(Query::Bool {
                must,
                should,
                must_not: clauses(body.get("must_not"))?,
                filter,
                // ES applies only a positive minimum_should_match.
                min_should: if min_should > 0 { min_should } else { default },
            })
        }
        other => Err(format!("unknown query [{other}]")),
    }
}

/// `q=` URI search: whitespace-separated `[field:]term` clauses, `term*` is a prefix.
// ponytail: no quoting, ranges, boolean operators or escapes
pub fn parse_query_string(q: &str, df: &str, and: bool) -> Query {
    let clauses: Vec<Query> = q
        .split_whitespace()
        .map(|t| {
            let (field, t) = t.split_once(':').unwrap_or((df, t));
            match t.strip_suffix('*') {
                Some(p) => Query::Prefix {
                    field: field.to_owned(),
                    prefix: p.to_lowercase(),
                },
                None => Query::Match {
                    field: field.to_owned(),
                    query: t.to_owned(),
                    and: false,
                },
            }
        })
        .collect();
    let (must, should) = if and {
        (clauses, vec![])
    } else {
        (vec![], clauses)
    };
    Query::Bool {
        min_should: usize::from(must.is_empty()),
        must,
        should,
        must_not: vec![],
        filter: vec![],
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
        // ponytail: dates compare as their JSON values (ISO strings sort correctly); no date math
        Some(FieldMapping::Date { .. }) => Kind::Keyword,
        Some(_) => Kind::Number,
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

fn scalar_cmp(a: &Value, b: &Value) -> Ordering {
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
    kinds: RefCell<HashMap<String, Kind>>,
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
        let path = source_path(self.index, field);
        if let Some(k) = self.kinds.borrow().get(field) {
            return (*k, path);
        }
        let sample = |f: &str| {
            self.index
                .docs()
                .find_map(|(_, d)| values(&d.source, f).first().map(|v| (*v).clone()))
        };
        let kind = resolve(self.index, field, sample);
        self.kinds.borrow_mut().insert(field.to_owned(), kind);
        (kind, path)
    }

    /// Precomputes BM25 statistics for every scored (field, term) in the query.
    fn gather(&mut self, q: &Query) {
        let (field, wanted) = match q {
            Query::Match { field, query, .. } => {
                let (kind, _) = self.kind(field);
                let t = match kind {
                    Kind::Text => Analyzer::Standard.analyze(query),
                    _ => vec![query.clone()],
                };
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
        let sources = (self.index.docs().map(|(_, d)| &d.source)).chain(self.index.superseded());
        for source in sources {
            let t = terms(kind, &values(source, path));
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

    // ponytail: exact field lengths; Lucene quantizes norms to one byte, so scores differ slightly
    fn bm25(&self, field: &str, term: &str, freq: f64, len: f64) -> f64 {
        let (n, avg) = self.field_stats.get(field).copied().unwrap_or((1.0, 1.0));
        let df = self
            .doc_freq
            .get(&(field.to_owned(), term.to_owned()))
            .copied()
            .unwrap_or(0.0);
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
                let wanted = match kind {
                    Kind::Text => Analyzer::Standard.analyze(query),
                    _ => vec![query.clone()],
                };
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
            Query::Range {
                field,
                gt,
                gte,
                lt,
                lte,
            } => {
                let (_, path) = self.kind(field);
                let locale = match self.index.mappings.properties.get(field) {
                    Some(FieldMapping::Date {
                        format: Some(_),
                        locale,
                    }) => Some(locale.as_deref().unwrap_or("en")),
                    _ => None,
                };
                let cmp = |a: &Value, b: &Value| match locale {
                    Some(l) => match (parse_dated(a, l), parse_dated(b, l)) {
                        (Some(x), Some(y)) => x.cmp(&y),
                        _ => Ordering::Less,
                    },
                    None => scalar_cmp(a, b),
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
            Kind::Number => num(&Value::String(w.clone()))
                .is_some_and(|x| vals.iter().any(|v| num(v) == Some(x))),
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

impl SearchRequest {
    pub fn parse(body: &Value) -> Result<Self, String> {
        if let Some(k) = body.as_object().and_then(|o| {
            o.keys().find(|k| {
                !["query", "from", "size", "sort", "track_total_hits"].contains(&k.as_str())
            })
        }) {
            return Err(format!("Unknown key for a START_OBJECT in [{k}]."));
        }
        let query = body.get("query").map_or(Ok(Query::MatchAll), parse_query)?;
        let usize_of = |k: &str, d| match body.get(k) {
            None => Ok(d),
            Some(v) => match v.as_i64() {
                Some(n) if n < 0 => Err(format!("[{k}] parameter cannot be negative, found [{n}]")),
                Some(n) => Ok(n as usize),
                None => Err(format!("[{k}] must be a number")),
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

fn parse_sort(v: &Value) -> Result<SortField, String> {
    let desc_of = |o: &Value| match o.as_str() {
        Some("desc") => Ok(true),
        Some("asc") => Ok(false),
        _ => Err(format!("invalid sort order [{o}]")),
    };
    match v {
        Value::String(f) => Ok(SortField {
            desc: f == "_score",
            field: f.clone(),
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
            })
        }
        _ => Err(format!("malformed sort [{v}]")),
    }
}

#[derive(Debug)]
pub struct Hit<'a> {
    /// The concrete index the hit came from.
    pub index: &'a str,
    pub id: &'a str,
    pub doc: &'a Document,
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
fn with_filter(q: &Query, filter: Option<&Value>) -> Result<Query, String> {
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

pub fn search<'a>(index: &'a Index, req: &SearchRequest) -> SearchResult<'a> {
    search_many(&[("", index, None)], req).expect("no alias filter to parse")
}

/// Searches several indices and merges the hits. Each index is scored with its own statistics,
/// like ES's default query_then_fetch over one shard per index.
pub fn search_many<'a>(
    targets: &[Target<'a>],
    req: &SearchRequest,
) -> Result<SearchResult<'a>, String> {
    let by_score = req.sort.is_empty() || req.sort.iter().any(|f| f.field == "_score");
    // (target position, hit, score, sort values)
    let mut hits = Vec::new();
    for (pos, (name, index, filter)) in targets.iter().enumerate() {
        let query = with_filter(&req.query, *filter)?;
        let s = Searcher::new(index, &query);
        for (id, doc) in index.docs().filter(|(_, d)| index.searchable(d)) {
            let Some(score) = s.score(&query, id, doc) else {
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
                        pick.map_or(Value::Null, |v| (*v).clone())
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
                    acc.then_with(|| match (x.is_null(), y.is_null()) {
                        // Missing values sort last in both directions.
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

pub fn count_many(targets: &[Target], q: &Query) -> Result<usize, String> {
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
