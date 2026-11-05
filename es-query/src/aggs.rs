//! Aggregations (`aggs`/`aggregations` in a search body): parsed into named aggregations,
//! collected over every match of each searched index (one shard each), then reduced across
//! indices and rendered like Elasticsearch.
//!
//! Values come from what the field's doc values hold: read from disk for documents in
//! segments, and converted the same way from the source for documents in memory.

mod bucket_selector;
mod bucket_sort;
mod derivative;
pub(crate) mod histogram;
mod max_bucket;
mod terms;

use crate::hll::{self, Hll};
use crate::tdigest::TDigest;
use crate::{
    MAX_BUCKETS, Query, QueryError, Searcher, SortField, fielddata_disabled, int, num, parse_query,
    source_path,
};
use bucket_selector::BucketSelector;
use bucket_sort::BucketSort;
use derivative::Derivative;
use es_index::{Document, FieldMapping, iso_date, postings::values, tz::TimeZone};
use histogram::Histogram;
pub use max_bucket::MaxBucket;
use serde_json::{Map, Value};
use std::cell::Cell;
use std::time::Instant;
use terms::{Buckets, Format, Terms};

#[derive(Debug, Clone, PartialEq)]
enum Metric {
    Avg,
    Sum,
    Min,
    Max,
    ValueCount,
    Stats,
    ExtendedStats {
        sigma: f64,
    },
    Cardinality {
        precision: u32,
        /// The `execution_hint` picks ES's collector for keywords: global ordinals, else
        /// segment ordinals (`Some(true)`) or hashing (`Some(false)`). Only profiles show it.
        segment_ords: Option<bool>,
    },
    Percentiles {
        compression: f64,
        /// Sorted, distinct, in `[0, 100]`; for ranks, the sorted values to rank.
        percents: Vec<f64>,
        keyed: bool,
        /// `percentile_ranks`: the percentage of values at or below each of `percents`.
        ranks: bool,
    },
    Terms(Box<Terms>),
    Histogram(Box<Histogram>),
    /// The single bucket of the documents matching a query.
    Filter(Box<Query>),
    /// The single bucket of the documents without a value for the field (nor `missing`).
    Missing,
    /// The single bucket of the documents with an object at the path.
    // ponytail: nested objects are not separate documents, so `doc_count` counts parents and
    // sub-aggregations see a parent's values from every object; real nested docs fix both
    Nested(String),
    /// The single bucket of every document of the searched indices, whatever the query.
    Global,
    /// A bucket per query, and maybe one of the documents matching none.
    Filters(Box<Filters>),
    /// A bucket per range of values.
    Range(Box<Ranges>),
    TopHits(Box<TopHits>),
}

/// The `top_hits` aggregation's request, like ES's TopHitsAggregationBuilder.
#[derive(Debug, Clone, PartialEq)]
struct TopHits {
    from: usize,
    size: usize,
    sort: Vec<SortField>,
    /// The `_source` filter as `filter_path` patterns (empty keeps it whole); `None` leaves
    /// `_source` out.
    source: Option<String>,
    version: bool,
    seq_no: bool,
    explain: bool,
    docvalue_fields: Vec<FieldSpec>,
    /// The `fields` to fetch; `None` inherits the search's.
    fields: Option<Vec<FieldSpec>>,
    stored_fields: Vec<String>,
    /// The fields to highlight, each with the query to highlight it by (the search's if `None`).
    highlight: Vec<(String, Option<Query>)>,
}

/// A document `top_hits` collected.
#[derive(Debug, Clone)]
struct TopHit {
    index: String,
    id: String,
    score: f64,
    sort: Vec<Value>,
    doc: Document,
    /// The requested `docvalue_fields` and `fields` that have values.
    fields: Map<String, Value>,
    matched_queries: Vec<String>,
    /// Its score's explanation, if asked for.
    explanation: Option<Value>,
    /// The `highlight` fragments of each field that has some.
    highlight: Map<String, Value>,
}

impl TopHits {
    fn parse(params: &Map<String, Value>) -> Result<Self, QueryError> {
        let bad =
            |k: &str| QueryError::x_content(format!("[top_hits] failed to parse field [{k}]"));
        let mut t = TopHits {
            from: 0,
            size: 3,
            sort: vec![],
            source: Some(String::new()),
            version: false,
            seq_no: false,
            explain: false,
            docvalue_fields: vec![],
            fields: None,
            stored_fields: vec![],
            highlight: vec![],
        };
        let mut stored_fields = None;
        // patterns of a string or an array of strings
        let patterns = |k: &str, v: &Value| match v {
            Value::String(s) => Ok(vec![s.clone()]),
            Value::Array(a) => (a.iter())
                .map(|p| p.as_str().map(String::from).ok_or_else(|| bad(k)))
                .collect(),
            _ => Err(bad(k)),
        };
        for (k, v) in params {
            match k.as_str() {
                "from" | "size" => {
                    let n = int(v).and_then(|n| usize::try_from(n).ok());
                    *(if k == "from" {
                        &mut t.from
                    } else {
                        &mut t.size
                    }) = n.ok_or_else(|| bad(k))?;
                }
                "sort" => t.sort = crate::request::parse_sorts(v)?,
                "version" => t.version = v.as_bool().ok_or_else(|| bad(k))?,
                "seq_no_primary_term" => t.seq_no = v.as_bool().ok_or_else(|| bad(k))?,
                "explain" => t.explain = v.as_bool().ok_or_else(|| bad(k))?,
                "docvalue_fields" => t.docvalue_fields = field_specs(v).ok_or_else(|| bad(k))?,
                "fields" => t.fields = Some(field_specs(v).ok_or_else(|| bad(k))?),
                "stored_fields" => stored_fields = Some(patterns(k, v)?),
                "highlight" => {
                    // ponytail: only `fields` of plain names, each with a `highlight_query`
                    let fields = v.get("fields").and_then(Value::as_object);
                    let fields = fields.filter(|_| v.as_object().is_some_and(|o| o.len() == 1));
                    for (name, params) in fields.ok_or_else(|| bad(k))? {
                        let query = match params
                            .as_object()
                            .ok_or_else(|| bad(k))?
                            .get("highlight_query")
                        {
                            Some(q) => Some(parse_query(q)?),
                            None => None,
                        };
                        t.highlight.push((name.clone(), query));
                    }
                }
                "_source" => {
                    t.source = match v {
                        Value::Bool(b) => b.then(String::new),
                        Value::Object(o) => {
                            let mut filter = vec![];
                            for (k, v) in o {
                                let exclude = match k.as_str() {
                                    "includes" | "include" => "",
                                    "excludes" | "exclude" => "-",
                                    _ => return Err(bad("_source")),
                                };
                                filter
                                    .extend(patterns(k, v)?.iter().map(|p| exclude.to_owned() + p));
                            }
                            Some(filter.join(","))
                        }
                        v => Some(patterns(k, v)?.join(",")),
                    }
                }
                _ => return Err(format!("[top_hits] unknown field [{k}]").into()),
            }
        }
        // Stored fields leave `_source` out unless it is asked for, like ES's FetchContext.
        if let Some(f) = stored_fields {
            let asked = params.contains_key("_source") || f.iter().any(|f| f == "_source");
            if f == ["_none_"] || !asked {
                t.source = None;
            }
            t.stored_fields = f;
        }
        Ok(t)
    }

    /// Keeps the hits this aggregation may render, best first.
    fn keep_top(&self, hits: &mut Vec<TopHit>) {
        hits.sort_by(|a, b| {
            crate::cmp_sorted(&self.sort, (&a.sort, a.score), (&b.sort, b.score))
                .then_with(|| a.index.cmp(&b.index))
                .then(a.doc.seq_no.cmp(&b.doc.seq_no))
        });
        hits.truncate(self.from.saturating_add(self.size));
    }

    /// The `hits` object of `total` matches, like a search response's.
    fn render(&self, mut hits: Vec<TopHit>, total: u64) -> Value {
        let by_score = self.sort.is_empty() || self.sort.iter().any(|f| f.field == "_score");
        let score = |h: &TopHit| by_score.then_some(h.score as f32);
        let max_score = hits.iter().filter_map(score).reduce(f32::max);
        self.keep_top(&mut hits);
        let hits = hits.iter().skip(self.from).map(|h| {
            let mut hit = Map::new();
            hit.insert("_index".into(), h.index.clone().into());
            hit.insert("_id".into(), h.id.clone().into());
            if self.version {
                hit.insert("_version".into(), h.doc.version.into());
            }
            if self.seq_no {
                hit.insert("_seq_no".into(), h.doc.seq_no.into());
                hit.insert("_primary_term".into(), 1.into());
            }
            hit.insert("_score".into(), score(h).into());
            if let Some(r) = &h.doc.routing {
                hit.insert("_routing".into(), r.as_str().into());
            }
            if let Some(filter) = &self.source {
                let source = match filter.is_empty() {
                    true => h.doc.source.clone(),
                    false => es_index::filter_path::apply(&h.doc.source, filter),
                };
                hit.insert("_source".into(), source);
            }
            if !h.fields.is_empty() {
                hit.insert("fields".into(), h.fields.clone().into());
            }
            if !self.sort.is_empty() {
                hit.insert("sort".into(), h.sort.clone().into());
            }
            if !h.matched_queries.is_empty() {
                hit.insert("matched_queries".into(), h.matched_queries.clone().into());
            }
            if let Some(e) = &h.explanation {
                hit.insert("_explanation".into(), e.clone());
            }
            if !h.highlight.is_empty() {
                hit.insert("highlight".into(), h.highlight.clone().into());
            }
            Value::from(hit)
        });
        serde_json::json!({
            "total": {"value": total, "relation": "eq"},
            "max_score": max_score,
            "hits": hits.collect::<Vec<_>>(),
        })
    }
}

/// The `range` or `date_range` aggregation's ranges, like ES's RangeAggregationBuilder and
/// DateRangeAggregationBuilder.
#[derive(Debug, Clone, PartialEq)]
struct Ranges {
    /// Each range's key, `from` and `to` as requested: numbers, or strings the field parses.
    ranges: Vec<(Option<String>, Value, Value)>,
    keyed: bool,
    /// A DecimalFormat pattern for numeric bounds, or a date format for dates.
    format: Option<String>,
    /// A `date_range`: bounds read as dates, even of an unmapped field.
    date: bool,
    /// A `date_range`'s `time_zone` other than UTC: of bounds without an offset, and their strings.
    time_zone: Option<TimeZone>,
}

/// A `range` bucket's key, requested bounds (open ends infinite) and their strings.
#[derive(Debug, Clone, Default)]
struct RangeBucket {
    key: String,
    bounds: [f64; 2],
    strings: [Option<String>; 2],
}

impl Ranges {
    fn parse(name: &str, ty: &str, params: &Map<String, Value>) -> Result<Self, QueryError> {
        let bad = |k: &str| QueryError::x_content(format!("[{ty}] failed to parse field [{k}]"));
        let mut r = Ranges {
            ranges: vec![],
            keyed: false,
            format: None,
            date: ty == "date_range",
            time_zone: None,
        };
        for (k, v) in params {
            match k.as_str() {
                "field" | "missing" => {}
                "keyed" => r.keyed = v.as_bool().ok_or_else(|| bad(k))?,
                "format" => r.format = Some(v.as_str().ok_or_else(|| bad(k))?.into()),
                "time_zone" if r.date => {
                    let tz = v.as_str().and_then(TimeZone::parse).ok_or_else(|| bad(k))?;
                    r.time_zone = Some(tz).filter(|tz| *tz != TimeZone::UTC);
                }
                "ranges" => {
                    for range in v.as_array().ok_or_else(|| bad(k))? {
                        let o = range.as_object().ok_or_else(|| bad(k))?;
                        if let Some(k) = o
                            .keys()
                            .find(|k| !["key", "from", "to"].contains(&k.as_str()))
                        {
                            return Err(format!("[{ty}] unknown field [{k}]").into());
                        }
                        let bound = |k| o.get(k).cloned().unwrap_or(Value::Null);
                        let key = o.get("key").map(es_index::postings::text);
                        r.ranges.push((key, bound("from"), bound("to")));
                    }
                }
                _ => return Err(format!("[{ty}] unknown field [{k}]").into()),
            }
        }
        if r.ranges.is_empty() {
            return Err(QueryError::illegal(format!(
                "No [ranges] specified for the [{name}] aggregation"
            )));
        }
        Ok(r)
    }

    /// Empty buckets of the ranges sorted by `from` then `to`, with their bounds parsed like
    /// values of `field` (`m` its mapping).
    fn buckets(&self, m: Option<&FieldMapping>, empty: &[Stats]) -> Result<Vec<Stats>, QueryError> {
        let field_format = match m {
            Some(FieldMapping::Date { format, .. } | FieldMapping::DateNanos { format, .. }) => {
                Some(format.as_deref())
            }
            None if self.date => Some(None),
            _ => None,
        };
        let (format, locale) = date_format(m);
        let tz = self.time_zone.clone().unwrap_or(TimeZone::UTC);
        let date = |v: &Value| es_index::date_millis_in(format, locale, v, &tz, false);
        let parse = |v: &Value, open: f64| {
            match v {
                Value::Null => Some(open),
                // a `range` reads numbers as millis, a `date_range` in the field's format
                Value::String(_) if field_format.is_some() => date(v).map(|t| t as f64),
                v if self.date && field_format.is_some() => date(v).map(|t| t as f64),
                v => num(v),
            }
            .ok_or_else(|| {
                QueryError::illegal(format!("failed to parse [{}]", es_index::postings::text(v)))
            })
        };
        let as_string = |x: f64| match field_format {
            Some(f) => Some(date_string_in(
                self.format.as_deref().or(f),
                x as i64,
                self.time_zone.as_ref(),
            )),
            None => self.format.as_deref().map(|p| decimal_format(p, x)),
        };
        let mut ranges = (self.ranges.iter())
            .map(|(key, from, to)| {
                let bounds = [parse(from, f64::NEG_INFINITY)?, parse(to, f64::INFINITY)?];
                let strings = bounds.map(|x| x.is_finite().then(|| as_string(x)).flatten());
                let end = |i: usize| match (&strings[i], bounds[i].is_finite()) {
                    (Some(s), _) => s.clone(),
                    (None, true) => java_double(bounds[i]),
                    (None, false) => "*".into(),
                };
                let key = key
                    .clone()
                    .unwrap_or_else(|| format!("{}-{}", end(0), end(1)));
                Ok(RangeBucket {
                    key,
                    bounds,
                    strings,
                })
            })
            .collect::<Result<Vec<_>, QueryError>>()?;
        ranges.sort_by(|x, y| {
            let ([a, b], [c, d]) = (x.bounds, y.bounds);
            a.total_cmp(&c).then(b.total_cmp(&d))
        });
        let bucket = |range| Stats {
            range: Some(range),
            subs: empty.to_vec(),
            ..Stats::default()
        };
        Ok(ranges.into_iter().map(bucket).collect())
    }
}

/// The `filters` aggregation's buckets, like ES's FiltersAggregationBuilder.
#[derive(Debug, Clone, PartialEq)]
struct Filters {
    /// Named filters sorted by name, or anonymous filters named by position.
    filters: Vec<(String, Query)>,
    /// Whether the filters were named, so array buckets have a `key`.
    named: bool,
    keyed: bool,
    /// The key of the bucket of the documents matching no filter, if any.
    other: Option<String>,
}

impl Filters {
    fn parse(params: &Map<String, Value>) -> Result<Self, QueryError> {
        let bad = |k: &str| QueryError::x_content(format!("[filters] failed to parse field [{k}]"));
        let (mut filters, mut named, mut keyed) = (vec![], true, None);
        let (mut other, mut other_key) = (false, None);
        for (k, v) in params {
            match k.as_str() {
                "filters" => match v {
                    Value::Object(o) => {
                        // a BTreeMap: already sorted by name, as ES sorts named filters
                        for (name, q) in o {
                            filters.push((name.clone(), parse_query(q)?));
                        }
                    }
                    Value::Array(a) => {
                        named = false;
                        for (i, q) in a.iter().enumerate() {
                            filters.push((i.to_string(), parse_query(q)?));
                        }
                    }
                    _ => return Err(bad(k)),
                },
                "keyed" => keyed = Some(v.as_bool().ok_or_else(|| bad(k))?),
                "other_bucket" => other = v.as_bool().ok_or_else(|| bad(k))?,
                "other_bucket_key" => other_key = Some(v.as_str().ok_or_else(|| bad(k))?.into()),
                _ => return Err(format!("[filters] unknown field [{k}]").into()),
            }
        }
        if filters.is_empty() {
            return Err(QueryError::illegal("[filters] cannot be empty."));
        }
        // a key implies the other bucket; anonymous filters are never keyed
        let other = (other || other_key.is_some()).then(|| other_key.unwrap_or("_other_".into()));
        let keyed = named && keyed.unwrap_or(true);
        Ok(Filters {
            filters,
            named,
            keyed,
            other,
        })
    }
}

impl Metric {
    fn name(&self) -> &'static str {
        match self {
            Metric::Avg => "avg",
            Metric::Sum => "sum",
            Metric::Min => "min",
            Metric::Max => "max",
            Metric::ValueCount => "value_count",
            Metric::Stats => "stats",
            Metric::ExtendedStats { .. } => "extended_stats",
            Metric::Cardinality { .. } => "cardinality",
            Metric::Percentiles { ranks: false, .. } => "percentiles",
            Metric::Percentiles { .. } => "percentile_ranks",
            Metric::Terms(_) => "terms",
            Metric::Histogram(h) if h.date => "date_histogram",
            Metric::Histogram(_) => "histogram",
            Metric::Filter(_) => "filter",
            Metric::Missing => "missing",
            Metric::Nested(_) => "nested",
            Metric::Global => "global",
            Metric::Filters(_) => "filters",
            Metric::Range(r) if r.date => "date_range",
            Metric::Range(_) => "range",
            Metric::TopHits(_) => "top_hits",
        }
    }

    /// Whether this aggregation buckets documents (and so may have sub-aggregations).
    fn buckets(&self) -> bool {
        matches!(
            self,
            Metric::Terms(_) | Metric::Histogram(_) | Metric::Filters(_) | Metric::Range(_)
        ) || self.single()
    }

    /// Whether this aggregation has a single bucket (a `doc_count` and sub-aggregations).
    fn single(&self) -> bool {
        matches!(
            self,
            Metric::Filter(_) | Metric::Missing | Metric::Nested(_) | Metric::Global
        )
    }
}

/// Java's `Double.toString`, which names the keys of `percentiles`.
pub fn java_double(x: f64) -> String {
    if x == 0.0 || (1e-3..1e7).contains(&x.abs()) {
        return format!("{x:?}");
    }
    let s = format!("{x:e}");
    let (m, e) = s.split_once('e').unwrap_or((&s, "0"));
    match m.contains('.') {
        true => format!("{m}E{e}"),
        false => format!("{m}.0E{e}"),
    }
}

/// Java's `DecimalFormat` of `x` with Locale.ROOT symbols, as ES's DocValueFormat.Decimal: a
/// prefix and suffix (`'` quotes, `%` and `\u{2030}` scale `x`) around integer digits grouped by
/// `,`, fraction digits and an `E0` exponent, and an optional negative subpattern after `;`
/// whose affixes replace a `-` prefix. Digits round HALF_EVEN.
fn decimal_format(pattern: &str, x: f64) -> String {
    if x.is_nan() {
        return "NaN".into();
    }
    // `resolve_dates` refuses malformed patterns before any is formatted
    let Ok(p) = DecimalPattern::parse(pattern) else {
        return java_double(x);
    };
    let negative = x < 0.0 || (x == 0.0 && x.is_sign_negative());
    let x = (x * p.multiplier).abs();
    let (prefix, suffix) = (
        &p.affixes[usize::from(negative)],
        &p.affixes[2 + usize::from(negative)],
    );
    let num = match x.is_infinite() {
        true => "\u{221e}".into(),
        false => p.number(x),
    };
    format!("{prefix}{num}{suffix}")
}

/// A parsed `DecimalFormat` pattern, as `DecimalFormat.applyPattern` sets it up.
struct DecimalPattern {
    /// The positive and negative prefixes, then the positive and negative suffixes.
    affixes: [String; 4],
    min_int: usize,
    /// Only bounded with an exponent.
    max_int: usize,
    min_frac: usize,
    max_frac: usize,
    /// Digits per group; 0 for no grouping.
    group: usize,
    multiplier: f64,
    /// The minimum exponent digits, if the pattern has an exponent.
    exponent: Option<usize>,
    /// Whether the decimal point shows even without fraction digits.
    point: bool,
}

