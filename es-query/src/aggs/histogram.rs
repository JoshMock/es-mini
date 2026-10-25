//! The `histogram` bucket aggregation: each value falls in the bucket of
//! `floor((value - offset) / interval)`; the reduce fills empty buckets like ES's
//! InternalHistogram when `min_doc_count` is 0.

use super::Stats;
use super::terms::{Buckets, Format};
use crate::{QueryError, int, num};
use es_index::iso_date;
use serde_json::{Map, Value};
use std::cell::Cell;

/// InternalMultiBucketAggregation.REPORT_EMPTY_EVERY.
const REPORT_EMPTY_EVERY: usize = 10_000;

/// A `histogram` request.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Histogram {
    interval: f64,
    offset: f64,
    min_doc_count: u64,
    keyed: bool,
    extended_bounds: Bounds,
    hard_bounds: Option<Bounds>,
}

/// ES's DoubleBounds: either end may be open.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
struct Bounds {
    min: Option<f64>,
    max: Option<f64>,
}

impl Bounds {
    fn parse(o: &Value, field: &str) -> Result<Bounds, QueryError> {
        let o =
            (o.as_object()).ok_or_else(|| format!("Expected [START_OBJECT] under [{field}]"))?;
        let mut b = Bounds::default();
        for (k, v) in o {
            let bad =
                || QueryError::x_content(format!("[double_bounds] failed to parse field [{k}]"));
            let x = match v {
                Value::Null => None,
                v => Some(num(v).ok_or_else(bad)?),
            };
            match k.as_str() {
                "min" => b.min = x,
                "max" => b.max = x,
                _ => return Err(format!("[double_bounds] unknown field [{k}]").into()),
            }
        }
        for (end, x) in [("min", b.min), ("max", b.max)] {
            if let Some(x) = x.filter(|x| !x.is_finite()) {
                let x = super::java_double(x);
                return Err(QueryError::illegal(format!(
                    "{end} bound must be finite, got: {x}"
                )));
            }
        }
        if let (Some(min), Some(max)) = (b.min, b.max)
            && max < min
        {
            let (min, max) = (super::java_double(min), super::java_double(max));
            return Err(QueryError::illegal(format!(
                "max bound [{max}] must be greater than min bound [{min}]"
            )));
        }
        Ok(b)
    }

    fn contains(&self, x: f64) -> bool {
        self.min.is_none_or(|m| x >= m) && self.max.is_none_or(|m| x <= m)
    }
}

impl Histogram {
    pub(super) fn parse(name: &str, params: &Map<String, Value>) -> Result<Histogram, QueryError> {
        let mut h = Histogram {
            interval: 0.0,
            offset: 0.0,
            min_doc_count: 0,
            keyed: false,
            extended_bounds: Bounds::default(),
            hard_bounds: None,
        };
        for (k, v) in params {
            let bad = || QueryError::x_content(format!("[histogram] failed to parse field [{k}]"));
            match k.as_str() {
                "field" | "missing" => {}
                "interval" => {
                    h.interval = num(v).ok_or_else(bad)?;
                    if h.interval <= 0.0 {
                        return Err(QueryError::illegal(format!(
                            "[interval] must be >0 for histogram aggregation [{name}]"
                        )));
                    }
                }
                "offset" => h.offset = num(v).ok_or_else(bad)?,
                "keyed" => h.keyed = v.as_bool().ok_or_else(bad)?,
                "min_doc_count" => {
                    let n = int(v).ok_or_else(bad)?;
                    h.min_doc_count = u64::try_from(n).map_err(|_| {
                        QueryError::illegal(format!(
                            "[minDocCount] must be greater than or equal to 0. Found [{n}] in [{name}]"
                        ))
                    })?;
                }
                "extended_bounds" => h.extended_bounds = Bounds::parse(v, k)?,
                "hard_bounds" => h.hard_bounds = Some(Bounds::parse(v, k)?),
                _ => return Err(format!("[histogram] unknown field [{k}]").into()),
            }
        }
        if h.interval <= 0.0 {
            let interval = super::java_double(h.interval);
            return Err(QueryError::illegal(format!(
                "interval must be positive, got: {interval}"
            )));
        }
        if let Some(hard) = h.hard_bounds {
            let ext = h.extended_bounds;
            let outside = |h: Option<f64>, e: Option<f64>, f: fn(f64, f64) -> bool| matches!((h, e), (Some(h), Some(e)) if f(h, e));
            if outside(hard.max, ext.max, |h, e| h < e) || outside(hard.min, ext.min, |h, e| h > e)
            {
                // DoubleBounds.toString leaves out open ends; the extended ones print null
                let end = |x: Option<f64>, open: &str| x.map_or(open.into(), super::java_double);
                return Err(QueryError::illegal(format!(
                    "Extended bounds have to be inside hard bounds, hard bounds: [{}--{}], extended bounds: [{}--{}]",
                    end(hard.min, ""),
                    end(hard.max, ""),
                    end(ext.min, "null"),
                    end(ext.max, "null"),
                )));
            }
        }
        Ok(h)
    }

