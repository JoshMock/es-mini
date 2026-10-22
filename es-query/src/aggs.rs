//! Aggregations (`aggs`/`aggregations` in a search body): parsed into named aggregations,
//! collected over every match of each searched index (one shard each), then reduced across
//! indices and rendered like Elasticsearch.
//!
//! Values come from what the field's doc values hold: read from disk for documents in
//! segments, and converted the same way from the source for documents in memory.

use crate::{QueryError, Searcher, fielddata_disabled, int, num, source_path};
use es_index::{Document, FieldMapping, Index, iso_date, postings::values};
use serde_json::{Map, Value};

#[derive(Debug, Clone, Copy, PartialEq)]
enum Metric {
    Avg,
    Sum,
    Min,
    Max,
    ValueCount,
    Stats,
    ExtendedStats { sigma: f64 },
}

impl Metric {
    fn name(self) -> &'static str {
        match self {
            Metric::Avg => "avg",
            Metric::Sum => "sum",
            Metric::Min => "min",
            Metric::Max => "max",
            Metric::ValueCount => "value_count",
            Metric::Stats => "stats",
            Metric::ExtendedStats { .. } => "extended_stats",
        }
    }
}

/// One named aggregation of a request.
#[derive(Debug, Clone, PartialEq)]
pub struct Agg {
    name: String,
    meta: Option<Value>,
    metric: Metric,
    field: String,
    missing: Option<Value>,
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
    let (mut meta, mut kind, mut subs) = (None, None, false);
    for (k, v) in body {
        match k.as_str() {
            "meta" => meta = Some(v.clone()),
            "aggs" | "aggregations" => {
                parse(v)?;
                subs = true;
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
    let mut metric = match t {
        "avg" => Metric::Avg,
        "sum" => Metric::Sum,
        "min" => Metric::Min,
        "max" => Metric::Max,
        "value_count" => Metric::ValueCount,
        "stats" => Metric::Stats,
        "extended_stats" => Metric::ExtendedStats { sigma: 2.0 },
        _ => return Err(format!("Unknown aggregation type [{t}]").into()),
    };
    if subs {
        return Err(QueryError::illegal(format!(
            "Aggregator [{name}] of type [{t}] cannot accept sub-aggregations"
        )));
    }
    let params = params
        .as_object()
        .ok_or_else(|| format!("Expected [START_OBJECT] under [{t}]"))?;
    for (k, v) in params {
        match (k.as_str(), &mut metric) {
            ("field" | "missing", _) => {}
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
    })
}

/// What a metric aggregation accumulates: every metric is read from it.
#[derive(Debug, Clone, Copy)]
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
    fn merge(&mut self, o: &Stats) {
        self.count += o.count;
        kahan(&mut self.sum, &mut self.delta, o.sum);
        kahan(&mut self.sum_sq, &mut self.sq_delta, o.sum_sq);
        self.min = self.min.min(o.min);
        self.max = self.max.max(o.max);
        self.date |= o.date;
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
        M::Date { .. } => s.instant(field).and_then(|i| i(v)).map(Value::from),
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
        M::Text | M::TextKeyword { .. } => None,
    }
}

/// The mapping of `field`, including a `keyword` multi-field of a text field.
fn mapping<'a>(index: &'a Index, field: &str) -> Option<&'a FieldMapping> {
    let props = &index.mappings.properties;
    props.get(field).or_else(|| {
        let (base, sub) = field.rsplit_once('.')?;
        match props.get(base)? {
            FieldMapping::TextKeyword { fields } => fields.get(sub),
            _ => None,
        }
    })
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
    pub(crate) stats: Vec<Stats>,
}

