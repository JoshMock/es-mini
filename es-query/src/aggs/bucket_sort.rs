//! The `bucket_sort` pipeline aggregation: sorts and pages the buckets of its parent
//! multi-bucket aggregation, like ES's BucketSortPipelineAggregator.

use crate::{ErrorKind, QueryError, int};
use serde_json::Value;
use std::cmp::Ordering;

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

    /// Sorts (stably) and then pages `buckets`, like ES's `skip` gap policy: a bucket missing
    /// every sort value is dropped, and a missing value sorts last.
    pub(super) fn apply(&self, buckets: &mut Vec<Value>) -> Result<(), QueryError> {
        if !self.sort.is_empty() {
            let mut keyed = vec![];
            for b in buckets.drain(..) {
                let values = (self.sort.iter())
                    .map(|(path, _)| bucket_value(&b, path))
                    .collect::<Result<Vec<_>, _>>()?;
                if values.iter().any(Option::is_some) {
                    keyed.push((values, b));
                }
            }
            keyed.sort_by(|(x, _), (y, _)| {
                let by = x.iter().zip(y).zip(&self.sort);
                (by.map(|((a, b), (_, asc))| match (a, b) {
                    (Some(a), Some(b)) if *asc => compare(a, b),
                    (Some(a), Some(b)) => compare(b, a),
                    (a, b) => a.is_none().cmp(&b.is_none()),
                }))
                .find(|o| o.is_ne())
                .unwrap_or(Ordering::Equal)
            });
            buckets.extend(keyed.into_iter().map(|(_, b)| b));
        }
        buckets.drain(..self.from.min(buckets.len()));
        buckets.truncate(self.size.unwrap_or(usize::MAX));
        Ok(())
    }
}

/// Orders two sort values: numbers, or `_key` strings.
fn compare(a: &Value, b: &Value) -> Ordering {
    match (a, b) {
        (Value::String(a), Value::String(b)) => a.cmp(b),
        _ => (a.as_f64().unwrap_or(f64::NAN)).total_cmp(&b.as_f64().unwrap_or(f64::NAN)),
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

/// The value at a bucket path (`_count`, `_key`, or `agg>agg.metric`) of a rendered bucket:
/// `None` when missing, a number, or a `_key` string.
pub(super) fn bucket_value(bucket: &Value, path: &str) -> Result<Option<Value>, QueryError> {
    let no_agg = || QueryError {
        kind: ErrorKind::Validation,
        msg: format!("No aggregation found for path [{path}]"),
    };
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
        m if m.ends_with(']') && m.contains('[') => {
            let (name, prop) = m[..m.len() - 1].split_once('[').unwrap_or_default();
            let mut at = bucket;
            for name in names.into_iter().flat_map(|n| n.split('>')) {
                at = at.get(name).ok_or_else(no_agg)?;
            }
            let hits = at.get(name).ok_or_else(no_agg)?["hits"]["hits"].as_array();
            return match top_hit_property(name, prop, hits.ok_or_else(no_agg)?)? {
                Value::Null => Ok(None),
                v => v.as_f64().map(|_| Some(v.clone())).ok_or_else(not_number),
            };
        }
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
        Value::Null => Ok(None),
        k @ Value::String(_) if last == "_key" => Ok(Some(k.clone())),
        v => v.as_f64().map(|_| Some(v.clone())).ok_or_else(not_number),
    }
}

/// The `_sort`, `_score` or `_source.<path>` of the single hit of the top_hits aggregation
/// `name`, like ES's InternalTopHits.getProperty.
fn top_hit_property<'a>(
    name: &str,
    prop: &str,
    hits: &'a [Value],
) -> Result<&'a Value, QueryError> {
    let [hit] = hits else {
        return Err(QueryError::illegal(format!(
            "property paths for top_hits [{name}] require configuring it with size to 1"
        )));
    };
    let unsupported = || QueryError::illegal(format!("path not supported for [{name}]: [{prop}]"));
    match prop {
        "_sort" => hit["sort"].get(0).ok_or_else(unsupported),
        "_score" => Ok(&hit["_score"]),
        _ => {
            let path = prop.strip_prefix("_source.").ok_or_else(unsupported)?;
            let mut at = hit.get("_source").ok_or_else(unsupported)?;
            for key in path.split('.') {
                at = at.get(key).ok_or_else(unsupported)?;
            }
            Ok(at)
        }
    }
}