    /// The bucket keys of a document's doc values (sorted, so equal keys are adjacent), as
    /// NumericHistogramAggregator collects them.
    pub(super) fn keys(&self, vals: Vec<Value>) -> Vec<Value> {
        let mut keys: Vec<f64> = (vals.iter().filter_map(num))
            .map(|x| ((x - self.offset) / self.interval).floor())
            .filter(|k| {
                self.hard_bounds
                    .is_none_or(|b| b.contains(k * self.interval))
            })
            .map(|k| k * self.interval + self.offset)
            .collect();
        keys.dedup();
        keys.into_iter().map(Value::from).collect()
    }

    /// The key of the bucket holding `key`.
    fn round(&self, key: f64) -> f64 {
        ((key - self.offset) / self.interval).floor() * self.interval + self.offset
    }

    fn next(&self, key: f64) -> f64 {
        self.round(key + self.interval + self.interval / 2.0)
    }

    /// The reduced result; `sub` renders a bucket's sub-aggregations into it.
    pub(super) fn render(
        &self,
        b: &mut Buckets,
        body: &mut Map<String, Value>,
        buckets: &Cell<usize>,
        mut sub: impl FnMut(&mut [Stats], &mut Map<String, Value>) -> Result<(), QueryError>,
    ) -> Result<(), QueryError> {
        let mut v: Vec<(f64, u64, Vec<Stats>)> = std::mem::take(&mut b.counts)
            .into_iter()
            .map(|(k, bucket)| (k.f64(), bucket.count, bucket.subs))
            .filter(|b| b.1 >= self.min_doc_count)
            .collect();
        super::consume_buckets(buckets, v.len())?;
        if self.min_doc_count == 0 {
            // InternalHistogram.iterateEmptyBuckets
            let (min, max) = (self.extended_bounds.min, self.extended_bounds.max);
            let mut keys = vec![];
            // ES counts empty buckets before and again (per 10,000) while adding them
            let mut fill = |from: f64, below: &dyn Fn(f64) -> bool| {
                let mut k = from;
                while below(k) {
                    keys.push(k);
                    let n = keys.len();
                    let again = if n % REPORT_EMPTY_EVERY == 0 {
                        REPORT_EMPTY_EVERY
                    } else {
                        0
                    };
                    super::consume_buckets(buckets, 1 + again)?;
                    k = self.next(k);
                }
                Ok::<_, QueryError>(())
            };
            let max = max.unwrap_or(f64::NEG_INFINITY);
            match (v.first(), v.last()) {
                (Some(first), Some(last)) => {
                    if let Some(min) = min {
                        fill(self.round(min), &|k| k < first.0)?;
                    }
                    for w in v.windows(2) {
                        fill(self.next(w[0].0), &|k| k < w[1].0)?;
                    }
                    fill(self.next(last.0), &|k| k <= max)?;
                }
                _ => fill(self.round(min.unwrap_or(f64::INFINITY)), &|k| k <= max)?,
            }
            v.extend(keys.into_iter().map(|k| (k, 0, b.empty.clone())));
            v.sort_by(|a, b| a.0.total_cmp(&b.0));
        }
        let rendered = v.into_iter().map(|(key, count, mut subs)| {
            let mut out = Map::new();
            let key_as_string = match b.format {
                Format::Date => Some(iso_date(key as u64)),
                _ => None,
            };
            if let Some(s) = &key_as_string {
                out.insert("key_as_string".into(), s.as_str().into());
            }
            out.insert("key".into(), key.into());
            out.insert("doc_count".into(), count.into());
            sub(&mut subs, &mut out)?;
            Ok((
                key_as_string.unwrap_or_else(|| super::java_double(key)),
                out,
            ))
        });
        let rendered = match self.keyed {
            true => (rendered.map(|b| b.map(|(k, b)| (k, Value::Object(b)))))
                .collect::<Result<Map<_, _>, QueryError>>()?
                .into(),
            false => (rendered.map(|b| b.map(|(_, b)| Value::Object(b))))
                .collect::<Result<Vec<_>, QueryError>>()?
                .into(),
        };
        body.insert("buckets".into(), rendered);
        Ok(())
    }
}

