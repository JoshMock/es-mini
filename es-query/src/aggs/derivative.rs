//! The `derivative` pipeline aggregation: adds to each bucket of its parent multi-bucket
//! aggregation the change of a bucket path's value since the previous bucket, like ES's
//! DerivativePipelineAggregator.

use super::bucket_sort::bucket_value;
use crate::QueryError;
use serde_json::{Map, Value};

#[derive(Debug, Clone, PartialEq)]
pub(super) struct Derivative {
    name: String,
    path: String,
}

impl Derivative {
    pub(super) fn parse(name: &str, params: &Value) -> Result<Self, QueryError> {
        let params = params
            .as_object()
            .ok_or("Expected [START_OBJECT] under [derivative]")?;
        let mut path = None;
        for (k, v) in params {
            match k.as_str() {
                "buckets_path" => path = v.as_str(),
                // ponytail: always the default `skip` policy, no `unit`
                "gap_policy" => {}
                _ => return Err(format!("[derivative] unknown field [{k}]").into()),
            }
        }
        match path {
            Some(path) => Ok(Derivative {
                name: name.to_owned(),
                path: path.to_owned(),
            }),
            None => Err("Missing required parameter [buckets_path]".into()),
        }
    }

    /// Adds `{"value": change}` to each bucket but the first with a value. A missing value is
    /// skipped, as in ES's `skip` gap policy.
    pub(super) fn apply(&self, buckets: &mut [Value], typed_keys: bool) -> Result<(), QueryError> {
        let key = match typed_keys {
            true => format!("derivative#{}", self.name),
            false => self.name.clone(),
        };
        let mut last = None;
        for b in buckets {
            let Some(v) = bucket_value(b, &self.path)?.and_then(|v| v.as_f64()) else {
                continue;
            };
            if let (Some(last), Some(b)) = (last, b.as_object_mut()) {
                let value = Map::from_iter([("value".to_owned(), Value::from(v - last))]);
                b.insert(key.clone(), value.into());
            }
            last = Some(v);
        }
        Ok(())
    }
}