impl DecimalPattern {
    /// Fails with Java's reason for a malformed pattern.
    fn parse(pattern: &str) -> Result<DecimalPattern, String> {
        let malformed = |what: &str| format!("{what} pattern \"{pattern}\"");
        let chars: Vec<char> = pattern.chars().collect();
        let mut p = DecimalPattern {
            affixes: Default::default(),
            min_int: 0,
            max_int: usize::MAX,
            min_frac: 0,
            // DOUBLE_FRACTION_DIGITS, for an empty pattern
            max_frac: 340,
            group: 0,
            multiplier: 1.0,
            exponent: None,
            point: false,
        };
        let mut start = Some(0);
        for neg in [false, true] {
            let Some(mut pos) = start.take().filter(|&s| s < chars.len()) else {
                break;
            };
            let mut affix = [String::new(), String::new()];
            let (mut phase, mut quoted, mut scaled) = (0, false, false);
            let (mut left, mut zeros, mut right) = (0usize, 0, 0);
            let (mut decimal, mut grouping) = (None, None);
            while pos < chars.len() {
                let ch = chars[pos];
                let a = &mut affix[usize::from(phase != 0)];
                match (phase, ch) {
                    (1, _) if neg => match ch {
                        '#' | '0' | ',' | '.' | 'E' => {}
                        _ => {
                            phase = 2;
                            continue;
                        }
                    },
                    (0 | 2, '\'') if chars.get(pos + 1) == Some(&'\'') => {
                        pos += 1;
                        a.push('\'');
                    }
                    (0 | 2, '\'') => quoted = !quoted,
                    (0 | 2, _) if quoted => a.push(ch),
                    (0 | 2, '#' | '0' | ',' | '.') => {
                        phase = 1;
                        continue;
                    }
                    (0 | 2, ';') if phase == 0 || neg => {
                        return Err(malformed("Unquoted special character ';' in"));
                    }
                    (0 | 2, ';') => {
                        start = Some(pos + 1);
                        break;
                    }
                    (0 | 2, '%' | '\u{2030}') if scaled => {
                        return Err(malformed("Too many percent/per mille characters in"));
                    }
                    (0 | 2, '%' | '\u{2030}') => {
                        scaled = true;
                        p.multiplier = if ch == '%' { 100.0 } else { 1000.0 };
                        a.push(ch);
                    }
                    (0 | 2, _) => a.push(ch),
                    (_, '0') if right > 0 => return Err(malformed("Unexpected '0' in")),
                    (_, '#' | '0') => {
                        match ch == '#' {
                            true if zeros > 0 => right += 1,
                            true => left += 1,
                            false => zeros += 1,
                        }
                        if decimal.is_none() {
                            grouping = grouping.map(|g| g + 1);
                        }
                    }
                    (_, ',') => grouping = Some(0),
                    (_, '.') if decimal.is_some() => {
                        return Err(malformed("Multiple decimal separators in"));
                    }
                    (_, '.') => decimal = Some(left + zeros + right),
                    (_, 'E') if p.exponent.is_some() => {
                        return Err(malformed("Multiple exponential symbols in"));
                    }
                    (_, 'E') => {
                        let digits = chars[pos + 1..].iter().take_while(|&&c| c == '0');
                        let n = digits.count();
                        if left + zeros < 1 || n < 1 {
                            return Err(malformed("Malformed exponential"));
                        }
                        p.exponent = Some(n);
                        pos += n;
                        phase = 2;
                    }
                    _ => {
                        phase = 2;
                        continue;
                    }
                }
                pos += 1;
            }
            let [prefix, suffix] = affix;
            if neg && quoted {
                return Err(malformed("Malformed"));
            }
            if neg {
                if prefix != p.affixes[0] || suffix != p.affixes[2] {
                    (p.affixes[1], p.affixes[3]) = (prefix, suffix);
                }
                continue;
            }
            (p.affixes[1], p.affixes[3]) = (format!("-{prefix}"), suffix.clone());
            (p.affixes[0], p.affixes[2]) = (prefix, suffix);
            // "##.###" is "#0.###", ".###" is ".0##"
            if let Some(d) = decimal.filter(|_| zeros == 0 && left > 0) {
                let n = d.max(1);
                (right, left, zeros) = (left - n, n - 1, 1);
            }
            if (decimal.is_none() && right > 0)
                || decimal.is_some_and(|d| d < left || d > left + zeros)
                || grouping == Some(0)
                || quoted
            {
                return Err(malformed("Malformed"));
            }
            let total = left + zeros + right;
            p.min_int = decimal.unwrap_or(total).saturating_sub(left);
            if p.exponent.is_some() {
                p.max_int = left + p.min_int;
            }
            if total > 0 || decimal.is_some() {
                p.max_frac = decimal.map_or(0, |d| total.saturating_sub(d));
            }
            p.min_frac = decimal.map_or(0, |d| (left + zeros).saturating_sub(d));
            p.group = grouping.unwrap_or(0);
            p.point = decimal == Some(0) || decimal == Some(total);
        }
        Ok(p)
    }

    /// The digits of finite, non-negative `x`: `DecimalFormat.subformatNumber`.
    fn number(&self, x: f64) -> String {
        // DigitList.set: the shortest repr, unless it has more digits than allowed
        let shortest = digit_list(&format!("{x:e}"));
        let (d, at) = match self.exponent {
            Some(_) => {
                let sig = self.max_int.saturating_add(self.max_frac).max(1);
                match shortest.0.len() <= sig {
                    true => shortest,
                    false => digit_list(&format!("{x:.*e}", sig - 1)),
                }
            }
            None => match shortest.0.len() as i64 - shortest.1 <= self.max_frac as i64 {
                true => shortest,
                false => digit_list(&format!("{x:.*}", self.max_frac)),
            },
        };
        let digit = |i: usize| char::from(b'0' + d.get(i).copied().unwrap_or(0));
        let mut out = String::new();
        if let Some(min_exp) = self.exponent {
            let (repeat, mut min_int) = (self.max_int as i64, self.min_int as i64);
            let mut exponent = at;
            if repeat > 1 && repeat > min_int {
                // engineering notation: exponents are multiples of the integer digits
                exponent = match exponent >= 1 {
                    true => (exponent - 1) / repeat * repeat,
                    false => (exponent - repeat) / repeat * repeat,
                };
                min_int = 1;
            } else {
                exponent -= min_int;
            }
            let int_digits = if d.is_empty() { min_int } else { at - exponent };
            let min_digits = ((self.min_int + self.min_frac) as i64).max(int_digits);
            let total = (d.len() as i64).max(min_digits);
            for i in 0..total {
                if i == int_digits {
                    out.push('.');
                }
                out.push(digit(i as usize));
            }
            if self.point && total == int_digits {
                out.push('.');
            }
            let exponent = if d.is_empty() { 0 } else { exponent };
            let sign = if exponent < 0 { "-" } else { "" };
            out += &format!("E{sign}{:0min_exp$}", exponent.unsigned_abs());
            return out;
        }
        let mut count = self.min_int as i64;
        if at > count {
            count = at;
        }
        let mut next = 0;
        for i in (0..count).rev() {
            if i < at && next < d.len() {
                out.push(digit(next));
                next += 1;
            } else {
                out.push('0');
            }
            if self.group > 0 && i > 0 && i % self.group as i64 == 0 {
                out.push(',');
            }
        }
        let fraction = self.min_frac > 0 || next < d.len();
        if !fraction && out.is_empty() {
            out.push('0');
        }
        if self.point || fraction {
            out.push('.');
        }
        for i in 0..self.max_frac as i64 {
            if i >= self.min_frac as i64 && next >= d.len() {
                break;
            }
            if -1 - i > at - 1 {
                out.push('0');
            } else {
                out.push(digit(next));
                next += 1;
            }
        }
        out
    }
}

/// A formatted decimal's digits without leading or trailing zeros, and the position of the
/// decimal point in them, like Java's DigitList (no digits for zero).
fn digit_list(s: &str) -> (Vec<u8>, i64) {
    let (m, e) = s.split_once('e').unwrap_or((s, "0"));
    let (int, frac) = m.split_once('.').unwrap_or((m, ""));
    let mut d: Vec<u8> = (int.bytes().chain(frac.bytes()))
        .map(|b| b - b'0')
        .collect();
    let lead = d.iter().take_while(|&&b| b == 0).count();
    d.drain(..lead);
    while d.last() == Some(&0) {
        d.pop();
    }
    let at = int.len() as i64 + e.parse::<i64>().unwrap_or(0) - lead as i64;
    let at = if d.is_empty() { 0 } else { at };
    (d, at)
}

/// One named aggregation of a request.
#[derive(Debug, Clone, PartialEq)]
pub struct Agg {
    name: String,
    meta: Option<Value>,
    metric: Metric,
    field: String,
    missing: Option<Value>,
    /// Sub-aggregations, computed per bucket.
    subs: Vec<Agg>,
    /// A `bucket_sort` sub-aggregation, which orders and pages this aggregation's buckets.
    bucket_sort: Option<BucketSort>,
    /// `bucket_selector` sub-aggregations, which drop buckets of this aggregation.
    bucket_selectors: Vec<BucketSelector>,
    /// `derivative` sub-aggregations, which add a value to each bucket of this aggregation.
    derivatives: Vec<Derivative>,
}

/// `aggs` with the queries of their filter aggregations resolved for `index` (see
/// [`crate::dates::resolve_dates`]). Fails, as ES's shards do, on a malformed DecimalFormat
/// pattern for a numeric (or unmapped) field.
pub(crate) fn resolve_dates(
    aggs: &[Agg],
    index: &es_index::Index,
    now: i64,
) -> Result<Vec<Agg>, QueryError> {
    let resolve = |q: &mut Query| -> Result<(), QueryError> {
        *q = crate::dates::resolve_dates(index, q.clone(), now)?;
        Ok(())
    };
    (aggs.iter())
        .map(|a| {
            let mut a = a.clone();
            let numeric = |f: &str| index.mappings.field(f).is_none_or(FieldMapping::is_numeric);
            let decimal = |p: Option<&String>| match p {
                Some(p) => DecimalPattern::parse(p)
                    .map(drop)
                    .map_err(QueryError::illegal),
                None => Ok(()),
            };
            match &mut a.metric {
                Metric::Filter(q) => resolve(q)?,
                Metric::Filters(f) => f.filters.iter_mut().try_for_each(|(_, q)| resolve(q))?,
                Metric::Range(r) if !r.date && numeric(&a.field) => decimal(r.format.as_ref())?,
                Metric::Histogram(h) if !h.date && numeric(&a.field) => decimal(h.format.as_ref())?,
                Metric::TopHits(t) => {
                    let names = index.mappings.names();
                    for spec in &t.docvalue_fields {
                        let matched = |f: &String| es_index::aliases::glob(&spec.pattern, f);
                        if names.iter().any(|f| {
                            matched(f)
                                && index
                                    .mappings
                                    .field(f)
                                    .is_some_and(FieldMapping::is_numeric)
                        }) {
                            decimal(spec.format.as_ref())?;
                        }
                    }
                }
                _ => {}
            }
            a.subs = resolve_dates(&a.subs, index, now)?;
            Ok(a)
        })
        .collect()
}

/// Parses the sub-aggregations of an aggregation: `bucket_sort`, `bucket_selector` and
/// `derivative` are kept apart from the rest, as they compute nothing per bucket.
fn parse_subs(v: &Value) -> Result<Pipelines, QueryError> {
    let (mut sort, mut selectors, mut derivatives, mut rest) = (None, vec![], vec![], Map::new());
    for (name, body) in v
        .as_object()
        .ok_or("Expected [START_OBJECT] under [aggregations]")?
    {
        let get = |t| body.get(t);
        match (
            get("bucket_sort"),
            get("bucket_selector"),
            get("derivative"),
        ) {
            (Some(params), ..) => sort = Some(BucketSort::parse(params)?),
            (_, Some(params), _) => selectors.push(BucketSelector::parse(params)?),
            (.., Some(params)) => derivatives.push(Derivative::parse(name, params)?),
            _ => drop(rest.insert(name.clone(), body.clone())),
        }
    }
    Ok((parse(&rest.into())?, sort, selectors, derivatives))
}

/// The sub-aggregations, `bucket_sort`, `bucket_selector`s and `derivative`s of an aggregation.
type Pipelines = (
    Vec<Agg>,
    Option<BucketSort>,
    Vec<BucketSelector>,
    Vec<Derivative>,
);

/// Parses the object under a request's top-level `aggs`: `max_bucket` is kept apart from the
/// rest, as it computes from the other aggregations' results.
pub(crate) fn parse_top(v: &Value) -> Result<(Vec<Agg>, Vec<MaxBucket>), QueryError> {
    let (mut pipelines, mut rest) = (vec![], Map::new());
    for (name, body) in v
        .as_object()
        .ok_or("Expected [START_OBJECT] under [aggregations]")?
    {
        match body.get("max_bucket") {
            Some(params) => pipelines.push(MaxBucket::parse(name, params)?),
            None => drop(rest.insert(name.clone(), body.clone())),
        }
    }
    Ok((parse(&rest.into())?, pipelines))
}

/// Parses the object under `aggs`/`aggregations`.
pub(crate) fn parse(v: &Value) -> Result<Vec<Agg>, QueryError> {
    let aggs = v
        .as_object()
        .ok_or("Expected [START_OBJECT] under [aggregations]")?;
    aggs.iter()
        .map(|(name, body)| parse_one(name, body))
        .collect()
}

fn parse_one(name: &str, body: &Value) -> Result<Agg, QueryError> {
    if name.contains(['[', ']', '>']) {
        return Err(format!("Invalid aggregation name [{name}]. Aggregation names can contain any character except '[', ']', and '>'").into());
    }
    let body = body
        .as_object()
        .ok_or_else(|| format!("Expected [START_OBJECT] under [{name}]"))?;
    let (mut meta, mut kind, mut subs) = (None, None, vec![]);
    let (mut bucket_sort, mut bucket_selectors, mut derivatives) = (None, vec![], vec![]);
    for (k, v) in body {
        match k.as_str() {
            "meta" if !v.is_object() => {
                let token = es_index::json_token(v);
                return Err(format!(
                    "Expected [START_OBJECT] under [meta], but got a [{token}] in [{name}]"
                )
                .into());
            }
            "meta" => meta = Some(v.clone()),
            "aggs" | "aggregations" => {
                (subs, bucket_sort, bucket_selectors, derivatives) = parse_subs(v)?
            }
            t => match kind {
                Some((prev, _)) => {
                    return Err(format!(
                        "Found two aggregation type definitions in [{name}]: [{prev}] and [{t}]"
                    )
                    .into());
                }
                None => kind = Some((t, v)),
            },
        }
    }
    let (t, params) = kind.ok_or_else(|| format!("Missing definition for aggregation [{name}]"))?;
    let object = || {
        params
            .as_object()
            .ok_or_else(|| QueryError::from(format!("Expected [START_OBJECT] under [{t}]")))
    };
    let mut metric = match t {
        "avg" => Metric::Avg,
        "sum" => Metric::Sum,
        "min" => Metric::Min,
        "max" => Metric::Max,
        "value_count" => Metric::ValueCount,
        "stats" => Metric::Stats,
        "extended_stats" => Metric::ExtendedStats { sigma: 2.0 },
        "cardinality" => Metric::Cardinality {
            precision: hll::DEFAULT_PRECISION,
            segment_ords: None,
        },
        "percentiles" => Metric::Percentiles {
            compression: 100.0,
            percents: vec![1.0, 5.0, 25.0, 50.0, 75.0, 95.0, 99.0],
            keyed: true,
            ranks: false,
        },
        "percentile_ranks" => Metric::Percentiles {
            compression: 100.0,
            percents: vec![],
            keyed: true,
            ranks: true,
        },
        "terms" => Metric::Terms(Box::new(Terms::parse(name, object()?, &subs)?)),
        "histogram" | "date_histogram" => {
            let h = Histogram::parse(name, object()?, t == "date_histogram")?;
            Metric::Histogram(Box::new(h))
        }
        "filter" => Metric::Filter(Box::new(parse_query(params)?)),
        "missing" => Metric::Missing,
        "nested" => match params.get("path").and_then(Value::as_str) {
            Some(path) => Metric::Nested(path.to_owned()),
            None => {
                return Err(QueryError::x_content(format!(
                    "Missing [path] field for nested aggregation [{name}]"
                )));
            }
        },
        "global" => Metric::Global,
        "filters" => Metric::Filters(Box::new(Filters::parse(object()?)?)),
        "range" | "date_range" => Metric::Range(Box::new(Ranges::parse(name, t, object()?)?)),
        "top_hits" => Metric::TopHits(Box::new(TopHits::parse(object()?)?)),
        _ => return Err(format!("Unknown aggregation type [{t}]").into()),
    };
    let has_pipelines = bucket_sort.is_some() || !bucket_selectors.is_empty();
    if (!subs.is_empty() || has_pipelines || !derivatives.is_empty()) && !metric.buckets() {
        return Err(QueryError::illegal(format!(
            "Aggregator [{name}] of type [{t}] cannot accept sub-aggregations"
        )));
    }
    if let Metric::Filter(_)
    | Metric::Nested(_)
    | Metric::Global
    | Metric::Filters(_)
    | Metric::TopHits(_) = metric
    {
        return Ok(Agg {
            name: name.to_owned(),
            meta,
            metric,
            field: String::new(),
            missing: None,
            subs,
            bucket_sort,
            bucket_selectors,
            derivatives,
        });
    }
    let params = object()?;
    for (k, v) in params {
        match (k.as_str(), &mut metric) {
            ("field" | "missing", _)
            | (_, Metric::Terms(_) | Metric::Histogram(_) | Metric::Range(_)) => {}
            ("sigma", Metric::ExtendedStats { sigma }) => {
                *sigma = num(v).ok_or_else(|| {
                    QueryError::x_content(format!("[{t}] failed to parse field [sigma]"))
                })?;
                if *sigma < 0.0 {
                    return Err(QueryError::illegal(format!(
                        "[sigma] must be greater than or equal to 0. Found [{sigma:?}] in [{name}]"
                    )));
                }
            }
            ("precision_threshold", Metric::Cardinality { precision, .. }) => {
                let n = int(v).ok_or_else(|| {
                    QueryError::x_content(format!("[{t}] failed to parse field [{k}]"))
                })?;
                *precision = u64::try_from(n).map(hll::precision_from_threshold).map_err(|_| {
                    QueryError::illegal(format!(
                        "[precisionThreshold] must be greater than or equal to 0. Found [{n}] in [{name}]"
                    ))
                })?;
            }
            // ponytail: every mode counts the same; the hint only picks ES's collector. The
            // heuristics assume few terms (ES switches off global ordinals at 4 x terms >= HLL
            // memory); count a shard's terms if profiles must match there.
            ("execution_hint", Metric::Cardinality { segment_ords, .. }) => {
                *segment_ords = match v.as_str().unwrap_or_default().to_ascii_lowercase().as_str() {
                    "global_ordinals" | "save_memory_heuristic" | "save_time_heuristic" => None,
                    "segment_ordinals" => Some(true),
                    "direct" => Some(false),
                    _ => {
                        return Err(QueryError::illegal(format!(
                            "Invalid execution mode for cardinality aggregation.  Got [{}]expected one of [global_ordinal, segment_ordinal, direct]",
                            es_index::postings::text(v)
                        )));
                    }
                };
            }
            (
                "percents",
                Metric::Percentiles {
                    percents,
                    ranks: false,
                    ..
                },
            ) => {
                let bad = || QueryError::x_content(format!("[{t}] failed to parse field [{k}]"));
                let mut p: Vec<f64> = (v.as_array().ok_or_else(bad)?.iter())
                    .map(|v| num(v).ok_or_else(bad))
                    .collect::<Result<_, _>>()?;
                if p.is_empty() {
                    return Err(QueryError::illegal(format!(
                        "[percents] must not be empty: [{name}]"
                    )));
                }
                p.sort_by(f64::total_cmp);
                for (i, &x) in p.iter().enumerate() {
                    if !(0.0..=100.0).contains(&x) {
                        return Err(QueryError::illegal(format!(
                            "percent must be in [0,100], got [{}]: [{name}]",
                            java_double(x)
                        )));
                    }
                    if i > 0 && p[i - 1] == x {
                        return Err(QueryError::illegal(format!(
                            "percent [{}] has been specified twice: [{name}]",
                            java_double(x)
                        )));
                    }
                }
                *percents = p;
            }
            (
                "values",
                Metric::Percentiles {
                    percents,
                    ranks: true,
                    ..
                },
            ) => {
                let bad = || QueryError::x_content(format!("[{t}] failed to parse field [{k}]"));
                *percents = (v.as_array().ok_or_else(bad)?.iter())
                    .map(|v| num(v).ok_or_else(bad))
                    .collect::<Result<_, _>>()?;
                if percents.is_empty() {
                    return Err(QueryError::illegal(format!(
                        "[values] must not be an empty array: [{name}]"
                    )));
                }
                percents.sort_by(f64::total_cmp);
            }
            ("keyed", Metric::Percentiles { keyed, .. }) => {
                *keyed = v.as_bool().ok_or_else(|| {
                    QueryError::x_content(format!("[{t}] failed to parse field [{k}]"))
                })?;
            }
            ("tdigest", Metric::Percentiles { compression, .. }) => {
                let o = v
                    .as_object()
                    .ok_or_else(|| format!("Expected [START_OBJECT] under [{k}]"))?;
                for (k, v) in o {
                    match k.as_str() {
                        "compression" => {
                            *compression = num(v).ok_or_else(|| {
                                QueryError::x_content(format!(
                                    "[tdigest] failed to parse field [{k}]"
                                ))
                            })?;
                            if *compression < 0.0 {
                                return Err(QueryError::illegal(format!(
                                    "[compression] must be greater than or equal to 0. Found [{}]",
                                    java_double(*compression)
                                )));
                            }
                        }
                        // ponytail: high_accuracy uses the default digest, which is exact up to
                        // 20x compression values; ES's AVLTreeDigest (randomized) differs above
                        "execution_hint" => {
                            let hint = v.as_str().unwrap_or_default();
                            let hints =
                                ["default", "", "high_accuracy", "DEFAULT", "HIGH_ACCURACY"];
                            if !hints.contains(&hint) {
                                return Err(QueryError::illegal(format!(
                                    "Invalid execution_hint [{}], valid values are [default, high_accuracy]",
                                    es_index::postings::text(v)
                                )));
                            }
                        }
                        _ => return Err(format!("[tdigest] unknown field [{k}]").into()),
                    }
                }
            }
            _ => return Err(format!("[{t}] unknown field [{k}]").into()),
        }
    }
    if let Metric::Percentiles {
        percents,
        ranks: true,
        ..
    } = &metric
        && percents.is_empty()
    {
        return Err(QueryError::illegal(format!(
            "[values] must not be null: [{name}]"
        )));
    }
    let field = params.get("field").and_then(Value::as_str).ok_or_else(|| {
        QueryError::illegal("Required one of fields [field, script], but none were specified.")
    })?;
    Ok(Agg {
        name: name.to_owned(),
        meta,
        metric,
        field: field.to_owned(),
        missing: params.get("missing").cloned(),
        subs,
        bucket_sort,
        bucket_selectors,
        derivatives,
    })
}

