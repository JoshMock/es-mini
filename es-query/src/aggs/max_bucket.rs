//! The `max_bucket` sibling pipeline aggregation: the largest value of a bucket path across the
//! buckets of a multi-bucket aggregation, like ES's MaxBucketPipelineAggregator.

use super::bucket_sort::{agg_at, bucket_value};
use super::java_double;
use crate::QueryError;
use serde_json::{Map, Value};

#[derive(Debug, Clone, PartialEq)]
pub struct MaxBucket {
    name: String,
    path: String,
}

impl MaxBucket {
    pub(super) fn parse(name: &str, params: &Value) -> Result<Self, QueryError> {
        let params = params
            .as_object()
            .ok_or("Expected [START_OBJECT] under [max_bucket]")?;
        let mut path = None;
        for (k, v) in params {
            match k.as_str() {
                "buckets_path" => path = v.as_str(),
                // ponytail: always the default `skip` policy
                "gap_policy" => {}
                _ => return Err(format!("[max_bucket] unknown field [{k}]").into()),
            }
        }
        match path {
            Some(path) => Ok(MaxBucket {
                name: name.to_owned(),
                path: path.to_owned(),
            }),
            None => Err("Missing required parameter [buckets_path]".into()),
        }
    }

    /// Adds this aggregation to the rendered `aggs`: its `value` and the `keys` of the buckets
    /// that have it.
    pub(super) fn apply(
        &self,
        aggs: &mut Map<String, Value>,
        typed_keys: bool,
    ) -> Result<(), QueryError> {
        let (agg, rest) = (self.path.split_once('>')).ok_or_else(|| {
            QueryError::illegal(format!("No aggregation found for path [{}]", self.path))
        })?;
        let buckets: Vec<&Value> = match agg_at(aggs, agg).map(|a| &a["buckets"]) {
            Some(Value::Array(b)) => b.iter().collect(),
            Some(Value::Object(b)) => b.values().collect(),
            _ => {
                return Err(QueryError::illegal(format!(
                    "No aggregation found for path [{}]",
                    self.path
                )));
            }
        };
        let (mut best, mut keys) = (f64::NEG_INFINITY, vec![]);
        for b in buckets {
            let Some(v) = bucket_value(b, rest)?.and_then(|v| v.as_f64()) else {
                continue;
            };
            if v > best {
                (best, keys) = (v, vec![]);
            }
            if v == best {
                keys.push(key_string(b));
            }
        }
        let value = best.is_finite().then_some(best);
        let body = Map::from_iter([
            ("value".to_owned(), value.map_or(Value::Null, Value::from)),
            ("keys".to_owned(), keys.into()),
        ]);
        let key = match typed_keys {
            true => format!("bucket_metric_value#{}", self.name),
            false => self.name.clone(),
        };
        aggs.insert(key, body.into());
        Ok(())
    }
}

/// A bucket's key as ES's `getKeyAsString` renders it.
fn key_string(bucket: &Value) -> String {
    match (&bucket["key_as_string"], &bucket["key"]) {
        (Value::String(s), _) | (_, Value::String(s)) => s.clone(),
        (_, k) => k
            .as_f64()
            .filter(|_| !k.is_i64() && !k.is_u64())
            .map_or_else(|| k.to_string(), java_double),
    }
}
