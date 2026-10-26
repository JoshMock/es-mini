//! The `bucket_sort` pipeline aggregation: sorts and pages the buckets of its parent
//! multi-bucket aggregation, like ES's BucketSortPipelineAggregator.

use crate::{QueryError, int};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub(super) struct BucketSort {
    /// Bucket paths to sort by, each ascending or not.
    sort: Vec<(String, bool)>,
    from: usize,
    size: Option<usize>,
}

impl BucketSort {
    pub(super) fn parse(params: &Value) -> Result<Self, QueryError> {
        let params = params
            .as_object()
            .ok_or("Expected [START_OBJECT] under [bucket_sort]")?;
        let bad =
            |k: &str| QueryError::x_content(format!("[bucket_sort] failed to parse field [{k}]"));
        let (mut sort, mut from, mut size) = (vec![], 0, None);
        for (k, v) in params {
            match k.as_str() {
                "sort" => {
                    let entries = match v {
                        Value::Array(a) => a.iter().collect(),
                        v => vec![v],
                    };
                    for e in entries {
                        sort.extend(sort_entry(e).ok_or_else(|| bad(k))?);
                    }
                }
                "from" => match int(v).ok_or_else(|| bad(k))? {
                    n if n < 0 => {
                        return Err(QueryError::illegal(format!(
                            "[from] must be a positive integer: [{n}]"
                        )));
                    }
                    n => from = n as usize,
                },
                "size" => match int(v).ok_or_else(|| bad(k))? {
                    n if n <= 0 => {
                        return Err(QueryError::illegal(format!(
                            "[size] must be a positive integer: [{n}]"
                        )));
                    }
                    n => size = Some(n as usize),
                },
                // ponytail: always the default `skip` policy
                "gap_policy" => {}
                _ => return Err(format!("[bucket_sort] unknown field [{k}]").into()),
            }
        }
        Ok(BucketSort { sort, from, size })
    }

    /// Sorts (stably; a missing value is NaN, the greatest) and then pages `buckets`.
    pub(super) fn apply(&self, buckets: &mut Vec<Value>) -> Result<(), QueryError> {
        if !self.sort.is_empty() {
            let mut keyed = vec![];
            for b in buckets.drain(..) {
                let values = (self.sort.iter())
                    .map(|(path, _)| bucket_value(&b, path))
                    .collect::<Result<Vec<_>, _>>()?;
                keyed.push((values, b));
            }
            keyed.sort_by(|(x, _), (y, _)| {
                let by = x.iter().zip(y).zip(&self.sort);
                (by.map(|((a, b), (_, asc))| match asc {
                    true => a.total_cmp(b),
                    false => b.total_cmp(a),
                }))
                .find(|o| o.is_ne())
                .unwrap_or(std::cmp::Ordering::Equal)
            });
            buckets.extend(keyed.into_iter().map(|(_, b)| b));
        }
        buckets.drain(..self.from.min(buckets.len()));
        buckets.truncate(self.size.unwrap_or(usize::MAX));
        Ok(())
    }
}

/// The paths and directions of one `sort` entry: `"path"`, `{"path": "desc"}` or
/// `{"path": {"order": "desc"}}`.
fn sort_entry(v: &Value) -> Option<Vec<(String, bool)>> {
    let order = |v: &Value| match v.as_str()? {
        "asc" => Some(true),
        "desc" => Some(false),
        _ => None,
    };
    match v {
        Value::String(path) => Some(vec![(path.clone(), true)]),
        Value::Object(o) => o
            .iter()
            .map(|(path, v)| {
                let asc = match v {
                    Value::Object(o) => order(o.get("order")?)?,
                    v => order(v)?,
                };
                Some((path.clone(), asc))
            })
            .collect(),
        _ => None,
    }
}

/// The number at a bucket path (`_count`, `_key`, or `agg>agg.metric`) of a rendered bucket.
fn bucket_value(bucket: &Value, path: &str) -> Result<f64, QueryError> {
    let no_agg = || QueryError::illegal(format!("No aggregation found for path [{path}]"));
    let not_number = || {
        QueryError::illegal(format!(
            "buckets_path must reference either a number value or a single value numeric metric aggregation, got: [{path}]"
        ))
    };
    let (names, metric) = match path.rsplit_once('>') {
        Some((names, last)) => (Some(names), last),
        None => (None, path),
    };
    let (last, metric) = match metric {
        "_count" | "_key" => (metric, None),
        m => match m.split_once('.') {
            Some((a, k)) => (a, Some(k)),
            None => (m, None),
        },
    };
    let mut at = bucket;
    for name in names.into_iter().flat_map(|n| n.split('>')) {
        at = at.get(name).ok_or_else(no_agg)?;
    }
    let v = match last {
        "_count" => at.get("doc_count"),
        "_key" => at.get("key"),
        name => {
            let agg = at.get(name).ok_or_else(no_agg)?;
            Some(agg.get(metric.unwrap_or("value")).ok_or_else(no_agg)?)
        }
    };
    match v.ok_or_else(no_agg)? {
        Value::Null => Ok(f64::NAN),
        v => v.as_f64().ok_or_else(not_number),
    }
}