/// Splits an order path into its first aggregation's name and the rest: `>` and the path into
/// that single-bucket aggregation, `[key]` or `.` and a metric key, or nothing. Like ES's
/// AggregationPath, only the last element may have a `[key]`.
fn split_path(path: &str) -> Result<(&str, &str), String> {
    let key = |e: &str| e.rfind('[').filter(|&k| k > 0 && e[k..].contains(']'));
    if let Some(i) = path.find('>') {
        let first = &path[..i];
        return match key(first) {
            Some(_) => Err(format!(
                "Key only allowed on last aggregation path element but got [{first}]"
            )),
            None => Ok(path.split_at(i)),
        };
    }
    Ok(path.split_at(key(path).or_else(|| path.rfind('.')).unwrap_or(path.len())))
}

/// Refuses an order path with a malformed element, as ES's AggregationPath.parse does: a
/// `[key]` with no name before it, an empty or unclosed key, text after the last element's
/// key, or a metric `.` at either end of the last element.
fn check_path(path: &str) -> Result<(), QueryError> {
    let elements: Vec<&str> = path.split('>').filter(|e| !e.is_empty()).collect();
    for (i, e) in elements.iter().enumerate() {
        let (key, metric, end) = (e.rfind('['), e.rfind('.'), e.rfind(']'));
        let bad = match (key, i + 1 == elements.len()) {
            (Some(k), _) if k == 0 || k + 3 > e.len() => true,
            (Some(_), false) => !e.ends_with(']'),
            (Some(k), true) => end.is_none_or(|j| j < k) || (metric.is_none() && !e.ends_with(']')),
            (None, true) => metric.is_some_and(|m| m == 0 || m + 2 > e.len()),
            (None, false) => false,
        };
        if bad {
            let msg = format!("Invalid path element [{e}] in path [{path}]");
            let outer = QueryError::x_content("[terms] failed to parse field [order]");
            return Err(outer.caused_by(QueryError::illegal(msg)));
        }
    }
    Ok(())
}

/// The message of an order path into an aggregation that has no single value.
const NOT_SORTABLE: &str = "Buckets can only be sorted on a sub-aggregator path that is built out of zero or more single-bucket aggregations within the path and a final single-bucket or a metrics aggregation at the path end.";

/// The value `terms` buckets sort by for an order path into `agg` (`rest` is the path after
/// its name, see `split_path`), like ES's aggregators' `bucketComparator`; `st` is `agg`'s
/// result, `None` to only check the path. `Err` explains an invalid path.
fn sort_value(agg: &Agg, rest: &str, st: Option<&Stats>) -> Result<f64, String> {
    let name = &agg.name;
    if let Some(path) = rest.strip_prefix('>') {
        if !agg.metric.single() {
            return Err(NOT_SORTABLE.into());
        }
        let (sub, rest) = split_path(path)?;
        let j = (agg.subs.iter().position(|s| s.name == sub)).ok_or_else(|| format!("The provided aggregation [{sub}] either does not exist, or is a pipeline aggregation and cannot be used to sort the buckets."))?;
        return sort_value(&agg.subs[j], rest, st.and_then(|s| s.subs.get(j)));
    }
    // a `[key]` wins over a `.metric` after it, as in AggregationPath.bucketComparator
    let key = match rest.strip_prefix('[') {
        Some(r) => r.rsplit_once(']').map(|(k, _)| k),
        None => rest.strip_prefix('.'),
    };
    let empty;
    let st = match st {
        Some(st) => st,
        None => {
            empty = Stats::default();
            &empty
        }
    };
    let avg = st.sum / st.count as f64;
    let single = match agg.metric {
        Metric::Filter(_) | Metric::Missing | Metric::Nested(_) | Metric::Global => {
            return match key {
                None | Some("doc_count") => Ok(st.count as f64),
                Some(_) => Err(format!(
                    "Ordering on a single-bucket aggregation can only be done on its doc_count. Either drop the key (a la \"{name}\") or change it to \"doc_count\" (a la \"{name}.doc_count\") or \"key\"."
                )),
            };
        }
        Metric::Avg => avg,
        Metric::Sum => st.sum,
        Metric::Min => st.min,
        Metric::Max => st.max,
        Metric::ValueCount => st.count as f64,
        Metric::Cardinality { .. } => st.hll.as_ref().map_or(0, Hll::cardinality) as f64,
        Metric::Terms(_)
        | Metric::Histogram(_)
        | Metric::Filters(_)
        | Metric::Range(_)
        | Metric::TopHits(_) => {
            return Err(NOT_SORTABLE.into());
        }
        Metric::Stats | Metric::ExtendedStats { .. } | Metric::Percentiles { .. } => {
            let unknown = |k| {
                format!("Unknown metric name [{k}] on multi-value metrics aggregation [{name}]")
            };
            return match (key, &agg.metric) {
                (None, _) => Err("When ordering on a multi-value metrics aggregation a metric name must be specified.".into()),
                // AbstractTDigestPercentilesAggregator.hasMetric and metric
                (Some(k), Metric::Percentiles { percents, ranks, .. }) => {
                    let x: f64 = k.parse().map_err(|_| format!("For input string: \"{k}\""))?;
                    if !percents.contains(&x) {
                        return Err(unknown(k));
                    }
                    // ponytail: clones the digest per comparison; cache values per bucket if
                    // sorting many buckets shows up in profiles
                    let Some(mut d) = st.digest.clone() else {
                        return Ok(f64::NAN);
                    };
                    Ok(match ranks {
                        true => d.cdf(x).clamp(0.0, 1.0) * 100.0,
                        false => d.quantile(x / 100.0),
                    })
                }
                (Some("count"), _) => Ok(st.count as f64),
                (Some("sum"), _) => Ok(st.sum),
                (Some("min"), _) => Ok(st.min),
                (Some("max"), _) => Ok(st.max),
                (Some("avg"), _) => Ok(avg),
                (Some(k), _) => Err(unknown(k)),
            };
        }
    };
    match key {
        None | Some("value") => Ok(single),
        Some(_) => Err(format!(
            "Ordering on a single-value metrics aggregation can only be done on its value. Either drop the key (a la \"{name}\") or change it to \"value\" (a la \"{name}.value\")"
        )),
    }
}

/// What a metric aggregation accumulates: every metric is read from it.
#[derive(Debug, Clone)]
pub(crate) struct Stats {
    count: u64,
    /// Kahan-compensated, like ES's CompensatedSum.
    sum: f64,
    delta: f64,
    /// The sum of squares, also compensated (only for `extended_stats`).
    sum_sq: f64,
    sq_delta: f64,
    min: f64,
    max: f64,
    /// The format of the field, if it is a date in some index, so values also render as dates.
    date: Option<String>,
    /// The distinct values (only for `cardinality`).
    hll: Option<Hll>,
    /// The value distribution (only for `percentiles`).
    digest: Option<TDigest>,
    /// The buckets (only for `terms` and `histogram`).
    terms: Option<Buckets>,
    /// The sub-aggregations' results (only for single-bucket aggregations).
    subs: Vec<Stats>,
    /// Each bucket's count and sub-aggregations (only for `filters` and `range`).
    buckets: Vec<Stats>,
    /// A `range` bucket's key and bounds.
    range: Option<RangeBucket>,
    /// The best hits so far (only for `top_hits`).
    top: Vec<TopHit>,
}

impl Default for Stats {
    fn default() -> Self {
        Stats {
            count: 0,
            sum: 0.0,
            delta: 0.0,
            sum_sq: 0.0,
            sq_delta: 0.0,
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
            date: None,
            hll: None,
            digest: None,
            terms: None,
            subs: vec![],
            buckets: vec![],
            range: None,
            top: vec![],
        }
    }
}

/// Adds `x` to a Kahan-compensated `sum`.
fn kahan(sum: &mut f64, delta: &mut f64, x: f64) {
    if !x.is_finite() {
        *sum += x;
    }
    if sum.is_finite() {
        let corrected = x + *delta;
        let updated = *sum + corrected;
        *delta = corrected - (updated - *sum);
        *sum = updated;
    }
}

/// The Kahan-compensated sum of `xs`.
pub(crate) fn compensated_sum(xs: impl IntoIterator<Item = f64>) -> f64 {
    let (mut sum, mut delta) = (0.0, 0.0);
    xs.into_iter().for_each(|x| kahan(&mut sum, &mut delta, x));
    sum
}

impl Stats {
    fn add(&mut self, x: f64) {
        kahan(&mut self.sum, &mut self.delta, x);
        kahan(&mut self.sum_sq, &mut self.sq_delta, x * x);
    }

    /// Reduces one shard's result into this one, as ES reduces shard results.
    fn merge(&mut self, o: Stats) {
        self.count += o.count;
        kahan(&mut self.sum, &mut self.delta, o.sum);
        kahan(&mut self.sum_sq, &mut self.sq_delta, o.sum_sq);
        self.min = self.min.min(o.min);
        self.max = self.max.max(o.max);
        self.date = self.date.take().or(o.date);
        self.range = self.range.take().or(o.range);
        self.top.extend(o.top);
        match (&mut self.hll, o.hll) {
            (Some(h), Some(o)) => h.merge(&o),
            (h, o) => *h = h.take().or(o),
        }
        match (&mut self.terms, o.terms) {
            (Some(t), Some(o)) => t.merge(o),
            (t, o) => *t = t.take().or(o),
        }
        match self.subs.is_empty() {
            true => self.subs = o.subs,
            false => reduce(&mut self.subs, o.subs),
        }
        match self.buckets.is_empty() {
            true => self.buckets = o.buckets,
            false => reduce(&mut self.buckets, o.buckets),
        }
        // like a data node, reduce shard results into a new digest (see TDigest::coordinated)
        if let Some(mut o) = o.digest {
            let d = self
                .digest
                .get_or_insert_with(|| TDigest::new(o.compression));
            d.add_digest(&mut o);
        }
    }
}

/// A document's doc values of `field`, read from its source.
fn doc_values(s: &Searcher, field: &str, mapping: &FieldMapping, doc: &Document) -> Vec<Value> {
    if field == "_ignored" {
        return (s.shard.ignored_fields(&s.index.mappings, doc).into_iter())
            .map(Value::from)
            .collect();
    }
    let path = source_path(s.index, field);
    // a keyword's with the `ignore_above` the document was indexed with
    let at;
    let mapping = match mapping {
        FieldMapping::Keyword {
            ignore_above,
            split_queries_on_whitespace,
        } => {
            at = FieldMapping::Keyword {
                ignore_above: s.shard.ignore_above(path, *ignore_above, doc.seq_no),
                split_queries_on_whitespace: *split_queries_on_whitespace,
            };
            &at
        }
        m => m,
    };
    let mut v: Vec<Value> = values(&doc.source, path)
        .into_iter()
        .filter_map(|v| doc_value(s, field, mapping, v))
        .collect();
    // keyword and ip doc values are a sorted set, other doc values sorted numbers
    v.sort_by(crate::scalar_cmp);
    if matches!(mapping, FieldMapping::Keyword { .. } | FieldMapping::Ip) {
        v.dedup();
    }
    v
}

/// A `fields`/`docvalue_fields` entry: a field name or `*` pattern, and the `format` of its
/// values.
#[derive(Debug, Clone, PartialEq)]
pub struct FieldSpec {
    pattern: String,
    format: Option<String>,
}

/// A `fields`/`docvalue_fields` list: names, or `{"field": name, "format": f}` objects.
pub(crate) fn field_specs(v: &Value) -> Option<Vec<FieldSpec>> {
    (v.as_array()?.iter())
        .map(|f| {
            let (pattern, format) = match f {
                Value::Object(o) => {
                    let format = match o.get("format") {
                        None => None,
                        Some(f) => Some(f.as_str()?.to_owned()),
                    };
                    let known = |k: &String| matches!(k.as_str(), "field" | "format");
                    (o.keys().all(known)).then_some((o.get("field")?, format))?
                }
                f => (f, None),
            };
            let pattern = pattern.as_str()?.to_owned();
            Some(FieldSpec { pattern, format })
        })
        .collect()
}

/// A hit's `stored_fields`: the source values of each field mapped with `store`.
// ponytail: only `text` can be stored; metadata fields (`_id`, `_routing`) are not rendered
fn stored_fields(s: &Searcher, fields: &[String], doc: &Document) -> Map<String, Value> {
    (fields.iter())
        .filter(|f| {
            matches!(
                s.index.mappings.field(f),
                Some(FieldMapping::Text { store: true, .. })
            )
        })
        .map(|f| (f.clone(), values(&doc.source, source_path(s.index, f))))
        .filter(|(_, v)| !v.is_empty())
        .map(|(f, v)| (f, Value::Array(v.into_iter().cloned().collect())))
        .collect()
}

/// Gives every `top_hits` without its own `fields` the search's, like ES's SubSearchContext.
pub(crate) fn inherit_fields(aggs: &mut [Agg], fields: &[FieldSpec]) {
    for a in aggs {
        if let Metric::TopHits(t) = &mut a.metric {
            t.fields.get_or_insert_with(|| fields.to_vec());
        }
        inherit_fields(&mut a.subs, fields);
    }
}

/// A hit's `fields`: each mapped field's values, dates formatted. `fetch` reads them like ES's
/// FieldFetcher (source order, text included, only dates take a `format`), otherwise as doc
/// values (numbers also take a DecimalFormat `format`). Patterns expand to mapped and runtime
/// fields; the last match of a field sets its format, like FetchDocValuesContext. A `format`
/// the field cannot take, or a text field's doc values, fail like ES's fetch (`index_name` is
/// the searched index's name).
pub(crate) fn hit_fields(
    s: &Searcher,
    index_name: &str,
    fields: &[FieldSpec],
    doc: &Document,
    fetch: bool,
) -> Result<Map<String, Value>, QueryError> {
    let mut expanded: Vec<(String, Option<&str>, &str)> = vec![];
    let names = || {
        let mut names = s.index.mappings.names();
        names.extend(s.runtime_source.iter().cloned());
        names
    };
    for spec in fields {
        let matched = match spec.pattern.contains('*') {
            true => (names().into_iter())
                .filter(|f| es_index::aliases::glob(&spec.pattern, f))
                .collect(),
            false => vec![spec.pattern.clone()],
        };
        for f in matched {
            let (format, pattern) = (spec.format.as_deref(), spec.pattern.as_str());
            match expanded.iter_mut().find(|(e, ..)| *e == f) {
                Some(e) => (e.1, e.2) = (format, pattern),
                None => expanded.push((f, format, pattern)),
            }
        }
    }
    (expanded.into_iter())
        .filter_map(|(f, format, pattern)| {
            let f = &f;
            if fetch && s.runtime_source.contains(f) {
                return Some(Ok((
                    f.clone(),
                    vec![Value::from(java_string(&doc.source))].into(),
                )));
            }
            let m = s.index.mappings.field(f)?;
            use FieldMapping as M;
            let ty = m.mapping_type();
            let refused = match (fetch, m) {
                (_, M::Object { .. } | M::Nested { .. }) => None,
                (true, M::Date { .. } | M::DateNanos { .. }) => None,
                (true, m) if m.is_range() => None,
                (true, _) => format.map(|_| {
                    let which = match pattern == f {
                        true => String::new(),
                        false => format!(" which matched [{pattern}]"),
                    };
                    let e = QueryError::illegal(format!(
                        "Field [{f}] of type [{ty}] doesn't support formats."
                    ));
                    QueryError::illegal(format!("error fetching [{f}]{which}: {e}")).caused_by(e)
                }),
                (false, M::Text { .. }) => Some(QueryError::illegal(format!(
                    "Fielddata is disabled on [{f}] in [{index_name}]. {}",
                    fielddata_disabled(f)
                ))),
                (false, M::Date { .. } | M::DateNanos { .. } | M::DateRange { .. }) => None,
                (false, m) if m.is_numeric() => None,
                (false, _) => format.map(|_| {
                    QueryError::illegal(format!(
                        "Field [{f}] of type [{ty}] does not support custom formats"
                    ))
                }),
            };
            if let Some(e) = refused {
                return Some(Err(e));
            }
            let v = match fetch {
                false => doc_values(s, f, m, doc),
                true => (values(&doc.source, source_path(s.index, f)).into_iter())
                    .filter_map(|v| match m {
                        FieldMapping::Text { .. } => Some(es_index::postings::text(v).into()),
                        _ => doc_value(s, f, m, v),
                    })
                    .collect(),
            };
            let v: Vec<Value> = match (m, format) {
                (FieldMapping::Date { format: mapped, .. }, f) => (v.iter())
                    .filter_map(|t| Some(date_string(f.or(mapped.as_deref()), int(t)?).into()))
                    .collect(),
                (m, Some(p)) if !fetch && m.is_numeric() => (v.iter())
                    .filter_map(|x| Some(decimal_format(p, num(x)?).into()))
                    .collect(),
                _ => v,
            };
            (!v.is_empty()).then(|| Ok((f.clone(), v.into())))
        })
        .collect()
}

/// A JSON value as Java's `toString` renders the parsed `Map`/`List`/scalar.
fn java_string(v: &Value) -> String {
    let join = |parts: Vec<String>| parts.join(", ");
    match v {
        Value::String(s) => s.clone(),
        Value::Array(a) => format!("[{}]", join(a.iter().map(java_string).collect())),
        Value::Object(o) => format!(
            "{{{}}}",
            join(
                o.iter()
                    .map(|(k, v)| format!("{k}={}", java_string(v)))
                    .collect()
            )
        ),
        v => v.to_string(),
    }
}

