//! The `histogram` and `date_histogram` bucket aggregations: each value falls in the bucket of
//! `floor((value - offset) / interval)` (or of its calendar unit); the reduce fills empty buckets
//! like ES's InternalHistogram and InternalDateHistogram when `min_doc_count` is 0.

use super::Stats;
use super::terms::{Buckets, Format};
use crate::{MAX_BUCKETS, QueryError, int, num};
use es_index::tz::TimeZone;
use es_index::{civil_from_days, days_from_civil, iso_date};
use serde_json::{Map, Value};
use std::cell::Cell;

/// InternalMultiBucketAggregation.REPORT_EMPTY_EVERY.
const REPORT_EMPTY_EVERY: usize = 10_000;

/// A `histogram` or `date_histogram` request.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Histogram {
    /// Whether this is a `date_histogram`: its keys are epoch millis.
    pub(super) date: bool,
    /// The fixed interval (in millis for dates) unless `calendar`.
    interval: f64,
    calendar: Option<Unit>,
    offset: f64,
    min_doc_count: u64,
    keyed: bool,
    extended_bounds: Bounds,
    hard_bounds: Option<Bounds>,
    /// A DecimalFormat pattern for numeric keys.
    format: Option<String>,
    /// A date_histogram's `time_zone` other than UTC.
    time_zone: Option<TimeZone>,
}

/// A `calendar_interval`'s Rounding.DateTimeUnit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Unit {
    Second,
    Minute,
    Hour,
    Day,
    Week,
    Month,
    Quarter,
    Year,
}

impl Unit {
    /// DateHistogramAggregationBuilder.DATE_FIELD_UNITS.
    // ponytail: no `years`/`months` (multiples of a unit)
    pub(crate) fn parse(s: &str) -> Option<Unit> {
        Some(match s {
            "second" | "1s" => Unit::Second,
            "minute" | "1m" => Unit::Minute,
            "hour" | "1h" => Unit::Hour,
            "day" | "1d" => Unit::Day,
            "week" | "1w" => Unit::Week,
            "month" | "1M" => Unit::Month,
            "quarter" | "1q" => Unit::Quarter,
            "year" | "1y" => Unit::Year,
            _ => return None,
        })
    }

    /// The start of the unit holding epoch millis `t`, or of the next one.
    pub(crate) fn round(self, t: i64, next: bool) -> i64 {
        const DAY: i64 = 86_400_000;
        let fixed = |ms: i64| t.div_euclid(ms) * ms + if next { ms } else { 0 };
        let (y, m, _) = civil_from_days(t.div_euclid(DAY));
        let month = |y: i64, m: i64| {
            days_from_civil(y + (m - 1).div_euclid(12), (m - 1).rem_euclid(12) + 1, 1) * DAY
        };
        let n = i64::from(next);
        match self {
            Unit::Second => fixed(1000),
            Unit::Minute => fixed(60_000),
            Unit::Hour => fixed(3_600_000),
            Unit::Day => fixed(DAY),
            // 1970-01-01 was a Thursday; weeks start on Monday
            Unit::Week => (t + 3 * DAY).div_euclid(7 * DAY) * 7 * DAY - 3 * DAY + 7 * DAY * n,
            Unit::Month => month(y, m + n),
            Unit::Quarter => month(y, (m - 1) / 3 * 3 + 1 + 3 * n),
            Unit::Year => month(y + n, 1),
        }
    }