impl<'s, 'a> Collector<'s, 'a> {
    pub(crate) fn new(aggs: &'s [Agg], searcher: &'s Searcher<'a>) -> Result<Self, QueryError> {
        let index = searcher.index;
        let mut c = Collector {
            searcher,
            aggs,
            mappings: vec![],
            disk: vec![],
            missing: vec![],
            stats: vec![],
        };
        for agg in aggs {
            let m = mapping(index, &agg.field);
            match m {
                Some(FieldMapping::Text | FieldMapping::TextKeyword { .. }) => {
                    return Err(fielddata_disabled(&agg.field));
                }
                Some(FieldMapping::Keyword { .. }) if agg.metric != Metric::ValueCount => {
                    return Err(QueryError::illegal(format!(
                        "Field [{}] of type [keyword] is not supported for aggregation [{}]",
                        agg.field,
                        agg.metric.name()
                    )));
                }
                _ => {}
            }
            c.missing.push(match m {
                Some(m) => {
                    (agg.missing.as_ref()).and_then(|v| doc_value(searcher, &agg.field, m, v))
                }
                None => agg.missing.clone(),
            });
            c.mappings.push(m);
            c.disk.push(Box::new(index.disk_values(&agg.field)));
            c.stats.push(Stats {
                date: matches!(m, Some(FieldMapping::Date { .. })),
                ..Stats::default()
            });
        }
        Ok(c)
    }

    pub(crate) fn collect(&mut self, id: &str, doc: &Document) {
        let index = self.searcher.index;
        for (i, agg) in self.aggs.iter().enumerate() {
            let vals = match self.mappings[i] {
                None => vec![],
                Some(m) => self.disk[i](id).unwrap_or_else(|| {
                    let path = source_path(index, &agg.field);
                    let mut v: Vec<Value> = values(&doc.source, path)
                        .into_iter()
                        .filter_map(|v| doc_value(self.searcher, &agg.field, m, v))
                        .collect();
                    // keyword doc values are a sorted set
                    if matches!(m, FieldMapping::Keyword { .. }) {
                        v.sort_by_key(es_index::postings::text);
                        v.dedup();
                    }
                    v
                }),
            };
            match vals.is_empty() {
                true => self.add(i, self.missing[i].iter().cloned().collect()),
                false => self.add(i, vals),
            }
        }
    }

    fn add(&mut self, i: usize, vals: Vec<Value>) {
        let st = &mut self.stats[i];
        for v in vals {
            st.count += 1;
            if self.aggs[i].metric == Metric::ValueCount {
                continue;
            }
            let Some(x) = num(&v).or_else(|| v.as_bool().map(f64::from)) else {
                continue;
            };
            st.add(x);
            st.min = st.min.min(x);
            st.max = st.max.max(x);
        }
    }
}

/// Reduces each index's results into `total` (one entry per aggregation).
pub(crate) fn reduce(total: &mut [Stats], shard: &[Stats]) {
    total.iter_mut().zip(shard).for_each(|(t, s)| t.merge(s));
}

/// The `aggregations` object of a search response; with `typed_keys`, names are prefixed with
/// their type (`avg#name`).
pub(crate) fn render(aggs: &[Agg], stats: &[Stats], typed_keys: bool) -> Value {
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
                body.insert(format!("{key}_as_string"), iso_date(v as u64).into());
            }
        };
        match agg.metric {
            Metric::Avg => put(&mut body, "value", avg),
            Metric::Sum => put(&mut body, "value", Some(st.sum)),
            Metric::Min => put(&mut body, "value", min),
            Metric::Max => put(&mut body, "value", max),
            Metric::ValueCount => {
                body.insert("value".into(), st.count.into());
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
            true => format!("{}#{}", agg.metric.name(), agg.name),
            false => agg.name.clone(),
        };
        out.insert(key, body.into());
    }
    out.into()
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

    fn index() -> Index {
        let mut ix = Index::default();
        let props = &mut ix.mappings.properties;
        props.insert("i".into(), FieldMapping::Integer);
        props.insert("f".into(), FieldMapping::Float);
        props.insert("k".into(), FieldMapping::Keyword { ignore_above: None });
        let date = FieldMapping::Date {
            format: None,
            locale: None,
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
            err(json!({"a": {"meta": {}}})),
            "Missing definition for aggregation [a]"
        );
    }
}