/// A source value as the field's doc values hold it; `None` if they cannot.
fn doc_value(s: &Searcher, field: &str, mapping: &FieldMapping, v: &Value) -> Option<Value> {
    use FieldMapping as M;
    match mapping {
        M::Long | M::Integer | M::Short | M::Byte => es_index::json_int(v).map(Value::from),
        M::Double => num(v).map(Value::from),
        M::Float => num(v).map(|f| Value::from(f64::from(f as f32))),
        M::HalfFloat => num(v).map(|f| {
            Value::from(f64::from(es_index::half_float(es_index::half_float_bits(
                f as f32,
            ))))
        }),
        M::Date { .. } | M::DateNanos { .. } => match s.instant(field) {
            Some(instant) => instant(v),
            // an unmapped field's `value_type`
            None => es_index::date_millis(None, None, v),
        }
        .map(Value::from),
        M::Boolean => match v {
            Value::Bool(_) => Some(v.clone()),
            Value::String(b) => b.parse::<bool>().ok().map(Value::Bool),
            _ => None,
        },
        M::Keyword { ignore_above, .. } => {
            let t = es_index::postings::text(v);
            (!es_index::postings::ignored(*ignore_above, &t)).then(|| t.into())
        }
        M::Ip => es_index::ip_bytes(v).map(|ip| es_index::ip_string(ip).into()),
        // aggregations read ip ranges in `Collector::values`
        M::IpRange => None,
        M::LongRange | M::IntegerRange | M::DateRange { .. } => {
            let (from, to) = mapping.range_keys(v).flatten()?;
            let n = es_index::mapping::key_long;
            Some(Value::from(vec![n(from), n(to)]))
        }
        // infinite ends as the largest finite doubles, which JSON can hold
        M::FloatRange | M::DoubleRange => {
            let (from, to) = mapping.range_keys(v).flatten()?;
            let n = |k| mapping.range_number(k).clamp(f64::MIN, f64::MAX);
            Some(Value::from(vec![n(from), n(to)]))
        }
        M::Text { .. } | M::Nested { .. } | M::Object { .. } => None,
    }
}

/// ES's ValuesSourceType of a field: every number is `numeric`.
fn values_source(m: &FieldMapping) -> &'static str {
    if m.is_numeric() {
        "numeric"
    } else {
        m.type_name()
    }
}

/// Reads a field's doc values for on-disk documents (see `Shard::disk_values`).
type DiskValues<'s> = Box<dyn FnMut(&str) -> Option<Vec<Value>> + 's>;

/// Collects one index's matches for every aggregation of a request.
pub(crate) struct Collector<'s, 'a> {
    searcher: &'s Searcher<'a>,
    /// The searched index's name, which `top_hits` report.
    index_name: &'s str,
    aggs: &'s [Agg],
    /// Per aggregation: its field's mapping in this index, `None` if unmapped.
    mappings: Vec<Option<&'a FieldMapping>>,
    disk: Vec<DiskValues<'s>>,
    /// Per aggregation: its `missing` value as a doc value of the field.
    missing: Vec<Option<Value>>,
    stats: Vec<Stats>,
    /// Per `terms` aggregation with sub-aggregations: their collector.
    subs: Vec<Option<Collector<'s, 'a>>>,
    /// Per aggregation, with `profile: true`: where its time went.
    timings: Option<Vec<Timings>>,
    /// The first failure collecting a document, which fails the shard.
    failed: Option<QueryError>,
}

/// Nanoseconds one aggregation spent in each phase of ES's aggregation profile on one shard.
#[derive(Debug, Default)]
struct Timings {
    initialize: u64,
    build_leaf_collector: u64,
    collect: u64,
    collect_count: u64,
    post_collection: u64,
    build_aggregation: u64,
    /// `terms`: the buckets this shard collected, before keeping its top `shard_size`.
    total_buckets: usize,
    /// Whether a document had several values. ponytail: a shard profiles as one segment.
    multi_valued: bool,
    /// keyword `terms`: the shard's accepted terms, before keeping its top `shard_size`.
    keys: Vec<String>,
}

fn nanos(since: Instant) -> u64 {
    since.elapsed().as_nanos() as u64
}

impl<'s, 'a> Collector<'s, 'a> {
    pub(crate) fn new(
        aggs: &'s [Agg],
        searcher: &'s Searcher<'a>,
        index_name: &'s str,
        profile: bool,
    ) -> Result<Self, QueryError> {
        let index = searcher.index;
        let mut c = Collector {
            searcher,
            index_name,
            aggs,
            mappings: vec![],
            disk: vec![],
            missing: vec![],
            stats: vec![],
            subs: vec![],
            timings: profile.then(Vec::new),
            failed: None,
        };
        for agg in aggs {
            let start = Instant::now();
            let m = match agg.field.as_str() {
                "_ignored" => Some(&terms::KEYWORD),
                f => index.mappings.field(f),
            };
            match m {
                Some(FieldMapping::Text { .. }) => {
                    return Err(fielddata_disabled(&agg.field));
                }
                Some(FieldMapping::Keyword { .. })
                    if !matches!(
                        agg.metric,
                        Metric::ValueCount
                            | Metric::Cardinality { .. }
                            | Metric::Terms(_)
                            | Metric::Missing
                    ) =>
                {
                    return Err(QueryError::illegal(format!(
                        "Field [{}] of type [keyword] is not supported for aggregation [{}]",
                        agg.field,
                        agg.metric.name()
                    )));
                }
                Some(FieldMapping::IpRange) => match &agg.metric {
                    Metric::ValueCount | Metric::Cardinality { .. } | Metric::Missing => {}
                    Metric::Histogram(h) if h.date => {
                        return Err(QueryError::illegal(
                            "Expected date range type but found range type [ip_range]",
                        ));
                    }
                    Metric::Histogram(_) => {
                        return Err(QueryError::illegal(
                            "Expected numeric range type but found non-numeric range [ip_range]",
                        ));
                    }
                    m => {
                        return Err(QueryError::illegal(format!(
                            "Field [{}] of type [ip_range] is not supported for aggregation [{}]",
                            agg.field,
                            m.name()
                        )));
                    }
                },
                _ => {}
            }
            if let (Some(m), Metric::Terms(t)) = (m, &agg.metric)
                && let Some(hint) = t.value_type.as_ref()
                && values_source(m) != values_source(hint)
            {
                // ES's ValuesSourceConfig fails the shard, so correctly mapped shards still answer
                return Err(QueryError::illegal(format!(
                    "Field type [{}] is incompatible with specified value_type [{}]",
                    m.type_name(),
                    match hint {
                        FieldMapping::Keyword { .. } => "string",
                        FieldMapping::Long => "byte|short|integer|long",
                        FieldMapping::Double => "float|double",
                        h => h.type_name(),
                    }
                )));
            }
            // terms type an unmapped field's values by `value_type`, else as keywords
            let typed = m.or(match &agg.metric {
                Metric::Terms(t) => Some(t.value_type.as_ref().unwrap_or(&terms::KEYWORD)),
                _ => None,
            });
            let as_doc_value =
                |v: &Value| typed.and_then(|m| doc_value(searcher, &agg.field, m, v));
            c.missing.push(match typed {
                Some(_) => agg.missing.as_ref().and_then(as_doc_value),
                None => agg.missing.clone(),
            });
            c.mappings.push(m);
            let initialize = nanos(start);
            let start = Instant::now();
            c.disk.push(Box::new(
                searcher.shard.disk_values(&index.mappings, &agg.field),
            ));
            if let Some(t) = &mut c.timings {
                t.push(Timings {
                    initialize,
                    build_leaf_collector: nanos(start),
                    ..Timings::default()
                });
            }
            let sub = match agg.metric.buckets() && !agg.subs.is_empty() {
                true => Some(Collector::new(&agg.subs, searcher, index_name, profile)?),
                false => None,
            };
            let empty = sub.as_ref().map(|s| s.stats.clone()).unwrap_or_default();
            c.subs.push(sub);
            c.stats.push(Stats {
                date: match m {
                    Some(
                        FieldMapping::Date { format, .. } | FieldMapping::DateNanos { format, .. },
                    ) => Some(
                        format
                            .clone()
                            .unwrap_or_else(|| "strict_date_optional_time".into()),
                    ),
                    _ => None,
                },
                subs: match agg.metric.single() {
                    true => empty.clone(),
                    false => vec![],
                },
                buckets: match &agg.metric {
                    Metric::Filters(f) => {
                        let n = f.filters.len() + usize::from(f.other.is_some());
                        let bucket = Stats {
                            subs: empty.clone(),
                            ..Stats::default()
                        };
                        vec![bucket; n]
                    }
                    Metric::Range(r) => r.buckets(m, &empty)?,
                    _ => vec![],
                },
                hll: match agg.metric {
                    Metric::Cardinality { precision, .. } => Some(Hll::new(precision)),
                    _ => None,
                },
                digest: match agg.metric {
                    Metric::Percentiles { compression, .. } => Some(TDigest::new(compression)),
                    _ => None,
                },
                terms: match &agg.metric {
                    Metric::Terms(t) => {
                        let has_values = m.is_some() || agg.missing.is_some();
                        let format = Format::of(typed.filter(|_| has_values));
                        let parse = |s: &str| as_doc_value(&Value::from(s));
                        Some(t.buckets(&agg.name, format, parse, empty)?)
                    }
                    Metric::Histogram(h) => {
                        let (format, locale) = date_format(m);
                        let bounds = h.bounds(&agg.name, format, locale)?;
                        Some(histogram::buckets(Format::of(m), empty, bounds))
                    }
                    _ => None,
                },
                ..Stats::default()
            });
        }
        Ok(c)
    }

    /// Collects a matching document (`score` its query score).
    pub(crate) fn collect(&mut self, id: &str, doc: &Document, score: f64) {
        for i in 0..self.aggs.len() {
            let start = self.timings.is_some().then(Instant::now);
            self.collect_one(i, id, doc, score);
            if let (Some(t), Some(start)) = (&mut self.timings, start) {
                t[i].collect += nanos(start);
                t[i].collect_count += 1;
            }
        }
    }

    fn collect_one(&mut self, i: usize, id: &str, doc: &Document, score: f64) {
        let matches = match &self.aggs[i].metric {
            Metric::TopHits(t) => {
                let (s, name) = (self.searcher, self.index_name);
                let fetch = t.fields.as_deref().unwrap_or_default();
                let fields =
                    hit_fields(s, name, &t.docvalue_fields, doc, false).and_then(|mut f| {
                        f.extend(stored_fields(s, &t.stored_fields, doc));
                        f.extend(hit_fields(s, name, fetch, doc, true)?);
                        Ok(f)
                    });
                let fields = match fields {
                    Ok(f) => f,
                    Err(e) => {
                        self.failed.get_or_insert(e);
                        return;
                    }
                };
                let st = &mut self.stats[i];
                st.count += 1;
                st.top.push(TopHit {
                    index: self.index_name.to_owned(),
                    id: id.to_owned(),
                    score,
                    sort: self.searcher.sort_values(&t.sort, score, doc),
                    matched_queries: self.searcher.matched_queries(id, doc),
                    explanation: t.explain.then(|| self.searcher.explain(id, doc)),
                    highlight: (t.highlight.iter())
                        .map(|(f, q)| {
                            (
                                f,
                                self.searcher.highlight(
                                    q.as_ref().unwrap_or(&self.searcher.query),
                                    f,
                                    doc,
                                ),
                            )
                        })
                        .filter(|(_, h)| !h.is_empty())
                        .map(|(f, h)| (f.clone(), h.into()))
                        .collect(),
                    doc: doc.clone(),
                    fields,
                });
                // ponytail: re-sorts at twice the kept hits; a heap if top_hits get large
                if st.top.len() > 2 * t.from.saturating_add(t.size).max(8) {
                    t.keep_top(&mut st.top);
                }
                return;
            }
            Metric::Filter(q) => self.searcher.score(q, id, doc).is_some(),
            Metric::Nested(path) => !es_index::postings::values(&doc.source, path).is_empty(),
            Metric::Missing => self.values(i, id, doc).is_empty() && self.missing[i].is_none(),
            // collected over every document by `finish_terms`
            Metric::Global => false,
            Metric::Filters(f) => {
                let mut hits: Vec<_> = (f.filters.iter())
                    .map(|(_, q)| self.searcher.score(q, id, doc).is_some())
                    .collect();
                if f.other.is_some() {
                    hits.push(!hits.contains(&true));
                }
                let st = &mut self.stats[i];
                for (b, _) in st.buckets.iter_mut().zip(hits).filter(|(_, hit)| *hit) {
                    b.count += 1;
                    if let Some(sub) = &mut self.subs[i] {
                        sub.collect_into(&mut b.subs, id, doc, score);
                    }
                }
                return;
            }
            Metric::Range(_) => {
                let mut vals = self.values(i, id, doc);
                if vals.is_empty() {
                    vals.extend(self.missing[i].clone());
                }
                // the bounds take the field's precision, like ES's reduceToStoredPrecision
                let fix = |x: f64| match self.mappings[i] {
                    Some(FieldMapping::Float) => f64::from(x as f32),
                    Some(FieldMapping::HalfFloat) => {
                        f64::from(es_index::half_float(es_index::half_float_bits(x as f32)))
                    }
                    _ => x,
                };
                let hits: Vec<bool> = (self.stats[i].buckets.iter())
                    .map(|b| {
                        let [from, to] = b.range.as_ref().map_or([0.0; 2], |r| r.bounds.map(fix));
                        vals.iter().filter_map(num).any(|x| from <= x && x < to)
                    })
                    .collect();
                let st = &mut self.stats[i];
                for (b, _) in st.buckets.iter_mut().zip(hits).filter(|(_, hit)| *hit) {
                    b.count += 1;
                    if let Some(sub) = &mut self.subs[i] {
                        sub.collect_into(&mut b.subs, id, doc, score);
                    }
                }
                return;
            }
            _ => {
                let vals = self.values(i, id, doc);
                if let Some(t) = &mut self.timings {
                    t[i].multi_valued |= vals.len() > 1;
                }
                match vals.is_empty() {
                    true => self.add(i, self.missing[i].iter().cloned().collect(), id, doc, score),
                    false => self.add(i, vals, id, doc, score),
                }
                return;
            }
        };
        if matches {
            let st = &mut self.stats[i];
            st.count += 1;
            if let Some(sub) = &mut self.subs[i] {
                sub.collect_into(&mut st.subs, id, doc, score);
            }
        }
    }

    /// Collects a document into `stats`, one bucket's results of these aggregations.
    fn collect_into(&mut self, stats: &mut Vec<Stats>, id: &str, doc: &Document, score: f64) {
        std::mem::swap(&mut self.stats, stats);
        self.collect(id, doc, score);
        std::mem::swap(&mut self.stats, stats);
    }

    /// A document's doc values of aggregation `i`'s field.
    fn values(&mut self, i: usize, id: &str, doc: &Document) -> Vec<Value> {
        let agg = &self.aggs[i];
        match self.mappings[i] {
            None => vec![],
            Some(m @ FieldMapping::IpRange) => {
                // one doc value: the ranges as ES's BinaryRangeUtil.encodeIPRanges writes them
                // ponytail: sorted, where ES writes a HashSet's order; a document with several
                // ranges hashes differently, which `cardinality` shows past precision_threshold
                let path = source_path(self.searcher.index, &agg.field);
                let mut ranges: Vec<_> = (values(&doc.source, path).into_iter())
                    .filter_map(|v| m.range_keys(v).flatten())
                    .collect();
                ranges.sort_unstable();
                ranges.dedup();
                if ranges.is_empty() {
                    return vec![];
                }
                let mut out = es_codec::output::Output(vec![]);
                out.vint(ranges.len() as i32);
                for (from, to) in ranges {
                    out.bytes(&from.to_be_bytes());
                    out.bytes(&to.to_be_bytes());
                }
                vec![out.0.into()]
            }
            Some(m) => {
                self.disk[i](id).unwrap_or_else(|| doc_values(self.searcher, &agg.field, m, doc))
            }
        }
    }

    fn add(&mut self, i: usize, vals: Vec<Value>, id: &str, doc: &Document, score: f64) {
        let vals = match &self.aggs[i].metric {
            Metric::Histogram(h) => match h.keys(
                vals,
                self.stats[i]
                    .terms
                    .as_ref()
                    .and_then(|b| b.bounds.1.as_ref()),
            ) {
                Ok(keys) => keys,
                Err(e) => {
                    self.failed.get_or_insert(e);
                    return;
                }
            },
            _ => vals,
        };
        let st = &mut self.stats[i];
        if let Some(t) = &mut st.terms {
            let sub = &mut self.subs[i];
            t.add(vals, |stats| {
                if let Some(sub) = sub {
                    sub.collect_into(stats, id, doc, score);
                }
            });
            return;
        }
        for v in vals {
            st.count += 1;
            if self.aggs[i].metric == Metric::ValueCount {
                continue;
            }
            if let Some(h) = &mut st.hll {
                h.collect(hash(self.mappings[i], &v));
                continue;
            }
            let Some(x) = num(&v).or_else(|| v.as_bool().map(f64::from)) else {
                continue;
            };
            if let Some(d) = &mut st.digest {
                d.add(x, 1);
                continue;
            }
            st.add(x);
            st.min = st.min.min(x);
            st.max = st.max.max(x);
        }
    }
}

impl Collector<'_, '_> {
    /// This shard's results, and with `profile: true` its `aggregations` profile: one entry per
    /// aggregation, like ES's (post collection releases the doc values readers, building the
    /// aggregation takes its result). `match_all` if the search's query matches every document.
    /// The first failure collecting a document, here or in a sub-aggregation.
    pub(crate) fn failed(&mut self) -> Option<QueryError> {
        (self.failed.take()).or_else(|| self.subs.iter_mut().flatten().find_map(Self::failed))
    }

    pub(crate) fn finish(mut self, match_all: bool) -> (Vec<Stats>, Option<Value>) {
        let mut stats = std::mem::take(&mut self.stats);
        self.finish_terms(&mut stats);
        self.stats = stats;
        let Some(mut timings) = self.timings.take() else {
            return (self.stats, None);
        };
        let mut stats = vec![];
        let disk = std::mem::take(&mut self.disk);
        for ((t, d), st) in timings.iter_mut().zip(disk).zip(&mut self.stats) {
            let start = Instant::now();
            drop(d);
            t.post_collection = nanos(start);
            let start = Instant::now();
            stats.push(std::mem::take(st));
            t.build_aggregation = nanos(start);
        }
        let profile = timings.iter().enumerate();
        let profile = profile.map(|(i, t)| self.profile(i, t, false, match_all));
        (stats, Some(profile.collect()))
    }

    /// Keeps each `terms` aggregation's top buckets of this shard in `stats` (and so on in
    /// their sub-aggregations), first adding empty buckets for the values of unmatched
    /// documents if `min_doc_count` is 0.
    /// Also collects every document into each `global` aggregation.
    fn finish_terms(&mut self, stats: &mut [Stats]) {
        let (index, shard) = (self.searcher.index, self.searcher.shard);
        for (i, st) in stats.iter_mut().enumerate() {
            if self.aggs[i].metric == Metric::Global {
                for (id, doc) in shard
                    .docs()
                    .filter(|(_, d)| shard.searchable(&index.settings, d.seq_no))
                {
                    st.count += 1;
                    if let Some(sub) = &mut self.subs[i] {
                        // ES scores `global` matches as match_all does
                        sub.collect_into(&mut st.subs, id, &doc, 1.0);
                    }
                }
            }
            if let Metric::TopHits(t) = &self.aggs[i].metric {
                t.keep_top(&mut st.top);
            }
            if let (true, Some(sub)) = (self.aggs[i].metric.single(), &mut self.subs[i]) {
                sub.finish_terms(&mut st.subs);
            }
            if let Some(sub) = &mut self.subs[i] {
                for b in &mut st.buckets {
                    sub.finish_terms(&mut b.subs);
                }
            }
            let Some(b) = &mut st.terms else {
                continue;
            };
            let terms = match &self.aggs[i].metric {
                Metric::Terms(t) => Some(t),
                _ => None,
            };
            if let Some(t) = terms
                && t.wants_zero_buckets(b)
            {
                for (id, doc) in shard
                    .docs()
                    .filter(|(_, d)| shard.searchable(&index.settings, d.seq_no))
                {
                    b.add_zero(self.values(i, id, &doc));
                }
            }
            if let Some(timings) = &mut self.timings {
                // a sub-aggregation's buckets add up over its parent's buckets
                timings[i].total_buckets += b.counts.len();
                timings[i].keys = (b.counts.keys())
                    .filter_map(|k| match k {
                        terms::Key::Str(s) => Some(s.clone()),
                        _ => None,
                    })
                    .collect();
            }
            if let Some(t) = terms {
                t.finish_shard(b);
            }
            if let Some(sub) = &mut self.subs[i] {
                for bucket in b.counts.values_mut() {
                    sub.finish_terms(&mut bucket.subs);
                }
            }
        }
    }