/// A shard's empty buckets of a field with `format`; `empty` is a new bucket's
/// sub-aggregation results.
pub(super) fn buckets(format: Format, empty: Vec<Stats>) -> Buckets {
    let mut b = Buckets::default();
    b.format = format;
    b.empty = empty;
    b
}

#[cfg(test)]
mod tests {
    use crate::{SearchRequest, search_many};
    use es_index::{FieldMapping, Index};
    use serde_json::{Value, json};

    fn index(numbers: &[Value]) -> Index {
        let mut ix = Index::default();
        let props = &mut ix.mappings.properties;
        props.insert("n".into(), FieldMapping::Integer);
        let date = FieldMapping::Date {
            format: None,
            locale: None,
        };
        props.insert("at".into(), date);
        for (i, n) in numbers.iter().enumerate() {
            ix.put(&i.to_string(), json!({"n": n}));
        }
        ix
    }

    fn run(targets: &[&Index], aggs: Value) -> Result<Value, String> {
        let req = SearchRequest::parse(&json!({"size": 0, "aggs": aggs})).map_err(|e| e.msg)?;
        let targets: Vec<_> = targets.iter().map(|ix| ("i", *ix, None)).collect();
        let res = search_many(&targets, &req).map_err(|e| e.msg)?;
        Ok(res.aggregations.unwrap())
    }

    /// The `h` histogram's buckets as `[key, doc_count]` pairs.
    fn histo(numbers: &[Value], params: Value) -> Value {
        let got = run(&[&index(numbers)], json!({"h": {"histogram": params}})).unwrap();
        let buckets = got["h"]["buckets"].as_array().unwrap().iter();
        buckets.map(|b| json!([b["key"], b["doc_count"]])).collect()
    }

    #[test]
    fn histogram_buckets_values_by_interval_and_fills_gaps() {
        let n = [json!(1), json!(51), json!(61), json!(71)];
        assert_eq!(
            histo(&n, json!({"field": "n", "interval": 20})),
            json!([[0.0, 1], [20.0, 0], [40.0, 1], [60.0, 2]])
        );
        assert_eq!(
            histo(
                &n,
                json!({"field": "n", "interval": 20, "min_doc_count": 1})
            ),
            json!([[0.0, 1], [40.0, 1], [60.0, 2]])
        );
        assert_eq!(
            histo(
                &n,
                json!({"field": "n", "interval": "20", "min_doc_count": 2})
            ),
            json!([[60.0, 2]])
        );
        // a document's values count once per bucket
        assert_eq!(
            histo(&[json!([1, 2, 30])], json!({"field": "n", "interval": 20})),
            json!([[0.0, 1], [20.0, 1]])
        );
    }

    #[test]
    fn histogram_offset_and_bounds() {
        let n = [json!(11), json!(51), json!(101), json!(151)];
        assert_eq!(
            histo(&n, json!({"field": "n", "interval": 50, "offset": 20})),
            json!([[-30.0, 1], [20.0, 1], [70.0, 1], [120.0, 1]])
        );
        let ext = json!({"field": "n", "interval": 50, "extended_bounds": {"min": 0, "max": 200}});
        assert_eq!(
            histo(&[json!(52), json!(51), json!(101), json!(150)], ext),
            json!([[0.0, 0], [50.0, 2], [100.0, 1], [150.0, 1], [200.0, 0]])
        );
        // hard bounds drop values outside them (tested on the unshifted key)
        let hard = json!({"field": "n", "interval": 50, "hard_bounds": {"min": 50, "max": 100}});
        assert_eq!(histo(&n, hard), json!([[50.0, 1], [100.0, 1]]));
        // extended bounds alone fill an empty result
        let ext = json!({"field": "n", "interval": 5, "extended_bounds": {"min": 2, "max": 11}});
        assert_eq!(histo(&[], ext), json!([[0.0, 0], [5.0, 0], [10.0, 0]]));
    }