    /// [`Unit::round`] in local time: ES's JavaTimeToMidnightRounding and
    /// JavaTimeNotToMidnightRounding.
    fn round_in(self, tz: &TimeZone, t: i64, next: bool) -> i64 {
        let ms = match self {
            Unit::Second => 1000,
            Unit::Minute => 60_000,
            Unit::Hour => 3_600_000,
            _ => {
                // the first instant of the local day the unit starts on
                let local = self.round(t + tz.offset(t), next);
                let first = tz.instants(local).first().copied();
                return first.unwrap_or_else(|| tz.gap_start(local));
            }
        };
        let round = |mut t: i64| loop {
            let local = (t + tz.offset(t)).div_euclid(ms) * ms;
            let truncated = tz.instants(local).into_iter().rev().find(|&i| i <= t);
            match (tz.previous_transition(t), truncated) {
                (Some(prev), Some(r)) if prev <= r => return r,
                (None, r) => return r.unwrap_or(local - tz.offset(t)),
                // a transition lies between: round down from just before it
                (Some(prev), _) => t = prev - 1,
            }
        };
        match next {
            false => round(t),
            true => match round(t + ms) {
                r if t < r => r,
                _ => round(t + 2 * ms),
            },
        }
    }
}

/// ES's TimeIntervalRounding.JavaTimeRounding.round: the latest local multiple of `interval` at
/// or before instant `t`.
fn round_interval(tz: &TimeZone, t: i64, interval: i64) -> i64 {
    let mut u = t;
    // ponytail: ES fails after 5,000 attempts; this gives up on the last one
    'attempt: for _ in 0..5000 {
        let local = (u + tz.offset(u)).div_euclid(interval) * interval;
        let instants = tz.instants(local);
        let Some(&earliest) = instants.first() else {
            return tz.gap_start(local);
        };
        let prev = tz.previous_transition(u + 1);
        for &i in instants.iter().rev() {
            if let Some(p) = prev.filter(|&p| i < p) {
                // rounding down across a transition: round from just before it instead
                u = p - 1;
                continue 'attempt;
            }
            if i <= u {
                return i;
            }
        }
        return earliest;
    }
    u
}

/// ES's TimeIntervalRounding.JavaTimeRounding.nextRoundingValue: a search for the smallest
/// rounded value above `t`'s.
fn next_interval(tz: &TimeZone, t: i64, interval: i64) -> i64 {
    let round = |t| round_interval(tz, t, interval);
    let prev = round(t);
    let (mut increment, mut from) = (interval, prev);
    for _ in 0..100 {
        from += increment;
        let rounded = round(from);
        if rounded <= prev {
            if increment < 0 {
                increment = -increment / 2;
            }
        } else if round(rounded - 1) > prev {
            if increment > 0 {
                increment = -increment / 2;
            }
        } else {
            return rounded;
        }
    }
    round(from)
}

/// ES's DoubleBounds (or LongBounds of dates): either end may be open.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
struct Bounds {
    min: Option<f64>,
    max: Option<f64>,
}