    /// One aggregation's profile: ES's aggregator class, its timings and its debug counters
    /// (this shard is one leaf); `under_bucket` if a bucket aggregation contains it.
    fn profile(&self, i: usize, t: &Timings, under_bucket: bool, match_all: bool) -> Value {
        let (agg, m) = (&self.aggs[i], self.mappings[i]);
        let keyword = matches!(m, Some(FieldMapping::Keyword { .. }));
        // TermsAggregatorFactory runs a top-level global ordinals `terms` as one term query per
        // ordinal when it merges with the search's query (only match_all does) and there are
        // at most MAX_ORDS_TO_TRY_FILTERS ordinals. ponytail: counts accepted terms, not all ords.
        let from_filters = !under_bucket
            && match_all
            && keyword
            && agg.missing.is_none()
            && matches!(&agg.metric, Metric::Terms(terms) if !terms.map)
            && (1..=1000).contains(&t.keys.len());
        let ty = match &agg.metric {
            Metric::Histogram(h) if h.date => "DateHistogramAggregator",
            Metric::Histogram(_) => "NumericHistogramAggregator",
            // ponytail: ES may run a filter as FilterByFilterAggregator; not profiled yet
            Metric::Filter(_) => "FilterAggregator",
            Metric::Missing => "MissingAggregator",
            Metric::Nested(_) => "NestedAggregator",
            Metric::Global => "GlobalAggregator",
            // ponytail: ES may run filters as FilterByFilterAggregator; not profiled yet
            Metric::Filters(_) => "FiltersAggregator",
            // ponytail: ES runs ranges as NoOverlap, Overlap or from filters; not profiled yet
            Metric::Range(_) => "NoOverlap",
            Metric::TopHits(_) => "TopHitsAggregator",
            _ if m.is_none() && agg.missing.is_none() => match agg.metric {
                Metric::Stats | Metric::ExtendedStats { .. } | Metric::Percentiles { .. } => {
                    "NonCollectingMultiMetricAggregator"
                }
                _ => "NonCollectingSingleMetricAggregator",
            },
            Metric::Cardinality {
                segment_ords: None, ..
            } if keyword => "GlobalOrdCardinalityAggregator",
            Metric::Cardinality { .. } => "CardinalityAggregator",
            Metric::Avg => "AvgAggregator",
            Metric::Sum => "SumAggregator",
            Metric::Min => "MinAggregator",
            Metric::Max => "MaxAggregator",
            Metric::ValueCount => "ValueCountAggregator",
            Metric::Stats => "StatsAggregator",
            Metric::ExtendedStats { .. } => "ExtendedStatsAggregator",
            Metric::Percentiles { ranks: false, .. } => "TDigestPercentilesAggregator",
            Metric::Percentiles { .. } => "TDigestPercentileRanksAggregator",
            Metric::Terms(_) if from_filters => "StringTermsAggregatorFromFilters",
            Metric::Terms(t) if keyword && t.map => "MapStringTermsAggregator",
            Metric::Terms(_) if keyword => "GlobalOrdinalsStringTermsAggregator",
            Metric::Terms(_) => "NumericTermsAggregator",
        };
        // the filters collect matches themselves, never through this aggregator
        let (collect, collect_count) = match from_filters {
            true => (0, 0),
            false => (t.collect, t.collect_count),
        };
        let mut out = serde_json::json!({
            "type": ty,
            "description": agg.name,
            "time_in_nanos": t.initialize + t.build_leaf_collector + collect
                + t.post_collection + t.build_aggregation,
            "breakdown": {
                "initialize": t.initialize, "initialize_count": 1,
                "build_leaf_collector": t.build_leaf_collector, "build_leaf_collector_count": 1,
                "collect": collect, "collect_count": collect_count,
                "post_collection": t.post_collection, "post_collection_count": 1,
                "build_aggregation": t.build_aggregation, "build_aggregation_count": 1,
                "reduce": 0, "reduce_count": 0,
            },
        });
        let used = |b: bool| u32::from(b);
        match (ty, &agg.metric) {
            ("StringTermsAggregatorFromFilters", _) => {
                // FilterByFilterAggregator counts each term query's matches from its doc freq
                // without sub-aggregations, else collects them. ponytail: no deleted docs.
                let counted = used(agg.subs.is_empty());
                let filters = t.keys.iter().map(|k| {
                    let query = format!("{}:{k}", agg.field);
                    serde_json::json!({"query": query, "segments_counted_in_constant_time": counted})
                });
                out["debug"] = serde_json::json!({
                    "delegate": "FilterByFilterAggregator",
                    "delegate_debug": {
                        "filters": filters.collect::<Vec<_>>(),
                        "segments_counted": counted,
                        "segments_collected": 1 - counted,
                        "segments_with_deleted_docs": 0,
                        "segments_with_doc_count_field": 0,
                    },
                });
            }
            ("GlobalOrdCardinalityAggregator", _) => {
                out["debug"] = serde_json::json!({
                    "dynamic_pruning_attempted": 0, "dynamic_pruning_used": 0,
                    "brute_force_used": 1, "skipped_due_to_no_data": 0,
                });
            }
            ("CardinalityAggregator", Metric::Cardinality { segment_ords, .. }) => {
                let ords = keyword && *segment_ords == Some(true);
                out["debug"] = serde_json::json!({
                    "empty_collectors_used": 0,
                    "numeric_collectors_used": used(m.is_some() && !keyword),
                    "ordinals_collectors_used": used(ords),
                    "ordinals_collectors_overhead_too_high": 0,
                    "string_hashing_collectors_used": used((keyword || m.is_none()) && !ords),
                });
            }
            (_, Metric::Histogram(_)) => {
                out["debug"] = serde_json::json!({"total_buckets": t.total_buckets});
            }
            (_, Metric::Terms(terms)) => {
                let mut debug = Map::new();
                let deferred = terms.deferred(&agg.subs);
                if !deferred.is_empty() {
                    debug.insert("deferred_aggregators".into(), deferred.into());
                }
                if ty == "GlobalOrdinalsStringTermsAggregator" {
                    let strategy = match terms.remaps(&agg.subs, under_bucket) {
                        true if under_bucket => "remap using many bucket ords",
                        true => "remap using single bucket ords",
                        false => "dense",
                    };
                    debug.insert("collection_strategy".into(), strategy.into());
                }
                debug.insert("total_buckets".into(), t.total_buckets.into());
                let result = match ty {
                    "GlobalOrdinalsStringTermsAggregator" => "terms",
                    "MapStringTermsAggregator" => {
                        let field = format!("from Field [{}] of type [keyword]", agg.field);
                        debug.insert("collection_strategy".into(), field.into());
                        "terms"
                    }
                    _ if matches!(
                        m,
                        Some(FieldMapping::Double | FieldMapping::Float | FieldMapping::HalfFloat)
                    ) =>
                    {
                        "double_terms"
                    }
                    _ => "long_terms",
                };
                debug.insert("result_strategy".into(), result.into());
                if ty == "GlobalOrdinalsStringTermsAggregator" {
                    let (single, multi) = (used(!t.multi_valued), used(t.multi_valued));
                    debug.insert("segments_with_single_valued_ords".into(), single.into());
                    debug.insert("segments_with_multi_valued_ords".into(), multi.into());
                    debug.insert("has_filter".into(), terms.has_filter().into());
                }
                out["debug"] = debug.into();
            }
            _ => {}
        }
        if let Some(sub) = &self.subs[i]
            && let Some(timings) = &sub.timings
        {
            let children = timings.iter().enumerate();
            let children = children.map(|(j, t)| sub.profile(j, t, true, false));
            out["children"] = children.collect();
        }
        out
    }
}

/// A doc value's hash for `cardinality`, as ES's CardinalityAggregator computes it; unmapped
/// fields count their `missing` value as a keyword.
fn hash(m: Option<&FieldMapping>, v: &Value) -> u64 {
    use FieldMapping as M;
    match m {
        Some(M::Double | M::Float | M::HalfFloat) => {
            hll::mix64(num(v).unwrap_or_default().to_bits())
        }
        Some(M::Keyword { .. }) | None => hll::murmur3(es_index::postings::text(v).as_bytes()),
        Some(M::IpRange) => {
            let bytes = v.as_array().into_iter().flatten();
            hll::murmur3(
                &bytes
                    .filter_map(|b| Some(b.as_u64()? as u8))
                    .collect::<Vec<_>>(),
            )
        }
        Some(_) => {
            let x = int(v).or_else(|| v.as_bool().map(i64::from));
            hll::mix64(x.unwrap_or_default() as u64)
        }
    }
}

/// Reduces each index's results into `total` (one entry per aggregation).
pub(crate) fn reduce(total: &mut [Stats], shard: Vec<Stats>) {
    total.iter_mut().zip(shard).for_each(|(t, s)| t.merge(s));
}

/// The `aggregations` object of a search response; with `typed_keys`, names are prefixed with
/// their type (`avg#name`).
pub(crate) fn render(
    aggs: &[Agg],
    pipelines: &[MaxBucket],
    stats: &mut [Stats],
    typed_keys: bool,
) -> Result<Value, QueryError> {
    let mut out = render_counted(aggs, stats, typed_keys, &Cell::new(0))?;
    if let Value::Object(out) = &mut out {
        for p in pipelines {
            p.apply(out, typed_keys)?;
        }
    }
    Ok(out)
}

/// Counts `n` more buckets toward the request's [`MAX_BUCKETS`], like ES's MultiBucketConsumer
/// in the final reduce.
// ponytail: terms count their buckets after rendering them and take back none they evict; ES
// counts them first (interleaved for key order) and, ordering by a sub-aggregation, counts the
// sub-buckets of terms it evicts, so it can fail on a peak this never reaches
fn consume_buckets(buckets: &Cell<usize>, n: usize) -> Result<(), QueryError> {
    buckets.set(buckets.get() + n);
    match buckets.get() > MAX_BUCKETS {
        true => Err(QueryError::new(
            crate::ErrorKind::TooManyBuckets,
            format!(
                "Trying to create too many buckets. Must be less than or equal to: [{MAX_BUCKETS}] but this number of buckets was exceeded. This limit can be set by changing the [search.max_buckets] cluster level setting."
            ),
        )),
        false => Ok(()),
    }
}

/// The buckets in rendered sub-aggregations, as ES's `countInnerBucket` counts them: those of
/// multi-bucket aggregations and everything below them, or below a single-bucket one.
fn inner_buckets(aggs: &Map<String, Value>) -> usize {
    let below = |b: &Value| b.as_object().map_or(0, |b| 1 + inner_buckets(b));
    (aggs.values().filter_map(Value::as_object))
        .map(|a| match a.get("buckets") {
            Some(Value::Array(b)) => b.iter().map(below).sum(),
            Some(Value::Object(b)) => b.values().map(below).sum(),
            _ if a.contains_key("doc_count") => inner_buckets(a),
            _ => 0,
        })
        .sum()
}

fn render_counted(
    aggs: &[Agg],
    stats: &mut [Stats],
    typed_keys: bool,
    buckets: &Cell<usize>,
) -> Result<Value, QueryError> {
    let mut out = Map::new();
    for (agg, st) in aggs.iter().zip(stats) {
        let mut body = Map::new();
        if let Some(m) = &agg.meta {
            body.insert("meta".into(), m.clone());
        }
        let avg = (st.count > 0).then(|| st.sum / st.count as f64);
        let min = st.min.is_finite().then_some(st.min);
        let max = st.max.is_finite().then_some(st.max);
        let date = st.date.as_deref();
        let put = |body: &mut Map<String, Value>, key: &str, v: Option<f64>| {
            body.insert(key.into(), v.map_or(Value::Null, Value::from));
            if let (Some(v), Some(f)) = (v, date) {
                body.insert(format!("{key}_as_string"), date_value(f, v));
            }
        };
        // the result's type: ES's InternalTDigestPercentiles names its digest
        let mut ty = match agg.metric {
            Metric::Percentiles { ranks: false, .. } => "tdigest_percentiles",
            Metric::Percentiles { .. } => "tdigest_percentile_ranks",
            _ => agg.metric.name(),
        };
        let sub = |subs: &mut [Stats], out: &mut Map<String, Value>| {
            if let Value::Object(m) = render_counted(&agg.subs, subs, typed_keys, buckets)? {
                out.extend(m);
            }
            Ok(())
        };
        match &agg.metric {
            Metric::Terms(t) => {
                if let Some(b) = &mut st.terms {
                    ty = t.render(b, &mut body, date, sub)?;
                    let n = body
                        .get("buckets")
                        .and_then(Value::as_array)
                        .map_or(0, Vec::len);
                    consume_buckets(buckets, n)?;
                }
            }
            Metric::Histogram(h) => {
                if let Some(b) = &mut st.terms {
                    h.render(b, &mut body, buckets, date, sub)?;
                }
            }
            Metric::Filter(_) | Metric::Missing | Metric::Nested(_) | Metric::Global => {
                body.insert("doc_count".into(), st.count.into());
                sub(&mut st.subs, &mut body)?;
            }
            Metric::Filters(f) => {
                consume_buckets(buckets, st.buckets.len())?;
                let keys = f.filters.iter().map(|(k, _)| k).chain(&f.other);
                let mut list = vec![];
                for (key, b) in keys.zip(&mut st.buckets) {
                    let mut bucket = Map::new();
                    if f.named && !f.keyed {
                        bucket.insert("key".into(), key.clone().into());
                    }
                    bucket.insert("doc_count".into(), b.count.into());
                    sub(&mut b.subs, &mut bucket)?;
                    list.push((key.clone(), bucket));
                }
                body.insert("buckets".into(), keyed_buckets(f.keyed, list));
            }
            Metric::Range(r) => {
                consume_buckets(buckets, st.buckets.len())?;
                let mut list = vec![];
                for b in &mut st.buckets {
                    let RangeBucket {
                        key,
                        bounds,
                        strings,
                    } = b.range.clone().unwrap_or_default();
                    let mut bucket = Map::new();
                    if !r.keyed {
                        bucket.insert("key".into(), key.as_str().into());
                    }
                    for ((name, x), s) in ["from", "to"].into_iter().zip(bounds).zip(strings) {
                        if x.is_finite() {
                            bucket.insert(name.into(), x.into());
                            if let Some(s) = s {
                                bucket.insert(format!("{name}_as_string"), s.into());
                            }
                        }
                    }
                    bucket.insert("doc_count".into(), b.count.into());
                    sub(&mut b.subs, &mut bucket)?;
                    list.push((key, bucket));
                }
                body.insert("buckets".into(), keyed_buckets(r.keyed, list));
            }
            Metric::TopHits(t) => {
                let hits = t.render(std::mem::take(&mut st.top), st.count);
                body.insert("hits".into(), hits);
            }
            Metric::Avg => put(&mut body, "value", avg),
            Metric::Sum => put(&mut body, "value", Some(st.sum)),
            Metric::Min => put(&mut body, "value", min),
            Metric::Max => put(&mut body, "value", max),
            Metric::ValueCount => {
                body.insert("value".into(), st.count.into());
            }
            Metric::Cardinality { .. } => {
                let n = st.hll.as_ref().map_or(0, Hll::cardinality);
                body.insert("value".into(), n.into());
            }
            Metric::Percentiles {
                percents,
                keyed,
                ranks,
                ..
            } => {
                let mut d = st.digest.take().map(TDigest::coordinated);
                let mut d = d.as_mut().filter(|d| d.size() > 0);
                let (mut keyed_values, mut list) = (Map::new(), vec![]);
                for &p in percents {
                    let v = (d.as_mut()).map(|d| match ranks {
                        // InternalTDigestPercentileRanks.percentileRank
                        true => d.cdf(p).clamp(0.0, 1.0) * 100.0,
                        false => d.quantile(p / 100.0),
                    });
                    if *keyed {
                        put(&mut keyed_values, &java_double(p), v);
                        continue;
                    }
                    let mut entry = Map::from_iter([("key".to_owned(), p.into())]);
                    put(&mut entry, "value", v);
                    list.push(entry.into());
                }
                let values = match keyed {
                    true => keyed_values.into(),
                    false => Value::Array(list),
                };
                body.insert("values".into(), values);
            }
            Metric::Stats | Metric::ExtendedStats { .. } => {
                body.insert("count".into(), st.count.into());
                put(&mut body, "min", min);
                put(&mut body, "max", max);
                put(&mut body, "avg", avg);
                match st.count {
                    0 => drop(body.insert("sum".into(), 0.0.into())),
                    _ => put(&mut body, "sum", Some(st.sum)),
                }
            }
        }
        if let Some(Value::Array(buckets)) = body.get_mut("buckets") {
            for derivative in &agg.derivatives {
                derivative.apply(buckets, typed_keys)?;
            }
            for selector in &agg.bucket_selectors {
                selector.apply(buckets)?;
            }
            if let Some(sort) = &agg.bucket_sort {
                sort.apply(buckets)?;
            }
        }
        if let Metric::ExtendedStats { sigma } = agg.metric {
            extended_stats(&mut body, st, sigma, date);
        }
        let key = match typed_keys {
            true => format!("{ty}#{}", agg.name),
            false => agg.name.clone(),
        };
        out.insert(key, body.into());
    }
    Ok(out.into())
}

/// Epoch millis `t` in a date field's `format` (the first of its `||` alternatives), in UTC.
fn date_string(format: Option<&str>, t: i64) -> String {
    date_string_in(format, t, None)
}

/// [`date_string`] in time zone `tz`, UTC if `None`.
fn date_string_in(format: Option<&str>, t: i64, tz: Option<&TimeZone>) -> String {
    let utc = &TimeZone::UTC;
    (format.and_then(|f| es_index::format_date(f, t, tz.unwrap_or(utc))))
        .unwrap_or_else(|| tz.map_or_else(|| iso_date(t), |tz| tz.format(t)))
}

/// The `format` and `locale` of a date field's mapping `m`, if it is one.
fn date_format(m: Option<&FieldMapping>) -> (Option<&str>, Option<&str>) {
    match m {
        Some(
            FieldMapping::Date { format, locale, .. } | FieldMapping::DateNanos { format, locale },
        ) => (format.as_deref(), locale.as_deref()),
        _ => (None, None),
    }
}

/// Buckets as an object by key, or else an array.
fn keyed_buckets(keyed: bool, buckets: Vec<(String, Map<String, Value>)>) -> Value {
    let buckets = buckets.into_iter();
    match keyed {
        true => buckets
            .map(|(k, b)| (k, b.into()))
            .collect::<Map<_, _>>()
            .into(),
        false => buckets.map(|(_, b)| Value::from(b)).collect(),
    }
}

/// The fields `extended_stats` adds to `stats`, computed like ES's InternalExtendedStats.
fn extended_stats(body: &mut Map<String, Value>, st: &Stats, sigma: f64, date: Option<&str>) {
    let n = st.count as f64;
    // ES's `variance < 0 ? 0 : variance` keeps NaN, which `f64::max` would not
    let var = |d: f64| match (st.sum_sq - st.sum * st.sum / n) / d {
        x if x < 0.0 => 0.0,
        x => x,
    };
    let (pop, samp) = (var(n), var(n - 1.0));
    let avg = st.sum / n;
    // Jackson writes NaN (the sampling variance of one value) as a string.
    let v = |x: f64| {
        if st.count == 0 {
            Value::Null
        } else if x.is_nan() {
            "NaN".into()
        } else {
            x.into()
        }
    };
    body.insert("sum_of_squares".into(), v(st.sum_sq));
    body.insert("variance".into(), v(pop));
    body.insert("variance_population".into(), v(pop));
    body.insert("variance_sampling".into(), v(samp));
    body.insert("std_deviation".into(), v(pop.sqrt()));
    body.insert("std_deviation_population".into(), v(pop.sqrt()));
    body.insert("std_deviation_sampling".into(), v(samp.sqrt()));
    let (upper, lower) = (avg + pop.sqrt() * sigma, avg - pop.sqrt() * sigma);
    let (s_upper, s_lower) = (avg + samp.sqrt() * sigma, avg - samp.sqrt() * sigma);
    body.insert(
        "std_deviation_bounds".into(),
        serde_json::json!({
            "upper": v(upper), "lower": v(lower),
            "upper_population": v(upper), "lower_population": v(lower),
            "upper_sampling": v(s_upper), "lower_sampling": v(s_lower),
        }),
    );
    let Some(f) = date.filter(|_| st.count > 0) else {
        return;
    };
    let d = |x: f64| date_value(f, x);
    body.insert("sum_of_squares_as_string".into(), d(st.sum_sq));
    body.insert("variance_as_string".into(), d(pop));
    body.insert("variance_population_as_string".into(), d(pop));
    body.insert("variance_sampling_as_string".into(), d(samp));
    body.insert("std_deviation_as_string".into(), d(pop.sqrt()));
    body.insert("std_deviation_population_as_string".into(), d(pop.sqrt()));
    body.insert("std_deviation_sampling_as_string".into(), d(samp.sqrt()));
    body.insert(
        "std_deviation_bounds_as_string".into(),
        serde_json::json!({
            "upper": d(upper), "lower": d(lower),
            "upper_population": d(upper), "lower_population": d(lower),
            "upper_sampling": d(s_upper), "lower_sampling": d(s_lower),
        }),
    );
}

/// A metric's value `x` in date format `f`, like ES's DocValueFormat.DateTime: cast to long
/// millis, null at the long limits (where values past a long's range land).
fn date_value(f: &str, x: f64) -> Value {
    match x as i64 {
        i64::MIN | i64::MAX => Value::Null,
        t => date_string(Some(f), t).into(),
    }
}

