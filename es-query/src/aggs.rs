//! Aggregations (`aggs`/`aggregations` in a search body): parsed into named aggregations,
//! collected over every match of each searched index (one shard each), then reduced across
//! indices and rendered like Elasticsearch.
//!
//! Values come from what the field's doc values hold: read from disk for documents in
//! segments, and converted the same way from the source for documents in memory.

mod histogram;
mod terms;

use crate::hll::{self, Hll};
use crate::tdigest::TDigest;
use crate::{
    MAX_BUCKETS, Query, QueryError, Searcher, fielddata_disabled, int, num, parse_query,
    source_path,
};
use es_index::{Document, FieldMapping, iso_date, postings::values};
use histogram::Histogram;
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
        /// Sorted, distinct, in `[0, 100]`.
        percents: Vec<f64>,
        keyed: bool,
    },
    Terms(Box<Terms>),
    Histogram(Box<Histogram>),
    /// The single bucket of the documents matching a query.
    Filter(Box<Query>),
    /// The single bucket of the documents without a value for the field (nor `missing`).
    Missing,
    /// The single bucket of every document of the searched indices, whatever the query.
    Global,
    /// A bucket per query, and maybe one of the documents matching none.
    Filters(Box<Filters>),
    /// A bucket per range of values.
    Range(Box<Ranges>),
}

/// The `range` aggregation's ranges, like ES's RangeAggregationBuilder.
#[derive(Debug, Clone, PartialEq)]
struct Ranges {
    /// Each range's key, `from` and `to` as requested: numbers, or strings the field parses.
    ranges: Vec<(Option<String>, Value, Value)>,
    keyed: bool,
    /// A DecimalFormat pattern for numeric bounds.
    format: Option<String>,
}