impl Bounds {
    fn parse(o: &Value, field: &str, date: bool) -> Result<Bounds, QueryError> {
        let o =
            (o.as_object()).ok_or_else(|| format!("Expected [START_OBJECT] under [{field}]"))?;
        let mut b = Bounds::default();
        for (k, v) in o {
            let bad =
                || QueryError::x_content(format!("[double_bounds] failed to parse field [{k}]"));
            let x = match v {
                Value::Null => None,
                // LongBounds takes numbers as millis
                Value::Number(n) if date => Some(n.as_i64().ok_or_else(bad)? as f64),
                // ponytail: dates parse in the default format, not the field's, which LongBounds
                // takes; that needs bounds resolved per index, after this request-time validation
                v if date => Some(es_index::date_millis(None, None, v).ok_or_else(bad)? as f64),
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

    /// Whether `x` is in the bounds; LongBounds exclude `max`.
    fn contains(&self, x: f64, date: bool) -> bool {
        self.min.is_none_or(|m| x >= m) && self.max.is_none_or(|m| x < m || !date && x == m)
    }
}

impl Histogram {
    pub(super) fn parse(
        name: &str,
        params: &Map<String, Value>,
        date: bool,
    ) -> Result<Histogram, QueryError> {
        let ty = if date { "date_histogram" } else { "histogram" };
        let mut h = Histogram {
            date,
            interval: 0.0,
            calendar: None,
            offset: 0.0,
            min_doc_count: 0,
            keyed: false,
            extended_bounds: Bounds::default(),
            hard_bounds: None,
            format: None,
            time_zone: None,
        };
        for (k, v) in params {
            let bad = || QueryError::x_content(format!("[{ty}] failed to parse field [{k}]"));
            // DateIntervalWrapper.setIntervalType
            let other = |set: &str| {
                QueryError::illegal(format!(
                    "Cannot use [{k}] with [{set}] configuration option."
                ))
            };
            match k.as_str() {
                "field" | "missing" => {}
                "calendar_interval" if date => {
                    let s = v.as_str().ok_or_else(bad)?;
                    if h.interval > 0.0 {
                        return Err(other("fixed_interval"));
                    }
                    h.calendar = Some(Unit::parse(s).ok_or_else(|| {
                        QueryError::illegal(format!(
                            "The supplied interval [{s}] could not be parsed as a calendar interval."
                        ))
                    })?);
                }
                "fixed_interval" if date => {
                    let s = v.as_str().ok_or_else(bad)?;
                    if h.calendar.is_some() {
                        return Err(other("calendar_interval"));
                    }
                    h.interval = time_millis(s).ok_or_else(|| {
                        QueryError::illegal(format!(
                            "failed to parse setting [date_histogram.fixedInterval] with value [{s}] as a time value: unit is missing or unrecognized"
                        ))
                    })?;
                }
                "offset" if date => {
                    h.offset = match v.as_str() {
                        Some(s) => {
                            let (sign, s) = match s.strip_prefix('-') {
                                Some(s) => (-1.0, s),
                                None => (1.0, s.strip_prefix('+').unwrap_or(s)),
                            };
                            sign * time_millis(s).ok_or_else(bad)?
                        }
                        None => int(v).ok_or_else(bad)? as f64,
                    }
                }
                "time_zone" if date => {
                    let tz = TimeZone::parse(v.as_str().ok_or_else(bad)?).ok_or_else(bad)?;
                    h.time_zone = Some(tz).filter(|tz| *tz != TimeZone::UTC);
                }
                "interval" if !date => {
                    h.interval = num(v).ok_or_else(bad)?;
                    if h.interval <= 0.0 {
                        return Err(QueryError::illegal(format!(
                            "[interval] must be >0 for histogram aggregation [{name}]"
                        )));
                    }
                }
                "offset" => h.offset = num(v).ok_or_else(bad)?,
                "format" => h.format = Some(v.as_str().ok_or_else(bad)?.into()),
                "keyed" => h.keyed = v.as_bool().ok_or_else(bad)?,
                "min_doc_count" => {
                    let n = int(v).ok_or_else(bad)?;
                    h.min_doc_count = u64::try_from(n).map_err(|_| {
                        QueryError::illegal(format!(
                            "[minDocCount] must be greater than or equal to 0. Found [{n}] in [{name}]"
                        ))
                    })?;
                }
                "extended_bounds" => h.extended_bounds = Bounds::parse(v, k, date)?,
                "hard_bounds" => h.hard_bounds = Some(Bounds::parse(v, k, date)?),
                _ => return Err(format!("[{ty}] unknown field [{k}]").into()),
            }
        }
        if date && h.interval <= 0.0 && h.calendar.is_none() {
            return Err(QueryError::illegal(
                "Invalid interval specified, must be non-null and non-empty",
            ));
        }
        if !date && h.interval <= 0.0 {
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
        // NumericHistogramAggregator tests hard bounds on the unshifted key, DateHistogramAggregator
        // on the rounded one
        let unshifted = |k: f64| match self.date {
            true => k,
            false => k - self.offset,
        };
        let mut keys: Vec<f64> = (vals.iter().filter_map(num))
            .map(|x| self.round(x))
            .filter(|&k| (self.hard_bounds).is_none_or(|b| b.contains(unshifted(k), self.date)))
            .collect();
        for r in vals.iter().filter_map(Value::as_array) {
            self.range_keys(num(&r[0]), num(&r[1]), &mut keys);
        }
        keys.sort_by(f64::total_cmp);
        keys.dedup();
        keys.into_iter().map(Value::from).collect()
    }

    /// The keys of every bucket a range field's `[from, to]` overlaps, clamped to the hard bounds
    /// (whose max is inclusive here), like RangeHistogramAggregator and
    /// DateRangeHistogramAggregator; at most one past [`MAX_BUCKETS`], so the render fails.
    // ponytail: a range whose keys stop advancing (f64 precision) ends there; ES loops forever
    fn range_keys(&self, from: Option<f64>, to: Option<f64>, keys: &mut Vec<f64>) {
        let (Some(mut from), Some(mut to)) = (from, to) else {
            return;
        };
        if let Some(b) = self.hard_bounds {
            from = b.min.map_or(from, |m| from.max(m));
            to = b.max.map_or(to, |m| to.min(m));
        }
        if self.date {
            // ponytail: open date ends stop at ±2^53 millis so rounding cannot overflow
            const LIMIT: f64 = (1u64 << 53) as f64;
            (from, to) = (from.max(-LIMIT), to.min(LIMIT));
        }
        let (mut k, end, start) = (self.round(from), self.round(to), keys.len());
        while k <= end && keys.len() - start <= MAX_BUCKETS {
            keys.push(k);
            match self.next(k) {
                next if next > k => k = next,
                _ => break,
            }
        }
    }

    /// The key of the bucket holding `key`.
    fn round(&self, key: f64) -> f64 {
        self.round_from(key - self.offset, false) + self.offset
    }

    /// The bucket key after `key`.
    fn next(&self, key: f64) -> f64 {
        match (self.calendar, &self.time_zone) {
            (None, None) => self.round(key + self.interval + self.interval / 2.0),
            _ => self.round_from(key - self.offset, true) + self.offset,
        }
    }

    /// The start of the interval holding `x` without the offset, or of the `next` one.
    fn round_from(&self, x: f64, next: bool) -> f64 {
        match (self.calendar, &self.time_zone) {
            (Some(unit), None) => unit.round(x as i64, next) as f64,
            (Some(unit), Some(tz)) => unit.round_in(tz, x as i64, next) as f64,
            (None, Some(tz)) => {
                let f = if next { next_interval } else { round_interval };
                f(tz, x as i64, self.interval as i64) as f64
            }
            (None, None) => {
                ((x / self.interval).floor() + f64::from(u8::from(next))) * self.interval
            }
        }
    }

    /// The reduced result; `sub` renders a bucket's sub-aggregations into it.
    pub(super) fn render(
        &self,
        b: &mut Buckets,
        body: &mut Map<String, Value>,
        buckets: &Cell<usize>,
        date: Option<&str>,
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
            let (mut min, mut max) = (self.extended_bounds.min, self.extended_bounds.max);
            if self.date {
                // LongBounds.round: dates round without the offset, then shift by it
                let round = |x: f64| self.round_from(x, false) + self.offset;
                (min, max) = (min.map(round), max.map(round));
            }
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
                _ => {
                    if let Some(min) = min {
                        fill(self.round(min), &|k| k <= max)?;
                    }
                }
            }
            v.extend(keys.into_iter().map(|k| (k, 0, b.empty.clone())));
            v.sort_by(|a, b| a.0.total_cmp(&b.0));
        }
        let rendered = v.into_iter().map(|(key, count, mut subs)| {
            let mut out = Map::new();
            let key_as_string = match (b.format, &self.format) {
                (Format::Date, _) => {
                    let (t, tz) = (key as i64, self.time_zone.as_ref());
                    let pattern = self.format.as_deref().or(date);
                    let utc = &es_index::tz::TimeZone::UTC;
                    Some(
                        match pattern.and_then(|p| es_index::format_date(p, t, tz.unwrap_or(utc))) {
                            Some(s) => s,
                            None => tz.map_or_else(|| iso_date(t), |tz| tz.format(t)),
                        },
                    )
                }
                (_, Some(p)) => Some(super::decimal_format(p, key)),
                _ => None,
            };
            if let Some(s) = &key_as_string {
                out.insert("key_as_string".into(), s.as_str().into());
            }
            match self.date {
                true => out.insert("key".into(), (key as i64).into()),
                false => out.insert("key".into(), key.into()),
            };
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

/// Millis of an ES time value.
fn time_millis(s: &str) -> Option<f64> {
    es_index::parse_time(s).map(|d| d.as_millis() as f64)
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
            fields: Default::default(),
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
    fn date_formats_date_range_fields_read_values_in_their_format() {
        let mut ix = Index::default();
        let format = Some("yyyy/MM/dd".into());
        (ix.mappings.properties).insert("d".into(), FieldMapping::DateRange { format });
        ix.put("1", json!({"d": {"gte": "2016/01/01", "lt": "2016/02/01"}}));
        let agg = json!({"date_histogram": {"field": "d", "calendar_interval": "month"}});
        let got = run(&[&ix], json!({"h": agg})).unwrap();
        let b = &got["h"]["buckets"];
        assert_eq!(
            (&b[0]["key"], &b[0]["doc_count"]),
            (&json!(1_451_606_400_000_i64), &json!(1))
        );
        assert_eq!(b.as_array().unwrap().len(), 1, "{b}");
    }

    #[test]
    fn date_formats_date_bounds_read_epoch_millis_strings() {
        // the default format's epoch_millis, as for an unmapped `value_type: date`
        let day = json!({"field": "at", "calendar_interval": "day",
            "extended_bounds": {"min": "1451606400000", "max": "2016-01-02"}});
        assert_eq!(
            date_histo(&["2016-01-02"], day).unwrap(),
            json!([
                ["2016-01-01T00:00:00.000Z", 0],
                ["2016-01-02T00:00:00.000Z", 1]
            ])
        );
        let mut ix = Index::default();
        ix.put("1", json!({}));
        let terms = json!({"t": {"terms": {"field": "u", "value_type": "date",
            "missing": "1451606400000"}}});
        let got = run(&[&ix], terms).unwrap();
        assert_eq!(got["t"]["buckets"][0]["key"], 1_451_606_400_000_i64);
    }

    #[test]
    fn histogram_format_renders_key_as_string() {
        let got = run(
            &[&index(&[json!(1), json!(51)])],
            json!({"h": {"histogram": {"field": "n", "interval": 50, "format": "Value is ##0.0"}}}),
        );
        assert_eq!(
            got.unwrap()["h"]["buckets"],
            json!([
                {"key_as_string": "Value is 0.0", "key": 0.0, "doc_count": 1},
                {"key_as_string": "Value is 50.0", "key": 50.0, "doc_count": 1},
            ])
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
    fn histograms_count_ranges_in_every_bucket_they_overlap() {
        let mut ix = Index::default();
        let props = &mut ix.mappings.properties;
        props.insert("l".into(), FieldMapping::LongRange);
        props.insert("d".into(), FieldMapping::DateRange { format: None });
        let docs = [
            json!({"l": {"lte": 2}, "d": {"gte": "2016-01-01", "lt": "2016-01-02"}}),
            json!({"l": {"gt": 3, "lt": 6}, "d": {"gte": "2016-01-02", "lt": "2016-01-03"}}),
            json!({"l": [{"gte": 4, "lte": 4}, {"gte": 5, "lte": 9}],
                "d": {"gte": "2016-02-01", "lt": "2016-03-01"}}),
        ];
        for (i, d) in docs.into_iter().enumerate() {
            ix.put(&i.to_string(), d);
        }
        let buckets = |ix: &Index, agg: Value| -> Result<Value, String> {
            let got = run(&[ix], json!({"h": agg}))?;
            let buckets = got["h"]["buckets"].as_array().unwrap().iter();
            Ok(buckets
                .map(|b| {
                    json!([
                        b["key_as_string"]
                            .as_str()
                            .map_or(b["key"].clone(), Value::from),
                        b["doc_count"]
                    ])
                })
                .collect())
        };
        // hard bounds clamp open ends, and their max is inclusive; a document counts once
        let hard = json!({"histogram": {"field": "l", "interval": 2,
            "hard_bounds": {"min": 0, "max": 8}}});
        assert_eq!(
            buckets(&ix, hard).unwrap(),
            json!([[0.0, 1], [2.0, 1], [4.0, 2], [6.0, 1], [8.0, 1]])
        );
        // `lt` is exclusive: the first range ends before 2016-01-02
        let month = json!({"date_histogram": {"field": "d", "calendar_interval": "month",
            "offset": "+1d"}});
        assert_eq!(
            buckets(&ix, month).unwrap(),
            json!([
                ["2015-12-02T00:00:00.000Z", 1],
                ["2016-01-02T00:00:00.000Z", 2],
                ["2016-02-02T00:00:00.000Z", 1]
            ])
        );
        // an open range without hard bounds is too many buckets
        let open = json!({"histogram": {"field": "l", "interval": 1}});
        assert!(
            buckets(&ix, open)
                .unwrap_err()
                .starts_with("Trying to create too many buckets")
        );
        let open = json!({"date_histogram": {"field": "d", "calendar_interval": "day"}});
        ix.put("3", json!({"d": {"lt": "2016-01-01"}}));
        assert!(
            buckets(&ix, open)
                .unwrap_err()
                .starts_with("Trying to create too many buckets")
        );
    }

    /// The `h` date_histogram's buckets over `dates` as `[key_as_string, doc_count]` pairs.
    fn date_histo(dates: &[&str], params: Value) -> Result<Value, String> {
        let mut ix = index(&[]);
        for (i, d) in dates.iter().enumerate() {
            ix.put(&i.to_string(), json!({"at": d}));
        }
        let got = run(&[&ix], json!({"h": {"date_histogram": params}}))?;
        let buckets = got["h"]["buckets"].as_array().unwrap().iter();
        Ok(buckets
            .map(|b| json!([b["key_as_string"], b["doc_count"]]))
            .collect())
    }

    #[test]
    fn date_histogram_rounds_to_calendar_units() {
        let dates = ["2016-01-01", "2016-01-02", "2016-02-01", "2016-03-01"];
        let mut ix = index(&[]);
        for (i, d) in dates.iter().enumerate() {
            ix.put(&i.to_string(), json!({"at": d}));
        }
        let got = run(
            &[&ix],
            json!({"h": {"date_histogram": {"field": "at", "calendar_interval": "month"}}}),
        );
        assert_eq!(
            got.unwrap()["h"]["buckets"],
            json!([
                {"key_as_string": "2016-01-01T00:00:00.000Z", "key": 1_451_606_400_000_i64, "doc_count": 2},
                {"key_as_string": "2016-02-01T00:00:00.000Z", "key": 1_454_284_800_000_i64, "doc_count": 1},
                {"key_as_string": "2016-03-01T00:00:00.000Z", "key": 1_456_790_400_000_i64, "doc_count": 1},
            ])
        );
        let first = |interval: &str, date: &str| {
            let got = date_histo(
                &[date],
                json!({"field": "at", "calendar_interval": interval}),
            );
            got.unwrap()[0][0].as_str().unwrap().to_owned()
        };
        // 2016-01-01 is a Friday; weeks start on Monday
        assert_eq!(
            first("1w", "2016-01-01T05:00:00Z"),
            "2015-12-28T00:00:00.000Z"
        );
        assert_eq!(first("quarter", "2016-05-15"), "2016-04-01T00:00:00.000Z");
        assert_eq!(first("1y", "2016-05-15"), "2016-01-01T00:00:00.000Z");
        assert_eq!(
            first("day", "2016-05-15T13:14:15Z"),
            "2016-05-15T00:00:00.000Z"
        );
        assert_eq!(
            first("1h", "2016-05-15T13:14:15Z"),
            "2016-05-15T13:00:00.000Z"
        );
        assert_eq!(
            first("minute", "2016-05-15T13:14:15Z"),
            "2016-05-15T13:14:00.000Z"
        );
        assert_eq!(
            first("1s", "2016-05-15T13:14:15.5Z"),
            "2016-05-15T13:14:15.000Z"
        );
        assert_eq!(first("month", "1969-12-15"), "1969-12-01T00:00:00.000Z");
        // empty buckets step a calendar unit at a time
        assert_eq!(
            date_histo(
                &["2016-01-31", "2016-04-01"],
                json!({"field": "at", "calendar_interval": "month"})
            )
            .unwrap(),
            json!([
                ["2016-01-01T00:00:00.000Z", 1],
                ["2016-02-01T00:00:00.000Z", 0],
                ["2016-03-01T00:00:00.000Z", 0],
                ["2016-04-01T00:00:00.000Z", 1]
            ])
        );
    }

    #[test]
    fn date_histogram_offsets_and_bounds() {
        let dates = ["2016-01-01", "2016-01-02", "2016-02-01", "2016-03-01"];
        let month = json!({"field": "at", "calendar_interval": "month", "offset": "+1d"});
        assert_eq!(
            date_histo(&dates, month).unwrap(),
            json!([
                ["2015-12-02T00:00:00.000Z", 1],
                ["2016-01-02T00:00:00.000Z", 2],
                ["2016-02-02T00:00:00.000Z", 1]
            ])
        );
        // extended bounds round without the offset, then shift by it
        let day = json!({"field": "at", "calendar_interval": "day", "offset": "1h",
            "extended_bounds": {"min": "2016-01-01", "max": "2016-01-04"}});
        assert_eq!(
            date_histo(&["2016-01-02", "2016-01-03"], day).unwrap(),
            json!([
                ["2016-01-01T01:00:00.000Z", 1],
                ["2016-01-02T01:00:00.000Z", 1],
                ["2016-01-03T01:00:00.000Z", 0],
                ["2016-01-04T01:00:00.000Z", 0]
            ])
        );
        let fixed = json!({"field": "at", "fixed_interval": "12h", "offset": -3_600_000,
            "min_doc_count": 1});
        assert_eq!(
            date_histo(&["2016-01-01T10:00:00Z", "2016-01-02T12:00:00Z"], fixed).unwrap(),
            json!([
                ["2015-12-31T23:00:00.000Z", 1],
                ["2016-01-02T11:00:00.000Z", 1]
            ])
        );
        // hard bounds keep rounded keys in [min, max)
        let hard = json!({"field": "at", "calendar_interval": "month",
            "hard_bounds": {"min": "2016-02-01", "max": "2016-03-01"}});
        assert_eq!(
            date_histo(&dates, hard).unwrap(),
            json!([["2016-02-01T00:00:00.000Z", 1]])
        );
    }

    #[test]
    fn date_histogram_rounds_in_time_zones() {
        // expectations from tests/yaml/aggregations/{date_,}histogram.yml
        let phoenix =
            json!({"field": "at", "fixed_interval": "1ms", "time_zone": "America/Phoenix"});
        assert_eq!(
            date_histo(&["2016-01-01"], phoenix).unwrap(),
            json!([["2015-12-31T17:00:00.000-07:00", 1]])
        );
        let berlin = json!({"field": "at", "fixed_interval": "30s", "time_zone": "Europe/Berlin"});
        assert_eq!(
            date_histo(&["2015-01-01T12:10:30Z"], berlin).unwrap(),
            json!([["2015-01-01T13:10:30.000+01:00", 1]])
        );
        let spring: Vec<_> = (3..9).map(|h| format!("2020-03-09T0{h}:00:00Z")).collect();
        let spring: Vec<_> = spring.iter().map(String::as_str).collect();
        let daily =
            json!({"field": "at", "calendar_interval": "1d", "time_zone": "America/New_York"});
        assert_eq!(
            date_histo(&spring, daily).unwrap(),
            json!([
                ["2020-03-08T00:00:00.000-05:00", 1],
                ["2020-03-09T00:00:00.000-04:00", 5]
            ])
        );
        let offset = json!({"field": "at", "calendar_interval": "1d",
            "time_zone": "America/New_York", "offset": "+1h"});
        assert_eq!(
            date_histo(&spring, offset).unwrap(),
            json!([
                ["2020-03-08T01:00:00.000-05:00", 2],
                ["2020-03-09T01:00:00.000-04:00", 4]
            ])
        );
        // the repeated hour of a fall-back transition is two buckets
        let hourly = json!({"field": "at", "calendar_interval": "hour",
            "time_zone": "America/New_York", "min_doc_count": 1});
        assert_eq!(
            date_histo(&["2020-11-01T05:30:00Z", "2020-11-01T06:30:00Z"], hourly).unwrap(),
            json!([
                ["2020-11-01T01:00:00.000-04:00", 1],
                ["2020-11-01T01:00:00.000-05:00", 1]
            ])
        );
        let fixed = json!({"field": "at", "fixed_interval": "1h", "time_zone": "+05:30"});
        assert_eq!(
            date_histo(&["2020-01-01T00:00:00Z"], fixed).unwrap(),
            json!([["2020-01-01T05:00:00.000+05:30", 1]])
        );
        // a date pattern parses mapped values and formats keys in the time zone
        let mut ix = Index::default();
        let p = "yyyy-MM-dd HH:mm:ss";
        let at = FieldMapping::Date {
            format: Some(p.into()),
            locale: None,
            fields: Default::default(),
        };
        ix.mappings.properties.insert("at".into(), at);
        ix.put("1", json!({"at": "2021-05-01 20:30:00"}));
        let jakarta = json!({"field": "at", "calendar_interval": "hour",
            "time_zone": "Asia/Jakarta", "format": p});
        let got = run(&[&ix], json!({"h": {"date_histogram": jakarta}})).unwrap();
        assert_eq!(
            got["h"]["buckets"],
            json!([{"key_as_string": "2021-05-02 03:00:00", "key": 1_619_899_200_000_i64,
                "doc_count": 1}])
        );
        // no documents: no buckets (and no rounding of an open end)
        let none = json!({"field": "at", "calendar_interval": "hour", "time_zone": "Asia/Jakarta"});
        assert_eq!(date_histo(&[], none).unwrap(), json!([]));
        let bad = json!({"field": "at", "fixed_interval": "1h", "time_zone": "../../etc/passwd"});
        assert_eq!(
            date_histo(&[], bad).unwrap_err(),
            "[date_histogram] failed to parse field [time_zone]"
        );
    }

    #[test]
    fn date_histogram_keyed_typed_and_bad_requests() {
        let mut ix = index(&[]);
        ix.put("1", json!({"at": "2016-01-01"}));
        let req = json!({"size": 0, "aggs": {"h": {"date_histogram": {"field": "at",
            "calendar_interval": "year", "keyed": true}}}});
        let req = SearchRequest::parse(&req).unwrap();
        let res = search_many(&[("i", &ix, None)], &req).unwrap();
        assert_eq!(
            res.aggregations.unwrap()["h"]["buckets"],
            json!({"2016-01-01T00:00:00.000Z": {"key_as_string": "2016-01-01T00:00:00.000Z",
                "key": 1_451_606_400_000_i64, "doc_count": 1}})
        );
        let err = |params: Value| date_histo(&[], params).unwrap_err();
        assert_eq!(
            err(json!({"field": "at", "calendar_interval": "2d"})),
            "The supplied interval [2d] could not be parsed as a calendar interval."
        );
        assert_eq!(
            err(json!({"field": "at", "calendar_interval": "1d", "fixed_interval": "1d"})),
            "Cannot use [fixed_interval] with [calendar_interval] configuration option."
        );
        assert_eq!(
            err(json!({"field": "at"})),
            "Invalid interval specified, must be non-null and non-empty"
        );
        assert_eq!(
            err(json!({"field": "at", "fixed_interval": "1M"})),
            "failed to parse setting [date_histogram.fixedInterval] with value [1M] as a time value: unit is missing or unrecognized"
        );
        assert_eq!(
            err(json!({"field": "at", "interval": "1d"})),
            "[date_histogram] unknown field [interval]"
        );
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