    #[test]
    fn histogram_missing_unmapped_keyed_and_dates() {
        let n = [json!(51), json!(null)];
        assert_eq!(
            histo(&n, json!({"field": "n", "interval": 50, "missing": 0})),
            json!([[0.0, 1], [50.0, 1]])
        );
        let unmapped = Index::default();
        let got = run(
            &[&index(&n), &unmapped],
            json!({"h": {"histogram": {"field": "n", "interval": 50, "keyed": true}}}),
        );
        assert_eq!(
            got.unwrap()["h"],
            json!({"buckets": {"50.0": {"key": 50.0, "doc_count": 1}}})
        );
        let mut ix = index(&[]);
        ix.put("d", json!({"at": "2021-05-01T10:00:00Z"}));
        let got = run(
            &[&ix],
            json!({"h": {"histogram": {"field": "at", "interval": 3_600_000}}}),
        );
        assert_eq!(
            got.unwrap()["h"]["buckets"],
            json!([{"key_as_string": "2021-05-01T10:00:00.000Z", "key": 1_619_863_200_000.0,
                "doc_count": 1}])
        );
    }

    #[test]
    fn histogram_buckets_hold_sub_aggregations() {
        let got = run(
            &[&index(&[json!(1), json!(3), json!(12)])],
            json!({"h": {"histogram": {"field": "n", "interval": 5},
                "aggs": {"m": {"max": {"field": "n"}}}}}),
        );
        assert_eq!(
            got.unwrap()["h"]["buckets"],
            json!([
                {"key": 0.0, "doc_count": 2, "m": {"value": 3.0}},
                {"key": 5.0, "doc_count": 0, "m": {"value": null}},
                {"key": 10.0, "doc_count": 1, "m": {"value": 12.0}},
            ])
        );
    }

    #[test]
    fn histogram_fails_past_max_buckets() {
        let too_many = "Trying to create too many buckets. Must be less than or equal to: [65536] but this number of buckets was exceeded. This limit can be set by changing the [search.max_buckets] cluster level setting.";
        let ext = |max: f64| {
            let h = json!({"field": "n", "interval": 1, "extended_bounds": {"min": 0, "max": max}});
            run(&[&index(&[json!(1)])], json!({"h": {"histogram": h}}))
        };
        // like ES, empty buckets count twice once there are 10,000 of them
        assert_eq!(ext(1e15).unwrap_err(), too_many);
        assert_eq!(ext(65_535.0).unwrap_err(), too_many);
        let ok = ext(30_000.0).unwrap();
        assert_eq!(ok["h"]["buckets"].as_array().unwrap().len(), 30_001);
        // the limit holds across a request: nested histograms multiply (stricter than ES, which
        // counts the sub-aggregations of empty buckets once)
        let nested = json!({"h": {"histogram": {"field": "n", "interval": 1,
            "extended_bounds": {"min": 0, "max": 1000}},
            "aggs": {"g": {"histogram": {"field": "n", "interval": 1,
                "extended_bounds": {"min": 0, "max": 1000}}}}}});
        assert_eq!(run(&[&index(&[json!(1)])], nested).unwrap_err(), too_many);
        // a key too large to step past still ends
        let stuck =
            json!({"field": "n", "interval": 1, "extended_bounds": {"min": 1e17, "max": 2e17}});
        let got = run(&[&index(&[])], json!({"h": {"histogram": stuck}}));
        assert_eq!(got.unwrap_err(), too_many);
    }

    #[test]
    fn histogram_rejects_bad_requests() {
        let err =
            |params: Value| run(&[&index(&[])], json!({"h": {"histogram": params}})).unwrap_err();
        assert_eq!(
            err(json!({"field": "n", "interval": 0})),
            "[interval] must be >0 for histogram aggregation [h]"
        );
        assert_eq!(
            err(json!({"field": "n"})),
            "interval must be positive, got: 0.0"
        );
        assert_eq!(
            err(json!({"field": "n", "interval": 1, "min_doc_count": -1})),
            "[minDocCount] must be greater than or equal to 0. Found [-1] in [h]"
        );
        assert_eq!(
            err(json!({"field": "n", "interval": 1, "extended_bounds": {"min": 5, "max": 1}})),
            "max bound [1.0] must be greater than min bound [5.0]"
        );
        assert_eq!(
            err(
                json!({"field": "n", "interval": 1, "hard_bounds": {"max": 5},
                "extended_bounds": {"min": 0, "max": 10}})
            ),
            "Extended bounds have to be inside hard bounds, hard bounds: [--5.0], extended bounds: [0.0--10.0]"
        );
        assert_eq!(
            err(json!({"field": "n", "interval": 1, "nope": 1})),
            "[histogram] unknown field [nope]"
        );
    }
}