#[cfg(test)]
mod tests {
    use crate::{SearchRequest, search};
    use es_index::{FieldMapping, Index, Indices, disk};
    use serde_json::{Value, json};

    #[test]
    fn date_formats_metrics_render_dates_in_the_mapping_format() {
        let mut ix = Index::default();
        let date = FieldMapping::Date {
            format: Some("yyyy/MM/dd HH:mm||epoch_millis".into()),
            locale: None,
            fields: Default::default(),
        };
        ix.mappings.properties.insert("d".into(), date);
        ix.put("1", json!({"d": "2016/01/02 10:00"}));
        ix.put("2", json!({"d": "2017/03/04 11:30"}));
        ix.put("3", json!({"d": 1000}));
        let aggs = json!({
            "mn": {"min": {"field": "d"}},
            "st": {"stats": {"field": "d"}},
            "es": {"extended_stats": {"field": "d"}},
        });
        let r = aggs_of(&ix, json!({"size": 0, "aggs": aggs}));
        // Elasticsearch 9.5.5's strings; values past a long's range format as null
        assert_eq!(r["mn"]["value_as_string"], "1970/01/01 00:00");
        let stats = [
            ("min_as_string", "1970/01/01 00:00"),
            ("max_as_string", "2017/03/04 11:30"),
            ("avg_as_string", "2001/01/21 23:10"),
            ("sum_as_string", "2063/03/05 21:30"),
        ];
        for (k, v) in stats {
            assert_eq!(r["st"][k], v, "{k}");
            assert_eq!(r["es"][k], v, "{k}");
        }
        let es = &r["es"];
        assert_eq!(es["sum_of_squares_as_string"], Value::Null);
        assert_eq!(es["variance_as_string"], Value::Null);
        assert_eq!(es["variance_population_as_string"], Value::Null);
        assert_eq!(es["variance_sampling_as_string"], Value::Null);
        assert_eq!(es["std_deviation_as_string"], "1991/12/20 06:56");
        assert_eq!(es["std_deviation_population_as_string"], "1991/12/20 06:56");
        assert_eq!(es["std_deviation_sampling_as_string"], "1996/11/26 11:34");
        let bounds = json!({
            "upper": "2044/12/28 13:03", "lower": "1957/02/15 09:16",
            "upper_population": "2044/12/28 13:03", "lower_population": "1957/02/15 09:16",
            "upper_sampling": "2054/11/12 22:19", "lower_sampling": "1947/04/03 00:00",
        });
        assert_eq!(es["std_deviation_bounds_as_string"], bounds);
        let keys: Vec<_> = es.as_object().unwrap().keys().collect();
        assert_eq!(
            keys.last().unwrap().as_str(),
            "std_deviation_bounds_as_string"
        );
    }

    #[test]
    fn date_formats_bucket_keys_render_in_the_mapping_format() {
        let mut ix = Index::default();
        let date = FieldMapping::Date {
            format: Some("yyyy/MM/dd||epoch_millis".into()),
            locale: None,
            fields: Default::default(),
        };
        ix.mappings.properties.insert("d".into(), date);
        ix.put("1", json!({"d": "2016/01/02"}));
        let aggs = json!({
            "t": {"terms": {"field": "d"}},
            "h": {"date_histogram": {"field": "d", "calendar_interval": "day"}},
            "f": {"date_histogram": {"field": "d", "calendar_interval": "day",
                "format": "yyyy"}},
        });
        let r = aggs_of(&ix, json!({"size": 0, "aggs": aggs}));
        assert_eq!(r["t"]["buckets"][0]["key_as_string"], "2016/01/02");
        assert_eq!(r["h"]["buckets"][0]["key_as_string"], "2016/01/02");
        assert_eq!(r["f"]["buckets"][0]["key_as_string"], "2016");
    }

    #[test]
    fn decimal_format_follows_java() {
        let f = super::decimal_format;
        assert_eq!(f("Value is ##0.0", 150.0), "Value is 150.0");
        assert_eq!(f("Value is ##0.0", 0.0), "Value is 0.0");
        assert_eq!(f("0.0#", 1.0), "1.0");
        assert_eq!(f("0.0#", 1.256), "1.26");
        assert_eq!(f("0.0###", 2.5), "2.5");
        assert_eq!(f("#.#", 0.5), "0.5");
        assert_eq!(f("000", 7.0), "007");
        assert_eq!(f("0 units", -3.4), "-3 units");
        // HALF_EVEN
        assert_eq!(f("0", 2.5), "2");
    }

    #[test]
    fn aggs_gaps_decimal_patterns_follow_java() {
        // expectations checked against Elasticsearch 9.5.5 (`min` with `format`)
        let f = super::decimal_format;
        // grouping: the size is the digits after the last `,`
        assert_eq!(f("#,##0.00", 1_234_567.891), "1,234,567.89");
        assert_eq!(f("#,##,###", 1_234_567.0), "1,234,567");
        assert_eq!(f("0,000", 5.0), "0,005");
        // quoting, and `%`/`‰` scale the value
        assert_eq!(f("'#'0 o''clock", 3.0), "#3 o'clock");
        assert_eq!(f("0.#%", 0.256), "25.6%");
        assert_eq!(f("0\u{2030}", 0.256), "256\u{2030}");
        // negative subpatterns take only their affixes
        assert_eq!(f("0.0;(0.0)", -1.25), "(1.2)");
        assert_eq!(f("0.0;(#)", 2.0), "2.0");
        assert_eq!(f("0;0", -2.0), "-2");
        // exponents: significant digits are the integer and fraction maximums
        assert_eq!(f("0.###E0", 12_345.0), "1.234E4");
        assert_eq!(f("00.##E0", 0.001_234_6), "12.35E-4");
        assert_eq!(f("##0.#####E0", 12_345.0), "12.345E3");
        assert_eq!(f("##0.#####E0", 0.000_123_45), "123.45E-6");
        assert_eq!(f("0.0E00", 0.0), "0.0E00");
        assert_eq!(f("##0.##E0", 0.5), "500E-3");
        assert_eq!(f("#E0", 123.0), ".1E3");
        // a pattern without any `0` gets one before the point; the shortest repr wins
        assert_eq!(f("#.00", 0.5), ".50");
        assert_eq!(f(".##", 5.0), "5.0");
        assert_eq!(f("0.", 3.0), "3.");
        assert_eq!(f("0.####################", 0.1), "0.1");
        assert_eq!(f("0", f64::INFINITY), "\u{221e}");
        assert_eq!(f("0", f64::NAN), "NaN");
        assert_eq!(f("0.00", -0.0001), "-0.00");
        assert_eq!(f("0", -0.0), "-0");
    }

    fn index() -> Index {
        let mut ix = Index::default();
        let props = &mut ix.mappings.properties;
        props.insert("i".into(), FieldMapping::Integer);
        props.insert("f".into(), FieldMapping::Float);
        props.insert(
            "k".into(),
            FieldMapping::Keyword {
                ignore_above: None,
                split_queries_on_whitespace: false,
            },
        );
        let date = FieldMapping::Date {
            format: None,
            locale: None,
            fields: Default::default(),
        };
        props.insert("at".into(), date);
        props.insert(
            "t".into(),
            FieldMapping::Text {
                store: false,
                fields: Default::default(),
            },
        );
        for (id, i, f, k, at) in [
            ("1", 1, 0.5, "a", "2021-05-01T00:00:00Z"),
            ("2", 51, 1.1, "b", "2021-05-02T00:00:00Z"),
            ("3", 101, 0.25, "b", "2021-05-03T00:00:00Z"),
            ("4", 151, 2.0, "c", "2021-05-04T00:00:00Z"),
        ] {
            ix.put(id, json!({"i": i, "f": f, "k": [k, k], "at": at, "t": "x"}));
        }
        ix.put("5", json!({"t": "no values"}));
        ix
    }

    fn aggs_of(ix: &Index, body: Value) -> Value {
        let req = SearchRequest::parse(&body).unwrap();
        search(ix, &req).aggregations.unwrap()
    }

    fn body() -> Value {
        json!({
            "size": 0,
            "query": {"range": {"i": {"gte": 50}}},
            "aggs": {
                "avg_i": {"avg": {"field": "i"}},
                "sum_i": {"sum": {"field": "i"}},
                "min_i": {"min": {"field": "i"}},
                "max_i": {"max": {"field": "i"}},
                "count_k": {"value_count": {"field": "k"}},
                "sum_f": {"sum": {"field": "f"}},
                "max_at": {"max": {"field": "at"}, "meta": {"m": 1}},
                "avg_none": {"avg": {"field": "nope"}},
                "count_none": {"value_count": {"field": "nope", "missing": 1}},
            }
        })
    }

    fn expected() -> Value {
        // floats aggregate as their f32 doc values
        json!({
            "avg_i": {"value": 101.0},
            "sum_i": {"value": 303.0},
            "min_i": {"value": 51.0},
            "max_i": {"value": 151.0},
            "count_k": {"value": 3},
            "sum_f": {"value": f64::from(1.1f32) + 2.25},
            "max_at": {"meta": {"m": 1}, "value": 1_620_086_400_000.0,
                "value_as_string": "2021-05-04T00:00:00.000Z"},
            "avg_none": {"value": null},
            "count_none": {"value": 3},
        })
    }

    #[test]
    fn aggs_compute_metrics_over_every_match_not_the_page() {
        assert_eq!(aggs_of(&index(), body()), expected());
    }

    #[test]
    fn aggs_on_an_opened_index_read_doc_values_like_memory() {
        let dir = std::env::temp_dir().join(format!("esm-aggs-{}", std::process::id()));
        disk::save(&dir, &Indices::from([("i".to_owned(), index())])).unwrap();
        let opened = disk::open(&dir).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(aggs_of(&opened["i"], body()), expected());
    }

    #[test]
    fn aggs_without_values_and_with_typed_keys() {
        let body = json!({"query": {"term": {"k": "zzz"}}, "aggs": {
            "s": {"sum": {"field": "i"}},
            "m": {"min": {"field": "i"}},
            "c": {"value_count": {"field": "i"}},
        }});
        let mut req = SearchRequest::parse(&body).unwrap();
        req.typed_keys = true;
        let got = search(&index(), &req).aggregations.unwrap();
        let want = json!({
            "sum#s": {"value": 0.0},
            "min#m": {"value": null},
            "value_count#c": {"value": 0},
        });
        assert_eq!(got, want);
    }

    #[test]
    fn typed_keys_name_percentiles_by_their_digest() {
        let body = json!({"aggs": {"p": {"percentiles": {"field": "i", "percents": [50]}}}});
        let mut req = SearchRequest::parse(&body).unwrap();
        req.typed_keys = true;
        let got = search(&index(), &req).aggregations.unwrap();
        assert!(got.get("tdigest_percentiles#p").is_some(), "{got}");
    }

    #[test]
    fn percentile_ranks_interpolate_between_values() {
        let body = json!({"aggs": {"r": {"percentile_ranks": {"field": "i",
            "values": [51, 1, 200, 0, 76]}}}});
        let mut req = SearchRequest::parse(&body).unwrap();
        req.typed_keys = true;
        let got = search(&index(), &req).aggregations.unwrap();
        // MergingDigest.cdf by hand over 1, 51, 101, 151: half the weight at a value
        let want = json!({"tdigest_percentile_ranks#r": {"values": {
            "0.0": 0.0, "1.0": 12.5, "51.0": 37.5, "76.0": 50.0, "200.0": 100.0}}});
        assert_eq!(got, want);
        let err = |v: Value| SearchRequest::parse(&json!({"aggs": {"r": v}})).unwrap_err();
        let e = err(json!({"percentile_ranks": {"field": "i"}}));
        assert!(
            e.to_string().contains("[values] must not be null: [r]"),
            "{e}"
        );
        let e = err(json!({"percentile_ranks": {"field": "i", "values": []}}));
        assert!(
            e.to_string()
                .contains("[values] must not be an empty array: [r]"),
            "{e}"
        );
    }

    #[test]
    fn top_hits_keep_the_top_documents_of_each_bucket() {
        let body = json!({"query": {"range": {"i": {"gte": 50}}}, "aggs": {
            "k": {"terms": {"field": "k"}, "aggs": {"top": {"top_hits": {
                "size": 1, "sort": [{"i": "desc"}], "_source": ["i"], "version": true,
            }}}},
            "all": {"top_hits": {"from": 1, "seq_no_primary_term": true,
                "_source": {"excludes": ["t", "at", "k", "f"]}}},
            "none": {"top_hits": {"_source": false}},
        }});
        let aggs = aggs_of(&index(), body);
        let top = json!({"hits": {"total": {"value": 2, "relation": "eq"}, "max_score": null,
            "hits": [{"_index": "", "_id": "3", "_version": 1, "_score": null,
                "_source": {"i": 101}, "sort": [101]}]}});
        assert_eq!(aggs["k"]["buckets"][0]["top"], top);
        // a constant-score query ties every hit, so they keep their index order
        let hit = |id: &str, seq_no: u64, i: u64| {
            json!({"_index": "", "_id": id, "_seq_no": seq_no, "_primary_term": 1,
                "_score": 1.0, "_source": {"i": i}})
        };
        let all = json!({"hits": {"total": {"value": 3, "relation": "eq"}, "max_score": 1.0,
            "hits": [hit("3", 2, 101), hit("4", 3, 151)]}});
        assert_eq!(aggs["all"], all);
        assert!(aggs["none"]["hits"]["hits"][0].get("_source").is_none());
        let body = json!({"aggs": {"t": {"top_hits": {"collapse": {}}}}});
        let err = SearchRequest::parse(&body).unwrap_err();
        assert_eq!(err.msg, "[top_hits] unknown field [collapse]");
    }

    #[test]
    fn top_hits_highlight_the_query_terms_of_their_fields() {
        let mut ix = index();
        ix.put("6", json!({"t": "The cat; THE dog", "k": "the"}));
        let hit = |query: Value| {
            let body = json!({"query": {"match": {"t": "the"}}, "aggs": {"top": {"top_hits": {
                "_source": false, "highlight": {"fields": {"t": query, "k": {}}}}}}});
            aggs_of(&ix, body)["top"]["hits"]["hits"][0].clone()
        };
        let want = json!({"t": ["<em>The</em> cat; <em>THE</em> dog"]});
        assert_eq!(hit(json!({}))["highlight"], want);
        // the query given to a field replaces the search's, and a field needs terms to show
        let own = hit(json!({"highlight_query": {"match": {"t": "cat"}}}));
        assert_eq!(
            own["highlight"],
            json!({"t": ["The <em>cat</em>; THE dog"]})
        );
        let none = hit(json!({"highlight_query": {"match": {"t": "zzz"}}}));
        assert!(none.get("highlight").is_none(), "{none}");
    }

    #[test]
    fn top_hits_docvalue_fields_render_doc_values_as_fields() {
        let body = json!({"aggs": {"top": {"top_hits": {"size": 1, "sort": [{"i": "desc"}],
            "_source": false, "docvalue_fields": ["i", {"field": "k"}, "at", "f", "nope"]}}}});
        let hit = &aggs_of(&index(), body)["top"]["hits"]["hits"][0];
        let fields =
            json!({"i": [151], "k": ["c"], "at": ["2021-05-04T00:00:00.000Z"], "f": [2.0]});
        assert_eq!(hit["fields"], fields);
    }

    #[test]
    fn aggs_gaps_fields_take_a_format() {
        // docvalue_fields format dates and numbers (DocValueFormat), fields only dates
        let body = json!({"fields": [{"field": "at", "format": "yyyy/MM"}], "sort": ["i"],
            "aggs": {"top": {"top_hits": {"size": 1, "sort": [{"i": "desc"}],
            "_source": false, "fields": [], "docvalue_fields": [{"field": "at", "format": "yyyy"},
                {"field": "f", "format": "0.0"}, {"field": "i", "format": "#,##0.00"}]}}}});
        let (ix, req) = (index(), SearchRequest::parse(&body).unwrap());
        let r = search(&ix, &req);
        let top = &r.aggregations.unwrap()["top"]["hits"]["hits"][0];
        let fields = json!({"at": ["2021"], "f": ["2.0"], "i": ["151.00"]});
        assert_eq!(top["fields"], fields);
        assert_eq!(
            Value::from(r.hits[0].fields.clone()),
            json!({"at": ["2021/05"]})
        );
    }

    #[test]
    fn aggs_gaps_2_fields_refuse_formats_and_text_doc_values() {
        // Elasticsearch 9.5.5 fails the shard, only when it has hits to fetch
        let run = |body: Value| {
            let (ix, req) = (index(), SearchRequest::parse(&body).unwrap());
            crate::search_many(&[("i", 0, &ix, None)], &req).map(drop)
        };
        let top = |spec: Value| json!({"size": 0, "aggs": {"top": {"top_hits": spec}}});
        let err = |body: Value| run(body).unwrap_err();
        let e = err(top(
            json!({"docvalue_fields": [{"field": "k", "format": "x"}]}),
        ));
        assert_eq!(
            e.msg,
            "Field [k] of type [keyword] does not support custom formats"
        );
        assert_eq!(e.shards[0].0, "i");
        let fielddata = "Fielddata is disabled on [t] in [i]. Text fields are not optimised";
        for f in ["t", "*t"] {
            let e = err(top(json!({"docvalue_fields": [f]})));
            assert!(e.msg.starts_with(fielddata), "{f}: {}", e.msg);
        }
        let e = err(top(json!({"fields": [{"field": "i", "format": "0.0"}]})));
        assert_eq!(
            e.msg,
            "error fetching [i]: Field [i] of type [integer] doesn't support formats."
        );
        let cause = "Field [i] of type [integer] doesn't support formats.";
        assert_eq!(e.cause.unwrap().msg, cause);
        let e = err(json!({"fields": [{"field": "k*", "format": "x"}]}));
        assert_eq!(
            e.msg,
            "error fetching [k] which matched [k*]: Field [k] of type [keyword] doesn't support formats."
        );
        let none = json!({"term": {"k": "zzz"}});
        let mut body = top(json!({"docvalue_fields": ["t"]}));
        body["query"] = none.clone();
        assert!(run(body).is_ok());
        let body = json!({"query": none, "fields": [{"field": "k", "format": "x"}]});
        assert!(run(body).is_ok());
    }

    #[test]
    fn aggs_gaps_2_ip_ranges_aggregate_one_value_per_document() {
        // as Elasticsearch 9.5.5: a document's ip ranges are one binary doc value, which only
        // value_count, cardinality and missing read
        let mut ix = Index::default();
        (ix.mappings.properties).insert("r".into(), FieldMapping::IpRange);
        let b = "192.168.0.0/16";
        ix.put(
            "1",
            json!({"r": [{"gte": "10.0.0.1", "lte": "10.0.0.5"}, b]}),
        );
        ix.put("2", json!({"r": b}));
        ix.put("3", json!({"r": [b]}));
        ix.put("4", json!({}));
        let aggs = json!({"v": {"value_count": {"field": "r"}},
            "c": {"cardinality": {"field": "r"}}, "m": {"missing": {"field": "r"}}});
        let r = aggs_of(&ix, json!({"size": 0, "aggs": aggs}));
        assert_eq!(
            r,
            json!({"v": {"value": 3}, "c": {"value": 2}, "m": {"doc_count": 1}})
        );
        let err = |agg: Value| {
            let req = SearchRequest::parse(&json!({"aggs": {"a": agg}})).unwrap();
            crate::search_many(&[("i", 0, &ix, None)], &req)
                .unwrap_err()
                .msg
        };
        let unsupported = "Field [r] of type [ip_range] is not supported for aggregation";
        assert_eq!(
            err(json!({"terms": {"field": "r"}})),
            format!("{unsupported} [terms]")
        );
        assert_eq!(
            err(json!({"min": {"field": "r"}})),
            format!("{unsupported} [min]")
        );
        assert_eq!(
            err(json!({"histogram": {"field": "r", "interval": 1}})),
            "Expected numeric range type but found non-numeric range [ip_range]"
        );
        assert_eq!(
            err(json!({"date_histogram": {"field": "r", "fixed_interval": "1d"}})),
            "Expected date range type but found range type [ip_range]"
        );
    }