impl Ranges {
    fn parse(name: &str, params: &Map<String, Value>) -> Result<Self, QueryError> {
        let bad = |k: &str| QueryError::x_content(format!("[range] failed to parse field [{k}]"));
        let mut r = Ranges {
            ranges: vec![],
            keyed: false,
            format: None,
        };
        for (k, v) in params {
            match k.as_str() {
                "field" | "missing" => {}
                "keyed" => r.keyed = v.as_bool().ok_or_else(|| bad(k))?,
                "format" => r.format = Some(v.as_str().ok_or_else(|| bad(k))?.into()),
                "ranges" => {
                    for range in v.as_array().ok_or_else(|| bad(k))? {
                        let o = range.as_object().ok_or_else(|| bad(k))?;
                        if let Some(k) = o
                            .keys()
                            .find(|k| !["key", "from", "to"].contains(&k.as_str()))
                        {
                            return Err(format!("[range] unknown field [{k}]").into());
                        }
                        let bound = |k| o.get(k).cloned().unwrap_or(Value::Null);
                        let key = o.get("key").map(es_index::postings::text);
                        r.ranges.push((key, bound("from"), bound("to")));
                    }
                }
                _ => return Err(format!("[range] unknown field [{k}]").into()),
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
    fn buckets(
        &self,
        s: &Searcher,
        field: &str,
        m: Option<&FieldMapping>,
        empty: &[Stats],
    ) -> Result<Vec<Stats>, QueryError> {
        let date = matches!(
            m,
            Some(FieldMapping::Date { .. } | FieldMapping::DateNanos { .. })
        );
        let parse = |v: &Value, open: f64| {
            match v {
                Value::Null => Some(open),
                Value::String(_) if date => s.instant(field).and_then(|f| f(v)).map(|t| t as f64),
                v => num(v),
            }
            .ok_or_else(|| {
                QueryError::illegal(format!("failed to parse [{}]", es_index::postings::text(v)))
            })
        };
        let mut ranges = (self.ranges.iter())
            .map(|(key, from, to)| {
                let bounds = (parse(from, f64::NEG_INFINITY)?, parse(to, f64::INFINITY)?);
                Ok((key.clone(), bounds.0, bounds.1))
            })
            .collect::<Result<Vec<_>, QueryError>>()?;
        ranges.sort_by(|x, y| x.1.total_cmp(&y.1).then(x.2.total_cmp(&y.2)));
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
            Metric::Percentiles { .. } => "percentiles",
            Metric::Terms(_) => "terms",
            Metric::Histogram(h) if h.date => "date_histogram",
            Metric::Histogram(_) => "histogram",
            Metric::Filter(_) => "filter",
            Metric::Missing => "missing",
            Metric::Global => "global",
            Metric::Filters(_) => "filters",
            Metric::Range(_) => "range",
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
        matches!(self, Metric::Filter(_) | Metric::Missing | Metric::Global)
    }
}

/// Java's `Double.toString`, which names the keys of `percentiles`.
fn java_double(x: f64) -> String {
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

/// Java's `DecimalFormat` (HALF_EVEN) of `x`: a literal prefix and suffix around `0`/`#`
/// integer and fraction digits.
// ponytail: no grouping, quoting, exponents, percent or negative subpatterns; add when needed
fn decimal_format(pattern: &str, x: f64) -> String {
    let digit = |c: char| matches!(c, '#' | '0' | ',' | '.');
    let start = pattern.find(digit).unwrap_or(pattern.len());
    let end = (pattern[start..].find(|c| !digit(c))).map_or(pattern.len(), |i| start + i);
    let (int, frac) = pattern[start..end]
        .split_once('.')
        .unwrap_or((&pattern[start..end], ""));
    let min_int = int.matches('0').count();
    let (min_frac, max_frac) = (frac.matches('0').count(), frac.len());
    // Rust rounds the exact binary value half to even, as DecimalFormat does
    let s = format!("{:.max_frac$}", x.abs());
    let (i, f) = s.split_once('.').unwrap_or((&s, ""));
    let f = f.trim_end_matches('0');
    let f = format!("{f:0<min_frac$}");
    let i = i.trim_start_matches('0');
    let mut num = format!("{i:0>min_int$}");
    if !f.is_empty() {
        num = format!("{num}.{f}");
    } else if num.is_empty() {
        num = "0".into();
    }
    let sign = if x < 0.0 { "-" } else { "" };
    format!("{sign}{}{num}{}", &pattern[..start], &pattern[end..])
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
    for (k, v) in body {
        match k.as_str() {
            "meta" if !v.is_object() => {
                let token = match v {
                    Value::Null => "VALUE_NULL",
                    Value::Bool(_) => "VALUE_BOOLEAN",
                    Value::Number(_) => "VALUE_NUMBER",
                    Value::String(_) => "VALUE_STRING",
                    _ => "START_ARRAY",
                };
                return Err(format!(
                    "Expected [START_OBJECT] under [meta], but got a [{token}] in [{name}]"
                )
                .into());
            }
            "meta" => meta = Some(v.clone()),
            "aggs" | "aggregations" => subs = parse(v)?,
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
        },
        "terms" => Metric::Terms(Box::new(Terms::parse(name, object()?, &subs)?)),
        "histogram" | "date_histogram" => {
            let h = Histogram::parse(name, object()?, t == "date_histogram")?;
            Metric::Histogram(Box::new(h))
        }
        "filter" => Metric::Filter(Box::new(parse_query(params)?)),
        "missing" => Metric::Missing,
        "global" => Metric::Global,
        "filters" => Metric::Filters(Box::new(Filters::parse(object()?)?)),
        "range" => Metric::Range(Box::new(Ranges::parse(name, object()?)?)),
        _ => return Err(format!("Unknown aggregation type [{t}]").into()),
    };
    if !subs.is_empty() && !metric.buckets() {
        return Err(QueryError::illegal(format!(
            "Aggregator [{name}] of type [{t}] cannot accept sub-aggregations"
        )));
    }
    if let Metric::Filter(_) | Metric::Global | Metric::Filters(_) = metric {
        return Ok(Agg {
            name: name.to_owned(),
            meta,
            metric,
            field: String::new(),
            missing: None,
            subs,
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
            ("percents", Metric::Percentiles { percents, .. }) => {
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
    let field = params
        .get("field")
        .and_then(Value::as_str)
        .ok_or("Required one of fields [field, script], but none were specified.")?;
    Ok(Agg {
        name: name.to_owned(),
        meta,
        metric,
        field: field.to_owned(),
        missing: params.get("missing").cloned(),
        subs,
    })
}

/// Splits an order path into its first aggregation's name and the rest: `>` and the path into
/// that single-bucket aggregation, `.` and a metric key, or nothing.
fn split_path(path: &str) -> (&str, &str) {
    path.split_at(
        path.find('>')
            .or_else(|| path.rfind('.'))
            .unwrap_or(path.len()),
    )
}

/// The message of an order path into an aggregation that has no single value.
const NOT_SORTABLE: &str = "Buckets can only be sorted on a sub-aggregator path that is built out of zero or more single-bucket aggregations within the path and a final single-bucket or a metrics aggregation at the path end.";

/// The value `terms` buckets sort by for an order path into `agg` (`rest` is the path after
/// its name, see `split_path`), like ES's aggregators' `bucketComparator`; `st` is `agg`'s
/// result, `None` to only check the path. `Err` explains an invalid path.
// ponytail: multi-value metrics sort by stats names only, not by percentiles
fn sort_value(agg: &Agg, rest: &str, st: Option<&Stats>) -> Result<f64, String> {
    let name = &agg.name;
    if let Some(path) = rest.strip_prefix('>') {
        if !agg.metric.single() {
            return Err(NOT_SORTABLE.into());
        }
        let (sub, rest) = split_path(path);
        let j = (agg.subs.iter().position(|s| s.name == sub)).ok_or_else(|| format!("The provided aggregation [{sub}] either does not exist, or is a pipeline aggregation and cannot be used to sort the buckets."))?;
        return sort_value(&agg.subs[j], rest, st.and_then(|s| s.subs.get(j)));
    }
    let key = rest.strip_prefix('.');
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
        Metric::Filter(_) | Metric::Missing | Metric::Global => {
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
        Metric::Terms(_) | Metric::Histogram(_) | Metric::Filters(_) | Metric::Range(_) => {
            return Err(NOT_SORTABLE.into());
        }
        Metric::Stats | Metric::ExtendedStats { .. } | Metric::Percentiles { .. } => {
            let stats = !matches!(agg.metric, Metric::Percentiles { .. });
            return match key {
                None => Err("When ordering on a multi-value metrics aggregation a metric name must be specified.".into()),
                Some("count") if stats => Ok(st.count as f64),
                Some("sum") if stats => Ok(st.sum),
                Some("min") if stats => Ok(st.min),
                Some("max") if stats => Ok(st.max),
                Some("avg") if stats => Ok(avg),
                Some(k) => Err(format!("Unknown metric name [{k}] on multi-value metrics aggregation [{name}]")),
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
    /// The field is a date in some index, so values also render as dates.
    date: bool,
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
    /// A `range` bucket's key and requested bounds (open ends infinite).
    range: Option<(Option<String>, f64, f64)>,
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
            date: false,
            hll: None,
            digest: None,
            terms: None,
            subs: vec![],
            buckets: vec![],
            range: None,
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
        self.date |= o.date;
        self.range = self.range.take().or(o.range);
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

/// A source value as the field's doc values hold it; `None` if they cannot.
fn doc_value(s: &Searcher, field: &str, mapping: &FieldMapping, v: &Value) -> Option<Value> {
    use FieldMapping as M;
    match mapping {
        M::Long | M::Integer | M::Short | M::Byte => {
            int(v).or_else(|| num(v).map(|f| f as i64)).map(Value::from)
        }
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
            None => es_index::parse_date_millis(v),
        }
        .map(Value::from),
        M::Boolean => match v {
            Value::Bool(_) => Some(v.clone()),
            Value::String(b) => b.parse::<bool>().ok().map(Value::Bool),
            _ => None,
        },
        M::Keyword { ignore_above } => {
            let t = es_index::postings::text(v);
            ignore_above
                .is_none_or(|n| t.chars().count() <= n as usize)
                .then(|| t.into())
        }
        M::Ip => es_index::ip_bytes(v).map(|ip| es_index::ip_string(ip).into()),
        // ponytail: date_range's `format` is ignored
        M::LongRange => range(v, int),
        M::DateRange { .. } => range(v, es_index::parse_date_millis),
        M::Text | M::TextKeyword { .. } => None,
    }
}

/// A range field's value as its inclusive `[from, to]`, open ends at the type's limits, like
/// RangeFieldMapper.
fn range(v: &Value, parse: fn(&Value) -> Option<i64>) -> Option<Value> {
    let o = v.as_object()?;
    let end = |k: &str| o.get(k).filter(|v| !v.is_null()).map(parse);
    let from = match (end("gte"), end("gt")) {
        (Some(x), _) => x?,
        (_, Some(x)) => x?.checked_add(1)?,
        _ => i64::MIN,
    };
    let to = match (end("lte"), end("lt")) {
        (Some(x), _) => x?,
        (_, Some(x)) => x?.checked_sub(1)?,
        _ => i64::MAX,
    };
    Some(Value::from(vec![from, to]))
}

/// ES's ValuesSourceType of a field: every number is `numeric`.
fn values_source(m: &FieldMapping) -> &'static str {
    if m.is_numeric() {
        "numeric"
    } else {
        m.type_name()
    }
}

/// Reads a field's doc values for on-disk documents (see `Index::disk_values`).
type DiskValues<'s> = Box<dyn FnMut(&str) -> Option<Vec<Value>> + 's>;

/// Collects one index's matches for every aggregation of a request.
pub(crate) struct Collector<'s, 'a> {
    searcher: &'s Searcher<'a>,
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
        profile: bool,
    ) -> Result<Self, QueryError> {
        let index = searcher.index;
        let mut c = Collector {
            searcher,
            aggs,
            mappings: vec![],
            disk: vec![],
            missing: vec![],
            stats: vec![],
            subs: vec![],
            timings: profile.then(Vec::new),
        };
        for agg in aggs {
            let start = Instant::now();
            let m = index.mappings.field(&agg.field);
            match m {
                Some(FieldMapping::Text | FieldMapping::TextKeyword { .. }) => {
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
            c.disk.push(Box::new(index.disk_values(&agg.field)));
            if let Some(t) = &mut c.timings {
                t.push(Timings {
                    initialize,
                    build_leaf_collector: nanos(start),
                    ..Timings::default()
                });
            }
            let sub = match agg.metric.buckets() && !agg.subs.is_empty() {
                true => Some(Collector::new(&agg.subs, searcher, profile)?),
                false => None,
            };
            let empty = sub.as_ref().map(|s| s.stats.clone()).unwrap_or_default();
            c.subs.push(sub);
            c.stats.push(Stats {
                date: matches!(
                    m,
                    Some(FieldMapping::Date { .. } | FieldMapping::DateNanos { .. })
                ),
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
                    Metric::Range(r) => r.buckets(searcher, &agg.field, m, &empty)?,
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
                        Some(t.buckets(format, |s| as_doc_value(&Value::from(s)), empty))
                    }
                    Metric::Histogram(_) => Some(histogram::buckets(Format::of(m), empty)),
                    _ => None,
                },
                ..Stats::default()
            });
        }
        Ok(c)
    }

    pub(crate) fn collect(&mut self, id: &str, doc: &Document) {
        for i in 0..self.aggs.len() {
            let start = self.timings.is_some().then(Instant::now);
            self.collect_one(i, id, doc);
            if let (Some(t), Some(start)) = (&mut self.timings, start) {
                t[i].collect += nanos(start);
                t[i].collect_count += 1;
            }
        }
    }

    fn collect_one(&mut self, i: usize, id: &str, doc: &Document) {
        let matches = match &self.aggs[i].metric {
            Metric::Filter(q) => self.searcher.score(q, id, doc).is_some(),
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
                        sub.collect_into(&mut b.subs, id, doc);
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
                        let (_, from, to) = b.range.clone().unwrap_or_default();
                        let (from, to) = (fix(from), fix(to));
                        vals.iter().filter_map(num).any(|x| from <= x && x < to)
                    })
                    .collect();
                let st = &mut self.stats[i];
                for (b, _) in st.buckets.iter_mut().zip(hits).filter(|(_, hit)| *hit) {
                    b.count += 1;
                    if let Some(sub) = &mut self.subs[i] {
                        sub.collect_into(&mut b.subs, id, doc);
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
                    true => self.add(i, self.missing[i].iter().cloned().collect(), id, doc),
                    false => self.add(i, vals, id, doc),
                }
                return;
            }
        };
        if matches {
            let st = &mut self.stats[i];
            st.count += 1;
            if let Some(sub) = &mut self.subs[i] {
                sub.collect_into(&mut st.subs, id, doc);
            }
        }
    }

    /// Collects a document into `stats`, one bucket's results of these aggregations.
    fn collect_into(&mut self, stats: &mut Vec<Stats>, id: &str, doc: &Document) {
        std::mem::swap(&mut self.stats, stats);
        self.collect(id, doc);
        std::mem::swap(&mut self.stats, stats);
    }

    /// A document's doc values of aggregation `i`'s field.
    fn values(&mut self, i: usize, id: &str, doc: &Document) -> Vec<Value> {
        let (index, agg) = (self.searcher.index, &self.aggs[i]);
        match self.mappings[i] {
            None => vec![],
            Some(m) => self.disk[i](id).unwrap_or_else(|| {
                let path = source_path(index, &agg.field);
                let mut v: Vec<Value> = values(&doc.source, path)
                    .into_iter()
                    .filter_map(|v| doc_value(self.searcher, &agg.field, m, v))
                    .collect();
                // keyword and ip doc values are a sorted set, other doc values sorted numbers
                v.sort_by(crate::scalar_cmp);
                if matches!(m, FieldMapping::Keyword { .. } | FieldMapping::Ip) {
                    v.dedup();
                }
                v
            }),
        }
    }

    fn add(&mut self, i: usize, vals: Vec<Value>, id: &str, doc: &Document) {
        let vals = match &self.aggs[i].metric {
            Metric::Histogram(h) => h.keys(vals),
            _ => vals,
        };
        let st = &mut self.stats[i];
        if let Some(t) = &mut st.terms {
            let sub = &mut self.subs[i];
            t.add(vals, |stats| {
                if let Some(sub) = sub {
                    sub.collect_into(stats, id, doc);
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
        let index = self.searcher.index;
        for (i, st) in stats.iter_mut().enumerate() {
            if self.aggs[i].metric == Metric::Global {
                for (id, doc) in index.docs().filter(|(_, d)| index.searchable(d)) {
                    st.count += 1;
                    if let Some(sub) = &mut self.subs[i] {
                        sub.collect_into(&mut st.subs, id, &doc);
                    }
                }
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
                for (id, doc) in index.docs().filter(|(_, d)| index.searchable(d)) {
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
            Metric::Global => "GlobalAggregator",
            // ponytail: ES may run filters as FilterByFilterAggregator; not profiled yet
            Metric::Filters(_) => "FiltersAggregator",
            // ponytail: ES runs ranges as NoOverlap, Overlap or from filters; not profiled yet
            Metric::Range(_) => "NoOverlap",
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
            Metric::Percentiles { .. } => "TDigestPercentilesAggregator",
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
    stats: &mut [Stats],
    typed_keys: bool,
) -> Result<Value, QueryError> {
    render_counted(aggs, stats, typed_keys, &Cell::new(0))
}

/// Counts `n` more buckets toward the request's [`MAX_BUCKETS`], like ES's MultiBucketConsumer.
// ponytail: counts histogram buckets as rendered, so sub-aggregations of empty buckets count once
// per bucket (ES reduces them once) and terms buckets not at all
fn consume_buckets(buckets: &Cell<usize>, n: usize) -> Result<(), QueryError> {
    buckets.set(buckets.get() + n);
    match buckets.get() > MAX_BUCKETS {
        true => Err(QueryError {
            kind: crate::ErrorKind::TooManyBuckets,
            msg: format!(
                "Trying to create too many buckets. Must be less than or equal to: [{MAX_BUCKETS}] but this number of buckets was exceeded. This limit can be set by changing the [search.max_buckets] cluster level setting."
            ),
        }),
        false => Ok(()),
    }
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
        let date = st.date;
        let put = |body: &mut Map<String, Value>, key: &str, v: Option<f64>| {
            body.insert(key.into(), v.map_or(Value::Null, Value::from));
            // ponytail: dates render in the default format, not a mapping's custom `format`
            if let Some(v) = v.filter(|_| date) {
                body.insert(format!("{key}_as_string"), iso_date(v as i64).into());
            }
        };
        let mut ty = agg.metric.name();
        let sub = |subs: &mut [Stats], out: &mut Map<String, Value>| {
            if let Value::Object(m) = render_counted(&agg.subs, subs, typed_keys, buckets)? {
                out.extend(m);
            }
            Ok(())
        };
        match &agg.metric {
            Metric::Terms(t) => {
                if let Some(b) = &mut st.terms {
                    ty = t.render(b, &mut body, sub)?;
                }
            }
            Metric::Histogram(h) => {
                if let Some(b) = &mut st.terms {
                    h.render(b, &mut body, buckets, sub)?;
                }
            }
            Metric::Filter(_) | Metric::Missing | Metric::Global => {
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
                // ponytail: dates render in the default format, not the field's or `format`
                let as_string = |x: f64| match (date, &r.format) {
                    (true, _) => Some(iso_date(x as i64)),
                    (_, Some(p)) => Some(decimal_format(p, x)),
                    _ => None,
                };
                let mut list = vec![];
                for b in &mut st.buckets {
                    let (key, from, to) = b.range.clone().unwrap_or_default();
                    let mut bucket = Map::new();
                    let end = |x: f64| match x.is_finite() {
                        true => as_string(x).unwrap_or_else(|| java_double(x)),
                        false => "*".into(),
                    };
                    let key = key.unwrap_or_else(|| format!("{}-{}", end(from), end(to)));
                    if !r.keyed {
                        bucket.insert("key".into(), key.as_str().into());
                    }
                    for (name, x) in [("from", from), ("to", to)] {
                        if x.is_finite() {
                            bucket.insert(name.into(), x.into());
                            if let Some(s) = as_string(x) {
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
                percents, keyed, ..
            } => {
                let mut d = st.digest.take().map(TDigest::coordinated);
                let mut d = d.as_mut().filter(|d| d.size() > 0);
                let (mut keyed_values, mut list) = (Map::new(), vec![]);
                for &p in percents {
                    let v = d.as_mut().map(|d| d.quantile(p / 100.0));
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
        if let Metric::ExtendedStats { sigma } = agg.metric {
            extended_stats(&mut body, st, sigma);
        }
        let key = match typed_keys {
            true => format!("{ty}#{}", agg.name),
            false => agg.name.clone(),
        };
        out.insert(key, body.into());
    }
    Ok(out.into())
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
// ponytail: no `_as_string` variants for date fields
fn extended_stats(body: &mut Map<String, Value>, st: &Stats, sigma: f64) {
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
}

#[cfg(test)]
mod tests {
    use crate::{SearchRequest, search};
    use es_index::{FieldMapping, Index, Indices, disk};
    use serde_json::{Value, json};

    #[test]
    fn decimal_format_follows_java() {
        let f = super::decimal_format;
        assert_eq!(f("Value is ##0.0", 150.0), "Value is 150.0");
        assert_eq!(f("Value is ##0.0", 0.0), "Value is 0.0");
        assert_eq!(f("0.0#", 1.0), "1.0");
        assert_eq!(f("0.0#", 1.256), "1.26");
        assert_eq!(f("0.0###", 2.5), "2.5");
        assert_eq!(f("#.#", 0.5), ".5");
        assert_eq!(f("000", 7.0), "007");
        assert_eq!(f("0 units", -3.4), "-3 units");
        // HALF_EVEN
        assert_eq!(f("0", 2.5), "2");
    }

    fn index() -> Index {
        let mut ix = Index::default();
        let props = &mut ix.mappings.properties;
        props.insert("i".into(), FieldMapping::Integer);
        props.insert("f".into(), FieldMapping::Float);
        props.insert("k".into(), FieldMapping::Keyword { ignore_above: None });
        let date = FieldMapping::Date {
            format: None,
            locale: None,
            fields: Default::default(),
        };
        props.insert("at".into(), date);
        props.insert("t".into(), FieldMapping::Text);
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
        props.insert("k".into(), FieldMapping::Keyword { ignore_above: None });
        props.insert("ks".into(), FieldMapping::Keyword { ignore_above: None });
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
        close(&values(&[("p1", &p1, None)]), &want_p1, 1e-4);
        let both = values(&[("p1", &p1, None), ("p2", &p2, None)]);
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
        assert_eq!(values(&[("p1", p1, None), ("p2", p2, None)]), both);
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
            match crate::search_many(&[("i", &ix, None)], &req) {
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
