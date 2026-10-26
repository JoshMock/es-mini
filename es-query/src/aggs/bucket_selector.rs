//! The `bucket_selector` pipeline aggregation: drops the buckets of its parent multi-bucket
//! aggregation for which its script is false, like ES's BucketSelectorPipelineAggregator.

use super::bucket_sort::bucket_value;
use crate::QueryError;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub(super) struct BucketSelector {
    /// Script variable and the bucket path that gives its value.
    paths: Vec<(String, String)>,
    /// The script `params.<var> <op> <number>`, the only form supported.
    // ponytail: no Painless; add an expression evaluator when a script needs more
    var: String,
    op: String,
    rhs: f64,
}

impl BucketSelector {
    pub(super) fn parse(params: &Value) -> Result<Self, QueryError> {
        let params = params
            .as_object()
            .ok_or("Expected [START_OBJECT] under [bucket_selector]")?;
        let bad = |k: &str| {
            QueryError::x_content(format!("[bucket_selector] failed to parse field [{k}]"))
        };
        let (mut paths, mut source) = (vec![], None);
        for (k, v) in params {
            match k.as_str() {
                "buckets_path" => {
                    for (var, path) in v.as_object().ok_or_else(|| bad(k))? {
                        paths.push((var.clone(), path.as_str().ok_or_else(|| bad(k))?.to_owned()));
                    }
                }
                "script" => {
                    source = v.as_str().or_else(|| v.get("source")?.as_str());
                    source.ok_or_else(|| bad(k))?;
                }
                // ponytail: always the default `skip` policy
                "gap_policy" => {}
                _ => return Err(format!("[bucket_selector] unknown field [{k}]").into()),
            }
        }
        let source = source.ok_or("Missing required parameter [script]")?;
        let unsupported = || QueryError::illegal(format!("script not supported: [{source}]"));
        let parts: Vec<&str> = source.split_whitespace().collect();
        let [lhs, op, rhs] = parts[..] else {
            return Err(unsupported());
        };
        let var = lhs.strip_prefix("params.").ok_or_else(unsupported)?;
        let rhs = rhs.parse().map_err(|_| unsupported())?;
        if !["<", "<=", ">", ">=", "==", "!="].contains(&op) || !paths.iter().any(|(v, _)| v == var)
        {
            return Err(unsupported());
        }
        Ok(BucketSelector {
            paths,
            var: var.to_owned(),
            op: op.to_owned(),
            rhs,
        })
    }

    /// Keeps the `buckets` for which the script holds. A missing value is NaN, as in ES's
    /// `skip` gap policy.
    pub(super) fn apply(&self, buckets: &mut Vec<Value>) -> Result<(), QueryError> {
        let path = (self.paths.iter().find(|(v, _)| *v == self.var)).map(|(_, p)| p);
        let path = path.ok_or("unknown script variable")?;
        let mut kept = vec![];
        for b in buckets.drain(..) {
            let v = bucket_value(&b, path)?
                .and_then(|v| v.as_f64())
                .unwrap_or(f64::NAN);
            let holds = match self.op.as_str() {
                "<" => v < self.rhs,
                "<=" => v <= self.rhs,
                ">" => v > self.rhs,
                ">=" => v >= self.rhs,
                "==" => v == self.rhs,
                _ => v != self.rhs,
            };
            if holds {
                kept.push(b);
            }
        }
        *buckets = kept;
        Ok(())
    }
}