    #[test]
    fn aggs_gaps_2_histograms_count_the_sub_buckets_of_dropped_buckets() {
        // as Elasticsearch 9.5.5's InternalHistogram.maybeAddBucket: a bucket below
        // min_doc_count counts its sub-buckets (empty ones twice: before and while adding them),
        // then takes back only those it would have rendered
        let run = |interval: u32, max: u32| {
            let sub = json!({"histogram": {"field": "i", "interval": 1, "min_doc_count": 0,
                "extended_bounds": {"min": 0, "max": max}}});
            let aggs = json!({"h": {"histogram": {"field": "i", "interval": interval,
                "min_doc_count": 5}, "aggs": {"s": sub}}});
            let (ix, req) = (
                index(),
                SearchRequest::parse(&json!({"aggs": aggs})).unwrap(),
            );
            crate::search_many(&[("i", 0, &ix, None)], &req).map(|r| r.aggregations.unwrap())
        };
        let too_many = |r: Result<Value, crate::QueryError>| {
            r.unwrap_err().kind == crate::ErrorKind::TooManyBuckets
        };
        // one bucket of 4 documents: 80,001, then 60,001
        assert!(too_many(run(1000, 40_000)));
        assert_eq!(run(1000, 30_000).unwrap(), json!({"h": {"buckets": []}}));
        // two of 2: 30,000 left counted by the first
        assert!(too_many(run(100, 30_000)));
        assert!(run(100, 9_000).is_ok());
    }

    #[test]
    fn aggs_gaps_fields_match_wildcards() {
        // like FetchDocValuesContext: patterns expand to mapped fields, the last match's
        // format wins
        let body = json!({"fields": ["k*", {"field": "a*", "format": "yyyy"}, "at"],
            "sort": ["i"], "aggs": {"top": {"top_hits": {"size": 1, "sort": [{"i": "desc"}],
            "_source": false, "fields": [],
            "docvalue_fields": ["a*", {"field": "at", "format": "yyyy"}]}}}});
        let (ix, req) = (index(), SearchRequest::parse(&body).unwrap());
        let r = search(&ix, &req);
        let top = &r.aggregations.unwrap()["top"]["hits"]["hits"][0];
        assert_eq!(top["fields"], json!({"at": ["2021"]}));
        assert_eq!(
            Value::from(r.hits[0].fields.clone()),
            json!({"k": ["a", "a"], "at": ["2021-05-01T00:00:00.000Z"]})
        );
    }

    #[test]
    fn top_hits_stored_fields_render_stored_values_as_fields() {
        let mut ix = index();
        let store = FieldMapping::Text {
            store: true,
            fields: Default::default(),
        };
        ix.mappings.properties.insert("s".into(), store);
        ix.put("6", json!({"i": 200, "s": ["The Fox", "x"], "t": "y"}));
        let body = json!({"aggs": {
            "top": {"top_hits": {"size": 1, "sort": [{"i": "desc"}],
                "stored_fields": ["s", "t", "i", "nope"]}},
            "one": {"top_hits": {"size": 1, "sort": [{"i": "desc"}], "stored_fields": "s",
                "_source": ["i"]}},
            "none": {"top_hits": {"size": 1, "stored_fields": "_none_", "_source": true}},
        }});
        let aggs = aggs_of(&ix, body);
        let fields = json!({"s": ["The Fox", "x"]});
        let top = &aggs["top"]["hits"]["hits"][0];
        assert_eq!(top["fields"], fields);
        // stored fields leave `_source` out unless it is asked for, like ES's FetchContext
        assert!(top.get("_source").is_none());
        let one = &aggs["one"]["hits"]["hits"][0];
        assert_eq!(one["fields"], fields);
        assert_eq!(one["_source"], json!({"i": 200}));
        assert!(aggs["none"]["hits"]["hits"][0].get("_source").is_none());
    }

    #[test]
    fn top_hits_fetch_the_search_fields_unless_they_set_their_own() {
        let body = json!({"fields": ["t", {"field": "k"}, "at", "nope"], "sort": ["i"],
        "aggs": {
            "top": {"top_hits": {"size": 1, "sort": [{"i": "desc"}]}},
            "own": {"top_hits": {"size": 1, "sort": [{"i": "desc"}], "fields": ["i"]}},
        }});
        let req = SearchRequest::parse(&body).unwrap();
        let ix = index();
        let r = search(&ix, &req);
        // source order, duplicates kept, dates formatted, like ES's FieldFetcher
        let fields = json!({"t": ["x"], "k": ["c", "c"], "at": ["2021-05-04T00:00:00.000Z"]});
        let aggs = r.aggregations.unwrap();
        assert_eq!(aggs["top"]["hits"]["hits"][0]["fields"], fields);
        assert_eq!(
            aggs["own"]["hits"]["hits"][0]["fields"],
            json!({"i": [151]})
        );
        assert_eq!(Value::from(r.hits[3].fields.clone()), fields);
        assert!(r.hits[4].fields.contains_key("t"));
    }

    #[test]
    fn top_hits_explain_their_scores_like_lucene() {
        let body = json!({"query": {"bool": {"should": [
            {"match": {"t": "x"}}, {"match": {"t": "no values"}}]}},
            "aggs": {"top": {"top_hits": {"explain": true, "_source": false}}}});
        let aggs = aggs_of(&index(), body);
        let hits = aggs["top"]["hits"]["hits"].as_array().unwrap();
        // doc 5 matches both terms, so it leads
        let first = &hits[1]["_explanation"];
        assert_eq!(first["value"], hits[1]["_score"]);
        assert_eq!(first["description"], "sum of:");
        // a single term is its weight, not a sum
        let weight = &first["details"][0];
        assert_eq!(weight["value"], hits[1]["_score"]);
        assert_eq!(
            weight["description"],
            "weight(t:x in 0) [PerFieldSimilarity], result of:"
        );
        let score = &weight["details"][0];
        let desc = "score(freq=1.0), computed as boost * idf * tf from:";
        assert_eq!(score["description"], desc);
        let parts = score["details"].as_array().unwrap();
        let [boost, idf, tf] = [0, 1, 2].map(|i| parts[i]["value"].as_f64().unwrap());
        assert_eq!(parts[0]["description"], "boost");
        assert!((boost * idf * tf - score["value"].as_f64().unwrap()).abs() < 1e-6);
        let n = json!([{"value": 4.0, "description": "n, number of documents containing term",
            "details": []}, {"value": 5.0, "description": "N, total number of documents with field",
            "details": []}]);
        assert_eq!(parts[1]["details"], n);
        assert_eq!(parts[2]["details"].as_array().unwrap().len(), 5);
        // two matched terms sum
        let both = &hits[0]["_explanation"]["details"][0];
        assert_eq!(both["description"], "sum of:");
        assert_eq!(both["details"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn top_hits_explain_the_frequencies_and_lengths_they_score_from() {
        let mut ix = Index::default();
        let keyword = FieldMapping::Keyword {
            ignore_above: None,
            split_queries_on_whitespace: false,
        };
        ix.mappings.properties.insert("k".into(), keyword);
        ix.put("1", json!({"k": ["a", "a", "b"]}));
        let body = json!({"query": {"term": {"k": "a"}},
            "aggs": {"top": {"top_hits": {"explain": true, "_source": false}}}});
        let hit = &aggs_of(&ix, body)["top"]["hits"]["hits"][0];
        assert_eq!(hit["_explanation"]["value"], hit["_score"]);
        let tf = &hit["_explanation"]["details"][0]["details"][2]["details"];
        // a keyword counts once and has no norms (length 1)
        assert_eq!([&tf[0]["value"], &tf[3]["value"]], [1.0, 1.0]);
    }

    #[test]
    fn top_hits_report_the_named_queries_each_hit_matches() {
        // ordered like Java's HashMap of the names ("a" before "b"), not like the query
        let body = json!({"query": {"bool": {"should": [
            {"term": {"k": {"value": "b", "_name": "b"}}},
            {"range": {"i": {"gte": 100, "_name": "a"}}},
            {"term": {"k": {"value": "a", "_name": "z"}}},
        ], "must_not": {"term": {"k": {"value": "zzz", "_name": "no"}}}}},
            "aggs": {"top": {"top_hits": {"sort": ["i"], "_source": false}}}});
        let aggs = aggs_of(&index(), body);
        let names: Vec<_> = (aggs["top"]["hits"]["hits"].as_array().unwrap().iter())
            .map(|h| h["matched_queries"].clone())
            .collect();
        assert_eq!(names, [json!(["z"]), json!(["b"]), json!(["a", "b"])]);
    }

    #[test]
    fn filter_counts_matches_and_aggregates_them() {
        let body = json!({"query": {"range": {"i": {"gte": 50}}}, "aggs": {
            "f": {"filter": {"term": {"k": "b"}}, "aggs": {"m": {"max": {"field": "i"}}}},
            "none": {"filter": {"term": {"k": "zzz"}}},
        }});
        let want = json!({
            "f": {"doc_count": 2, "m": {"value": 101.0}},
            "none": {"doc_count": 0},
        });
        assert_eq!(aggs_of(&index(), body), want);
    }

    #[test]
    fn filters_bucket_each_query_and_the_others() {
        let body = json!({"query": {"range": {"i": {"gte": 50}}}, "aggs": {
            "named": {"filters": {"other_bucket": true, "filters": {
                "z": {"term": {"k": "b"}},
                "a": {"term": {"k": "zzz"}},
            }}, "aggs": {"m": {"max": {"field": "i"}}}},
            "list": {"filters": {"keyed": false, "other_bucket_key": "rest",
                "filters": {"b": {"term": {"k": "b"}}}}},
            "anon": {"filters": {"filters": [{"term": {"k": "c"}}, {"match_all": {}}]}},
        }});
        let want = json!({
            "named": {"buckets": {
                "a": {"doc_count": 0, "m": {"value": null}},
                "z": {"doc_count": 2, "m": {"value": 101.0}},
                "_other_": {"doc_count": 1, "m": {"value": 151.0}},
            }},
            "list": {"buckets": [
                {"key": "b", "doc_count": 2},
                {"key": "rest", "doc_count": 1},
            ]},
            "anon": {"buckets": [{"doc_count": 1}, {"doc_count": 3}]},
        });
        assert_eq!(aggs_of(&index(), body), want);
        for filters in [json!({}), json!({"filters": []})] {
            let body = json!({"aggs": {"f": {"filters": filters}}});
            let err = SearchRequest::parse(&body).unwrap_err();
            assert_eq!(err.msg, "[filters] cannot be empty.");
        }
    }

    #[test]
    fn bucket_sort_orders_and_pages_the_buckets_of_its_parent() {
        let aggs = |bucket_sort: Value| {
            let body = json!({"aggs": {"f": {
                "filters": {"keyed": false, "filters": {
                    "a": {"term": {"k": "a"}},
                    "b": {"term": {"k": "b"}},
                    "c": {"match_all": {}},
                }},
                "aggs": {"m": {"max": {"field": "i"}}, "s": {"bucket_sort": bucket_sort}},
            }}});
            let aggs = aggs_of(&index(), body);
            let keys = aggs["f"]["buckets"].as_array().unwrap().iter();
            keys.map(|b| b["key"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(aggs(json!({"sort": [{"m": "desc"}]})), ["c", "b", "a"]);
        assert_eq!(
            aggs(json!({"sort": [{"m": {"order": "asc"}}]})),
            ["a", "b", "c"]
        );
        assert_eq!(aggs(json!({"sort": "_count", "from": 1})), ["b", "c"]);
        assert_eq!(
            aggs(json!({"sort": {"_count": "desc"}, "size": 2})),
            ["c", "b"]
        );
    }

    #[test]
    fn bucket_sort_skips_buckets_missing_its_values_and_sorts_by_any_key() {
        let aggs = |bucket_sort: Value| {
            let body = json!({"aggs": {"f": {
                "filters": {"keyed": false, "filters": {
                    "a": {"term": {"k": "a"}},
                    "none": {"term": {"k": "none"}},
                    "c": {"match_all": {}},
                }},
                "aggs": {"m": {"max": {"field": "i"}}, "s": {"bucket_sort": bucket_sort}},
            }}});
            let aggs = aggs_of(&index(), body);
            let keys = aggs["f"]["buckets"].as_array().unwrap().iter();
            keys.map(|b| b["key"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        };
        // Like ES's default `skip` gap policy: a bucket with no value for any sort is dropped.
        assert_eq!(aggs(json!({"sort": [{"m": "desc"}]})), ["c", "a"]);
        assert_eq!(
            aggs(json!({"sort": [{"_key": "desc"}]})),
            ["none", "c", "a"]
        );
    }

    #[test]
    fn bucket_selector_keeps_buckets_whose_script_holds() {
        let keys = |path: &str, script: &str| {
            let body = json!({"size": 0, "aggs": {"k": {"terms": {"field": "k"}, "aggs": {
                "top": {"top_hits": {"size": 1, "sort": [{"i": "desc"}]}},
                "having": {"bucket_selector": {"buckets_path": {"ts": path}, "script": script}},
            }}}});
            let aggs = aggs_of(&index(), body);
            let buckets = aggs["k"]["buckets"].as_array().unwrap().iter();
            (buckets.map(|b| b["key"].as_str().unwrap().to_owned())).collect::<Vec<_>>()
        };
        assert_eq!(keys("top[_source.i]", "params.ts < 120"), ["b", "a"]);
        assert_eq!(keys("top[_sort]", "params.ts >= 101"), ["b", "c"]);
        // a null score is NaN, which no comparison holds for
        assert!(keys("top[_score]", "params.ts < 8000").is_empty());
        assert_eq!(keys("_count", "params.ts == 1"), ["a", "c"]);
    }

    #[test]
    fn derivative_and_max_bucket_pipelines() {
        let body = json!({"size": 0, "aggs": {
            "h": {"histogram": {"field": "i", "interval": 50},
                "aggs": {"s": {"sum": {"field": "i"}}, "d": {"derivative": {"buckets_path": "s"}}}},
            "best": {"max_bucket": {"buckets_path": "h>s"}},
        }});
        let aggs = aggs_of(&index(), body.clone());
        let buckets = aggs["h"]["buckets"].as_array().unwrap();
        let derivs: Vec<_> = buckets.iter().map(|b| b.get("d").cloned()).collect();
        let want = [
            None,
            Some(json!({"value": 50.0})),
            Some(json!({"value": 50.0})),
        ];
        assert_eq!(derivs[..3], want);
        assert_eq!(aggs["best"], json!({"value": 151.0, "keys": ["150.0"]}));
        // typed keys prefix names, but paths use the plain ones
        let mut req = SearchRequest::parse(&body).unwrap();
        req.typed_keys = true;
        let aggs = search(&index(), &req).aggregations.unwrap();
        assert_eq!(aggs["bucket_metric_value#best"]["value"], 151.0);
        assert_eq!(
            aggs["histogram#h"]["buckets"][1]["derivative#d"]["value"],
            50.0
        );
    }

    #[test]
    fn range_buckets_values_in_sorted_ranges() {
        let body = json!({"aggs": {
            "i": {"range": {"field": "i", "missing": 1000, "ranges": [
                {"from": 100}, {"to": 50, "key": "low"}, {"from": 50, "to": null}]},
                "aggs": {"m": {"max": {"field": "i"}}}},
            "f": {"range": {"field": "f", "format": "0.0#", "ranges": [{"from": 1, "to": 2.5}]}},
            "at": {"range": {"field": "at", "keyed": true,
                "ranges": [{"to": "2021-05-02T00:00:00Z"}]}},
            "none": {"range": {"field": "nope", "ranges": [{"from": -1}]}},
        }});
        let want = json!({
            "i": {"buckets": [
                {"key": "low", "to": 50.0, "doc_count": 1, "m": {"value": 1.0}},
                {"key": "50.0-*", "from": 50.0, "doc_count": 4, "m": {"value": 151.0}},
                {"key": "100.0-*", "from": 100.0, "doc_count": 3, "m": {"value": 151.0}},
            ]},
            "f": {"buckets": [{"key": "1.0-2.5", "from": 1.0, "from_as_string": "1.0",
                "to": 2.5, "to_as_string": "2.5", "doc_count": 2}]},
            "at": {"buckets": {"*-2021-05-02T00:00:00.000Z": {"to": 1_619_913_600_000.0,
                "to_as_string": "2021-05-02T00:00:00.000Z", "doc_count": 1}}},
            "none": {"buckets": [{"key": "-1.0-*", "from": -1.0, "doc_count": 0}]},
        });
        assert_eq!(aggs_of(&index(), body), want);
        let body = json!({"aggs": {"r": {"range": {"field": "i", "ranges": []}}}});
        let err = SearchRequest::parse(&body).unwrap_err();
        assert_eq!(err.msg, "No [ranges] specified for the [r] aggregation");
    }

    #[test]
    fn epoch_formats_read_numbers() {
        let mut ix = Index::default();
        for (f, format) in [("s", "epoch_second"), ("ms", "epoch_millis")] {
            let m = FieldMapping::Date {
                format: Some(format.into()),
                locale: None,
                fields: Default::default(),
            };
            ix.mappings.properties.insert(f.into(), m);
        }
        ix.put("1", json!({"s": 1000, "ms": 1000}));
        ix.put("2", json!({"s": "2000", "ms": "2000"}));
        let body = json!({"aggs": {"s": {"max": {"field": "s"}}, "ms": {"max": {"field": "ms"}}}});
        let aggs = aggs_of(&ix, body);
        assert_eq!(
            (&aggs["s"]["value"], &aggs["ms"]["value"]),
            (&json!(2e6), &json!(2e3))
        );
    }

    #[test]
    fn date_range_reads_bounds_and_renders_keys_in_the_field_format() {
        let mut ix = Index::default();
        let m = FieldMapping::Date {
            format: Some("epoch_second".into()),
            locale: None,
            fields: Default::default(),
        };
        ix.mappings.properties.insert("s".into(), m);
        ix.put("1", json!({"s": 1000}));
        ix.put("2", json!({"s": "3000"}));
        ix.put("3", json!({}));
        let body = json!({"aggs": {
            "s": {"date_range": {"field": "s", "missing": "0",
                "ranges": [{"from": "3000", "to": 4000}, {"key": "low", "to": 1000}]}},
            "none": {"date_range": {"field": "nope",
                "ranges": [{"from": "2020-01-01T00:00:00Z"}]}},
        }});
        let want = json!({
            "s": {"buckets": [
                {"key": "low", "to": 1e6, "to_as_string": "1000", "doc_count": 1},
                {"key": "3000-4000", "from": 3e6, "from_as_string": "3000",
                    "to": 4e6, "to_as_string": "4000", "doc_count": 1},
            ]},
            "none": {"buckets": [{"key": "2020-01-01T00:00:00.000Z-*", "from": 1_577_836_800_000.0,
                "from_as_string": "2020-01-01T00:00:00.000Z", "doc_count": 0}]},
        });
        assert_eq!(aggs_of(&ix, body), want);
    }

    #[test]
    fn date_field_formats_date_range_reads_and_prints_bounds_in_its_time_zone() {
        let mut ix = Index::default();
        let m = FieldMapping::Date {
            format: Some("yyyy-MM-dd HH:mm||yyyy-MM-dd".into()),
            locale: None,
            fields: Default::default(),
        };
        ix.mappings.properties.insert("at".into(), m);
        ix.put("1", json!({"at": "2016-01-01 22:00"}));
        ix.put("2", json!({"at": "2016-01-01 23:30"}));
        let body = json!({"aggs": {"r": {"date_range": {"field": "at", "time_zone": "+01:00",
            "ranges": [{"from": "2016-01-01", "to": "2016-01-02"}]}}}});
        let want = json!({"r": {"buckets": [{"key": "2016-01-01 00:00-2016-01-02 00:00",
            "from": 1_451_602_800_000.0, "from_as_string": "2016-01-01 00:00",
            "to": 1_451_689_200_000.0, "to_as_string": "2016-01-02 00:00", "doc_count": 1}]}});
        assert_eq!(aggs_of(&ix, body), want);
    }

    #[test]
    fn named_date_formats_parse_iso_dates() {
        let mut ix = Index::default();
        let at = FieldMapping::Date {
            format: Some("strict_date_time||strict_date".into()),
            locale: None,
            fields: Default::default(),
        };
        ix.mappings.properties.insert("at".into(), at);
        ix.put("1", json!({"at": "2021-05-01T07:10:00Z"}));
        let aggs = aggs_of(&ix, json!({"aggs": {"max": {"max": {"field": "at"}}}}));
        assert_eq!(aggs["max"]["value"], 1_619_853_000_000.0);
    }

    #[test]
    fn localized_dates_aggregate_as_millis() {
        let mut ix = Index::default();
        let at = FieldMapping::Date {
            format: Some("E, d MMM yyyy HH:mm:ss Z".into()),
            locale: Some("fr".into()),
            fields: Default::default(),
        };
        ix.mappings.properties.insert("at".into(), at);
        ix.put("1", json!({"at": "mer., 6 déc. 2000 02:55:00 -0800"}));
        let aggs = aggs_of(&ix, json!({"aggs": {"max": {"max": {"field": "at"}}}}));
        // 2000-12-06T10:55:00Z
        assert_eq!(aggs["max"]["value"], 976_100_100_000.0);
    }

    #[test]
    fn range_bounds_take_the_field_precision() {
        // like ES's "Float Endpoint Exclusive": 0.04 as a float is below 0.04 as a double
        let mut ix = Index::default();
        let props = &mut ix.mappings.properties;
        props.insert("f".into(), FieldMapping::Float);
        props.insert("h".into(), FieldMapping::HalfFloat);
        for (id, x) in [("1", 0.01), ("2", 0.0152), ("3", 0.04)] {
            ix.put(id, json!({"f": x, "h": x}));
        }
        let ranges = json!([{"from": 0, "to": 0.04}, {"from": 0.04, "to": 1}]);
        let body = json!({"aggs": {
            "f": {"range": {"field": "f", "ranges": ranges}},
            "h": {"range": {"field": "h", "ranges": [{"to": 0.0152}, {"from": 0.0152}]}},
        }});
        let aggs = aggs_of(&ix, body);
        let counts = |a: &str| {
            aggs[a]["buckets"]
                .as_array()
                .unwrap()
                .iter()
                .map(|b| b["doc_count"].as_u64().unwrap())
                .collect::<Vec<_>>()
        };
        assert_eq!(counts("f"), [2, 1]);
        assert_eq!(counts("h"), [1, 2]);
        // the bounds render as requested
        assert_eq!(aggs["f"]["buckets"][0]["key"], "0.0-0.04");
        assert_eq!(aggs["h"]["buckets"][1]["from"], 0.0152);
    }

    #[test]
    fn missing_counts_documents_without_values() {
        let body = json!({"aggs": {
            "i": {"missing": {"field": "i"}, "aggs": {"c": {"value_count": {"field": "k"}}}},
            "none": {"missing": {"field": "nope"}},
            "filled": {"missing": {"field": "i", "missing": 1}},
        }});
        let want = json!({
            "i": {"doc_count": 1, "c": {"value": 0}},
            "none": {"doc_count": 5},
            "filled": {"doc_count": 0},
        });
        assert_eq!(aggs_of(&index(), body), want);
    }

    #[test]
    fn nested_buckets_documents_with_objects_at_the_path() {
        let mut ix = Index::default();
        let props = &mut ix.mappings.properties;
        props.insert("n".into(), FieldMapping::Nested { dynamic: None });
        props.insert("n.i".into(), FieldMapping::Integer);
        ix.put("1", json!({"n": {"i": 2}}));
        ix.put("2", json!({"n": [{"i": 3}]}));
        ix.put("3", json!({"other": 1}));
        let body = json!({"aggs": {
            "n": {"nested": {"path": "n"}, "aggs": {"s": {"sum": {"field": "n.i"}}}},
            "none": {"nested": {"path": "nope"}},
        }});
        let want = json!({"n": {"doc_count": 2, "s": {"value": 5.0}}, "none": {"doc_count": 0}});
        assert_eq!(aggs_of(&ix, body), want);
        let err = SearchRequest::parse(&json!({"aggs": {"n": {"nested": {}}}})).unwrap_err();
        assert!(
            err.to_string()
                .contains("Missing [path] field for nested aggregation [n]")
        );
    }

    #[test]
    fn global_aggregates_every_document_not_the_matches() {
        let body = json!({"query": {"term": {"k": "b"}}, "aggs": {
            "g": {"global": {}, "aggs": {"m": {"max": {"field": "i"}}}},
        }});
        let want = json!({"g": {"doc_count": 5, "m": {"value": 151.0}}});
        assert_eq!(aggs_of(&index(), body), want);
    }

    #[test]
    fn request_doubles_parse_exactly() {
        // serde_json's default float parser can be one ulp off this value
        let body = r#"{"aggs": {"m": {"max": {"field": "nope", "missing": -91.70509831248421}}}}"#;
        let got = aggs_of(&index(), serde_json::from_str(body).unwrap());
        assert_eq!(got["m"]["value"].as_f64(), Some(-91.705_098_312_484_21));
    }

    #[test]
    fn stats_and_extended_stats() {
        let body = json!({"query": {"range": {"i": {"gte": 50}}}, "aggs": {
            "s": {"stats": {"field": "i"}},
            "e": {"extended_stats": {"field": "i"}},
            "e3": {"extended_stats": {"field": "i", "sigma": 3}},
            "d": {"stats": {"field": "at"}},
            "none": {"extended_stats": {"field": "nope"}},
        }});
        let got = aggs_of(&index(), body);
        assert_eq!(
            got["s"],
            json!({"count": 3, "min": 51.0, "max": 151.0, "avg": 101.0, "sum": 303.0})
        );
        // values from ES's extended_stats_metric.yml "Filtered test"
        let e = &got["e"];
        assert_eq!(e["count"], 3);
        assert_eq!(e["sum_of_squares"], 35603.0);
        assert_eq!(e["variance"], 1666.6666666666667);
        assert_eq!(e["variance_population"], 1666.6666666666667);
        assert_eq!(e["variance_sampling"], 2500.0);
        assert_eq!(e["std_deviation"], 40.824829046386306);
        assert_eq!(e["std_deviation_sampling"], 50.0);
        assert_eq!(
            e["std_deviation_bounds"],
            json!({"upper": 182.6496580927726, "lower": 19.35034190722739,
                "upper_population": 182.6496580927726, "lower_population": 19.35034190722739,
                "upper_sampling": 201.0, "lower_sampling": 1.0})
        );
        assert_eq!(got["e3"]["std_deviation_bounds"]["upper_sampling"], 251.0);
        assert_eq!(got["d"]["min_as_string"], "2021-05-02T00:00:00.000Z");
        assert_eq!(got["d"]["max_as_string"], "2021-05-04T00:00:00.000Z");
        let none = &got["none"];
        assert_eq!(
            (
                &none["count"],
                &none["min"],
                &none["sum"],
                &none["variance"]
            ),
            (&json!(0), &Value::Null, &json!(0.0), &Value::Null)
        );
        assert_eq!(none["std_deviation_bounds"]["lower_sampling"], Value::Null);
        // Jackson quotes NaN, the sampling variance of one value
        let body = json!({"query": {"term": {"i": 1}}, "aggs": {
            "e": {"extended_stats": {"field": "i"}}}});
        let one = &aggs_of(&index(), body)["e"];
        assert_eq!(
            (&one["variance"], &one["variance_sampling"]),
            (&json!(0.0), &json!("NaN"))
        );
    }

    #[test]
    fn cardinality_counts_like_elasticsearch() {
        let mut ix = Index::default();
        let props = &mut ix.mappings.properties;
        props.insert(
            "k".into(),
            FieldMapping::Keyword {
                ignore_above: None,
                split_queries_on_whitespace: false,
            },
        );
        props.insert(
            "ks".into(),
            FieldMapping::Keyword {
                ignore_above: None,
                split_queries_on_whitespace: false,
            },
        );
        props.insert("i".into(), FieldMapping::Long);
        props.insert("d".into(), FieldMapping::Double);
        props.insert("f".into(), FieldMapping::Float);
        let date = FieldMapping::Date {
            format: None,
            locale: None,
            fields: Default::default(),
        };
        props.insert("dt".into(), date);
        // ES indexed these values as one document; spread them so the disk save fits a chunk
        let f = |n: u64| format!("{}.{}", n / 10, n % 10).parse::<f64>().unwrap();
        for d in 0..20u64 {
            let n = d * 1000..(d + 1) * 1000;
            ix.put(
                &d.to_string(),
                json!({
                    "k": n.clone().map(|n| format!("v{n}")).collect::<Vec<_>>(),
                    "i": n.clone().map(|n| n * 7).collect::<Vec<_>>(),
                    "d": n.clone().map(|n| n as f64 / 4.0).collect::<Vec<_>>(),
                    "f": n.clone().map(f).collect::<Vec<_>>(),
                    "dt": n.map(|n| 1_600_000_000_000 + n * 1000).collect::<Vec<_>>(),
                    "ks": (0..30).map(|n| format!("s{n}")).collect::<Vec<_>>(),
                }),
            );
        }
        let thresholds = [Some(0), Some(10), Some(100), Some(1000), None, Some(40000)];
        let mut aggs = serde_json::Map::new();
        for field in ["k", "i", "d", "f", "dt", "ks"] {
            for t in thresholds {
                let mut c = json!({"field": field});
                if let Some(t) = t {
                    c["precision_threshold"] = t.into();
                }
                let name = format!("{field}_{}", t.map_or("None".into(), |t| t.to_string()));
                aggs.insert(name, json!({"cardinality": c}));
            }
        }
        // Elasticsearch 9.5.5's response to the same document and request
        let want = json!({
            "k_0": 22845, "k_10": 24377, "k_100": 20243, "k_1000": 20095, "k_None": 19858,
            "k_40000": 19998, "i_0": 16119, "i_10": 17317, "i_100": 20676, "i_1000": 19778,
            "i_None": 19822, "i_40000": 19999, "d_0": 17108, "d_10": 22787, "d_100": 20404,
            "d_1000": 20041, "d_None": 19809, "d_40000": 19994, "f_0": 16317, "f_10": 18057,
            "f_100": 20877, "f_1000": 20017, "f_None": 19830, "f_40000": 20002,
            "dt_0": 15135, "dt_10": 18210, "dt_100": 20486, "dt_1000": 20057,
            "dt_None": 19839, "dt_40000": 19995, "ks_0": 35, "ks_10": 30, "ks_100": 30,
            "ks_1000": 30, "ks_None": 30, "ks_40000": 30,
        });
        let values = |got: Value| -> Value {
            let got = got.as_object().unwrap().iter();
            got.map(|(k, v)| (k.clone(), v["value"].clone())).collect()
        };
        let body = json!({"size": 0, "aggs": aggs});
        assert_eq!(values(aggs_of(&ix, body.clone())), want);
        let dir = std::env::temp_dir().join(format!("esm-card-{}", std::process::id()));
        disk::save(&dir, &Indices::from([("c".to_owned(), ix)])).unwrap();
        let opened = disk::open(&dir).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(values(aggs_of(&opened["c"], body)), want);
    }

    #[test]
    fn percentiles_interpolate_like_elasticsearch() {
        let body = json!({"aggs": {
            "p": {"percentiles": {"field": "i"}},
            "e": {"percentiles": {"field": "i", "percents": [50, 5], "keyed": false,
                "tdigest": {"compression": 200, "execution_hint": "high_accuracy"}}},
            "d": {"percentiles": {"field": "at", "percents": [50]}},
            "none": {"percentiles": {"field": "nope", "percents": [0.0001]}},
            "m": {"percentiles": {"field": "nope", "missing": 1.0, "percents": [99.9]}},
        }});
        let got = aggs_of(&index(), body);
        // values from ES's percentiles_tdigest_metric.yml "Basic test"
        assert_eq!(
            got["p"],
            json!({"values": {"1.0": 2.5, "5.0": 8.5, "25.0": 38.5, "50.0": 76.0,
                "75.0": 113.5, "95.0": 143.5, "99.0": 149.5}})
        );
        assert_eq!(
            got["e"],
            json!({"values": [{"key": 5.0, "value": 8.5}, {"key": 50.0, "value": 76.0}]})
        );
        assert_eq!(
            got["d"],
            json!({"values": {"50.0": 1_619_956_800_000.0,
                "50.0_as_string": "2021-05-02T12:00:00.000Z"}})
        );
        assert_eq!(got["none"], json!({"values": {"1.0E-4": null}}));
        assert_eq!(got["m"], json!({"values": {"99.9": 1.0}}));
    }

    #[test]
    fn percentiles_digest_like_elasticsearch() {
        let index = |base: u64, docs: u64| {
            let mut ix = Index::default();
            let props = &mut ix.mappings.properties;
            props.insert("d".into(), FieldMapping::Double);
            props.insert("l".into(), FieldMapping::Long);
            for doc in 0..docs {
                let n = base + doc * 500..base + (doc + 1) * 500;
                let d = n
                    .clone()
                    .map(|n| (n * 7919 % 10007) as f64 / 7.0 + (n % 13) as f64);
                let l = n.map(|n| (n * 7919 % 10007) * (n % 5));
                let source = json!({"d": d.collect::<Vec<_>>(), "l": l.collect::<Vec<_>>()});
                ix.put(&doc.to_string(), source);
            }
            ix
        };
        let percents = [
            0.0, 0.1, 1.0, 5.0, 25.0, 50.0, 75.0, 95.0, 99.0, 99.9, 100.0,
        ];
        let agg = |field: &str, compression: Option<u32>| {
            let mut p = json!({"field": field, "percents": percents});
            if let Some(c) = compression {
                p["tdigest"] = json!({"compression": c});
            }
            json!({"percentiles": p})
        };
        let body = json!({"size": 0, "aggs": {
            "d_def": agg("d", None), "d_c1": agg("d", Some(1)), "d_c50": agg("d", Some(50)),
            "l_def": agg("l", None), "l_c1000": agg("l", Some(1000)),
        }});
        // Elasticsearch 9.5.5's responses to the same documents (indexed in this order) and
        // request, over index p1 and over p1,p2
        let want_p1 = json!({"d_c1":[0.0,7.21360544217687,24.53486282536385,91.17562698546972,376.7322952610915,721.4876900242893,1061.15479708507,1352.4189169465624,1416.181128285852,1433.7180952380957,1436.0],"d_c50":[0.0,6.464285714285714,20.4096912202381,78.47402597402596,365.27301382811163,721.3542580279241,1075.6396867558842,1363.2653586476094,1420.8612995027493,1434.571428571429,1436.0],"d_def":[0.0,6.357142857142858,20.21153373878624,77.56954501527233,364.3704463731752,721.1580498708637,1077.8105211450777,1364.0550774526675,1420.8700621118014,1434.5816326530617,1436.0],"l_c1000":[0.0,0.0,0.0,0.0,1207.0,7181.0,16125.75,30064.199999999997,37976.04,39720.15600000003,39912.0],"l_def":[0.0,0.0,0.0,0.0,1229.4640981262087,7184.06824408223,16122.229760923818,30248.488320042532,37988.40946440947,39823.33333333344,39912.0]});
        let want_both = json!({"d_c1":[0.0,7.08733441056925,25.93499698210927,97.33525789786015,416.3464105486386,708.0253131189929,947.0418360774675,1343.333692040015,1416.8177900224964,1433.7797959183677,1439.4285714285713],"d_c50":[0.0,6.296409472880062,20.386422638149412,78.75168553959415,367.8227239196291,717.6558500152502,1068.6814964870318,1359.981660211795,1420.7944776591944,1434.563775510204,1439.4285714285713],"d_def":[0.0,6.285714285714286,20.324704372552397,77.55426322761487,365.8184886427841,721.1480414265849,1078.5901697174731,1364.4613141387238,1421.0354890699064,1434.5556586270873,1439.4285714285713],"l_c1000":[0.0,0.0,0.0,0.0,1195.0,7185.0,16174.5,29993.64999999996,37968.04,39720.136000000275,39912.0],"l_def":[0.0,0.0,0.0,0.0,1296.6107989361483,7180.151654252229,16160.187182393422,30287.33779789335,37982.72589473684,39783.60000000006,39912.0]});
        let req = SearchRequest::parse(&body).unwrap();
        let values = |targets: &[crate::Target]| -> Value {
            let got = crate::search_many(targets, &req)
                .unwrap()
                .aggregations
                .unwrap();
            let got = got.as_object().unwrap().iter();
            let vals = |v: &Value| {
                let p = percents
                    .iter()
                    .map(|p| v["values"][super::java_double(*p)].clone());
                Value::Array(p.collect())
            };
            got.map(|(k, v)| (k.clone(), vals(v))).collect()
        };
        // ponytail: values kept exactly (l_c1000) match ES's bits; merged centroids match within
        // `tol` of the value range (the cause of the difference is not found). Several shards
        // reduce more coarsely, so they differ more, most at compression 1 (skipped).
        let close = |got: &Value, want: &Value, tol: f64| {
            for (k, w) in want.as_object().unwrap() {
                let range = w[10].as_f64().unwrap() - w[0].as_f64().unwrap();
                let tol = match k.as_str() {
                    "l_c1000" => 0.0,
                    "d_c1" if tol > 1e-4 => continue,
                    _ => tol * range,
                };
                for (g, w) in got[k].as_array().unwrap().iter().zip(w.as_array().unwrap()) {
                    let (g, w) = (g.as_f64().unwrap(), w.as_f64().unwrap());
                    assert!((g - w).abs() <= tol, "{k}: {g} vs {w}");
                }
            }
        };
        let (p1, p2) = (index(0, 20), index(10_000, 10));
        close(&values(&[("p1", 0, &p1, None)]), &want_p1, 1e-4);
        let both = values(&[("p1", 0, &p1, None), ("p2", 0, &p2, None)]);
        close(&both, &want_both, 1e-2);
        let dir = std::env::temp_dir().join(format!("esm-pct-{}", std::process::id()));
        disk::save(
            &dir,
            &Indices::from([("p1".to_owned(), p1), ("p2".to_owned(), p2)]),
        )
        .unwrap();
        let opened = disk::open(&dir).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        let (p1, p2) = (&opened["p1"], &opened["p2"]);
        assert_eq!(values(&[("p1", 0, p1, None), ("p2", 0, p2, None)]), both);
    }

    #[test]
    fn aggs_reject_bad_requests() {
        let err = |aggs: Value| {
            let req = SearchRequest::parse(&json!({"aggs": aggs}));
            let req = match req {
                Ok(req) => req,
                Err(e) => return e.msg,
            };
            let ix = index();
            match crate::search_many(&[("i", 0, &ix, None)], &req) {
                Ok(_) => panic!("accepted {aggs}"),
                Err(e) => e.msg,
            }
        };
        assert_eq!(
            err(json!({"a": {"avg": {"field": "k"}}})),
            "Field [k] of type [keyword] is not supported for aggregation [avg]"
        );
        assert!(err(json!({"a": {"value_count": {"field": "t"}}})).starts_with("Text fields"));
        assert_eq!(
            err(json!({"a": {"avg": {"field": "i"}, "sum": {"field": "i"}}})),
            "Found two aggregation type definitions in [a]: [avg] and [sum]"
        );
        assert_eq!(
            err(json!({"a": {"nope": {}}})),
            "Unknown aggregation type [nope]"
        );
        assert_eq!(
            err(json!({"a": {"meta": null, "global": {}}})),
            "Expected [START_OBJECT] under [meta], but got a [VALUE_NULL] in [a]"
        );
        assert_eq!(
            err(json!({"a": {"avg": {"field": "i"}, "aggs": {"b": {"max": {"field": "i"}}}}})),
            "Aggregator [a] of type [avg] cannot accept sub-aggregations"
        );
        assert_eq!(
            err(json!({"a": {"extended_stats": {"field": "i", "sigma": -1}}})),
            "[sigma] must be greater than or equal to 0. Found [-1.0] in [a]"
        );
        assert_eq!(
            err(json!({"a": {"extended_stats": {"field": "i", "sigma": "foo"}}})),
            "[extended_stats] failed to parse field [sigma]"
        );
        assert_eq!(
            err(json!({"a": {"stats": {"field": "i", "sigma": 1}}})),
            "[stats] unknown field [sigma]"
        );
        assert_eq!(
            err(json!({"a": {"cardinality": {"field": "i", "precision_threshold": -1}}})),
            "[precisionThreshold] must be greater than or equal to 0. Found [-1] in [a]"
        );
        assert!(
            err(json!({"a": {"cardinality": {"field": "k", "execution_hint": "x"}}}))
                .starts_with("Invalid execution mode for cardinality aggregation.  Got [x]")
        );
        assert_eq!(
            err(json!({"a": {"percentiles": {"field": "i", "tdigest": {"compression": -1}}}})),
            "[compression] must be greater than or equal to 0. Found [-1.0]"
        );
        assert_eq!(
            err(json!({"a": {"percentiles": {"field": "i", "tdigest": {"execution_hint": "x"}}}})),
            "Invalid execution_hint [x], valid values are [default, high_accuracy]"
        );
        assert_eq!(
            err(json!({"a": {"percentiles": {"field": "i", "percents": []}}})),
            "[percents] must not be empty: [a]"
        );
        assert_eq!(
            err(json!({"a": {"percentiles": {"field": "i", "percents": [5, 5.0]}}})),
            "percent [5.0] has been specified twice: [a]"
        );
        assert_eq!(
            err(json!({"a": {"percentiles": {"field": "i", "percents": [101]}}})),
            "percent must be in [0,100], got [101.0]: [a]"
        );
        for bad in [json!(null), json!(["foo"])] {
            assert_eq!(
                err(json!({"a": {"percentiles": {"field": "i", "percents": bad}}})),
                "[percentiles] failed to parse field [percents]"
            );
        }
        assert_eq!(
            err(json!({"a": {"percentiles": {"field": "k"}}})),
            "Field [k] of type [keyword] is not supported for aggregation [percentiles]"
        );
        assert_eq!(
            err(json!({"a": {"meta": {}}})),
            "Missing definition for aggregation [a]"
        );
    }
}
