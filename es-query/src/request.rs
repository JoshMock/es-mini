//! Search request parsing and the query error type.

use crate::{Query, parse_query};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct SortField {
    pub field: String,
    pub desc: bool,
    /// `unmapped_type` was given, so a missing mapping is not an error.
    pub unmapped_ok: bool,
    /// `missing`: `_first`, `_last` (the default) or a value to sort absent fields as.
    pub missing: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SearchRequest {
    pub query: Query,
    pub from: usize,
    pub size: usize,
    pub sort: Vec<SortField>,
    /// Body `track_total_hits` (`true`, `false` or a number) as the URL-parameter spelling.
    pub track_total_hits: Option<String>,
    /// The hit count past which the total need not be exact (`None`: count every hit), as
    /// `track_total_hits` sets it. Hits ordered by one score for all then stop being counted
    /// once the page is full and the count is past it.
    pub total_up_to: Option<usize>,
    pub aggs: Vec<crate::aggs::Agg>,
    /// Top-level `max_bucket` aggregations, computed from the rendered `aggs`.
    pub pipelines: Vec<crate::aggs::MaxBucket>,
    /// The `typed_keys` URL parameter: aggregation names are prefixed with their type.
    pub typed_keys: bool,
    /// Body `profile`: report where each shard's aggregation time went.
    pub profile: bool,
    /// Body `fields`: the mapped fields each hit fetches.
    pub fields: Vec<crate::aggs::FieldSpec>,
    /// Body `runtime_mappings`: names of the fields that emit the hit's `_source` as a string.
    pub runtime_source: Vec<String>,
}

/// Which Elasticsearch error type a query failure maps to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// `parsing_exception`
    Parsing,
    /// `illegal_argument_exception`
    IllegalArgument,
    /// `x_content_parse_exception`
    XContentParse,
    /// `too_many_buckets_exception`, past `search.max_buckets`
    TooManyBuckets,
    /// `parse_exception`, such as a date that does not parse
    Parse,
    /// `action_request_validation_exception`, such as a pipeline aggregation's bad bucket path
    Validation,
}

/// ES's `search.max_buckets`: a search fails rather than render more aggregation buckets.
pub const MAX_BUCKETS: usize = 65_536;

/// A query failure: its Elasticsearch error type and reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryError {
    pub kind: ErrorKind,
    pub msg: String,
    /// When the search failed on every index: each index's failure; the first gives `kind`
    /// and `msg`.
    pub shards: Vec<(String, QueryError)>,
}

impl QueryError {
    pub fn new(kind: ErrorKind, msg: impl Into<String>) -> Self {
        let (msg, shards) = (msg.into(), vec![]);
        Self { kind, msg, shards }
    }

    pub fn illegal(msg: impl Into<String>) -> Self {
        Self::new(ErrorKind::IllegalArgument, msg)
    }

    pub fn x_content(msg: impl Into<String>) -> Self {
        Self::new(ErrorKind::XContentParse, msg)
    }
}

/// A bare message is a `parsing_exception`.
impl From<String> for QueryError {
    fn from(msg: String) -> Self {
        Self::new(ErrorKind::Parsing, msg)
    }
}

impl From<&str> for QueryError {
    fn from(msg: &str) -> Self {
        msg.to_string().into()
    }
}

impl std::fmt::Display for QueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.msg)
    }
}

impl SearchRequest {
    pub fn parse(body: &Value) -> Result<Self, QueryError> {
        if let Some(k) = body.as_object().and_then(|o| {
            o.keys().find(|k| {
                ![
                    "query",
                    "from",
                    "size",
                    "sort",
                    "track_total_hits",
                    "aggs",
                    "aggregations",
                    "profile",
                    "fields",
                    "runtime_mappings",
                ]
                .contains(&k.as_str())
            })
        }) {
            return Err(format!("Unknown key for a START_OBJECT in [{k}].").into());
        }
        let query = body.get("query").map_or(Ok(Query::MatchAll), parse_query)?;
        let usize_of = |k: &str, d| match body.get(k) {
            None => Ok(d),
            Some(v) => match v.as_i64() {
                Some(n) if n < 0 => Err(QueryError::illegal(format!(
                    "[{k}] parameter cannot be negative, found [{n}]"
                ))),
                Some(n) => Ok(n as usize),
                None => Err(format!("[{k}] must be a number").into()),
            },
        };
        let sort = body.get("sort").map_or(Ok(vec![]), parse_sorts)?;
        let track_total_hits = match body.get("track_total_hits") {
            None => None,
            Some(v) if v.is_boolean() || v.is_i64() => Some(v.to_string()),
            Some(_) => return Err("[track_total_hits] must be a boolean or a number".into()),
        };
        let (mut aggs, pipelines) = match (body.get("aggs"), body.get("aggregations")) {
            (Some(_), Some(_)) => {
                return Err("Found two aggregation definitions: [aggs] and [aggregations]".into());
            }
            (Some(a), _) | (_, Some(a)) => crate::aggs::parse_top(a)?,
            (None, None) => Default::default(),
        };
        let fields = match body.get("fields") {
            None => vec![],
            Some(v) => crate::aggs::field_specs(v).ok_or("[fields] must be field names")?,
        };
        crate::aggs::inherit_fields(&mut aggs, &fields);
        let runtime_source = body
            .get("runtime_mappings")
            .map_or(Ok(vec![]), parse_runtime_mappings)?;
        Ok(Self {
            runtime_source,
            fields,
            aggs,
            pipelines,
            typed_keys: false,
            profile: body.get("profile").and_then(Value::as_bool) == Some(true),
            query,
            track_total_hits,
            total_up_to: None,
            from: usize_of("from", 0)?,
            size: usize_of("size", 10)?,
            sort,
        })
    }
}

/// The names of the runtime fields in `runtime_mappings`.
// ponytail: there is no Painless; only `emit(params['_source'].toString())` is understood
fn parse_runtime_mappings(v: &Value) -> Result<Vec<String>, QueryError> {
    const SOURCE: &str = "emit(params['_source'].toString())";
    let fields = v
        .as_object()
        .ok_or("[runtime_mappings] must be an object")?;
    (fields.iter())
        .map(|(name, def)| {
            let script = def.get("script");
            let source = script.and_then(|s| s.get("source").or(Some(s)));
            match (def.get("type").and_then(Value::as_str), source) {
                (Some("keyword"), Some(Value::String(s))) if s == SOURCE => Ok(name.clone()),
                _ => Err(QueryError::illegal(format!(
                    "unsupported runtime field [{name}]"
                ))),
            }
        })
        .collect()
}

/// A `sort`: one sort field or an array of them.
pub(crate) fn parse_sorts(v: &Value) -> Result<Vec<SortField>, QueryError> {
    match v {
        Value::Array(a) => a.iter().map(parse_sort).collect(),
        s => Ok(vec![parse_sort(s)?]),
    }
}

fn parse_sort(v: &Value) -> Result<SortField, QueryError> {
    let desc_of = |o: &Value| match o.as_str() {
        Some("desc") => Ok(true),
        Some("asc") => Ok(false),
        _ => Err(format!("invalid sort order [{o}]")),
    };
    match v {
        Value::String(f) => Ok(SortField {
            desc: f == "_score",
            field: f.clone(),
            unmapped_ok: false,
            missing: None,
        }),
        Value::Object(m) if m.len() == 1 => {
            let (field, p) = m.iter().next().expect("len checked");
            let order = match p {
                Value::Object(o) => o.get("order"),
                o => Some(o),
            };
            let desc = match order {
                Some(o) => desc_of(o)?,
                None => field == "_score",
            };
            Ok(SortField {
                field: field.clone(),
                desc,
                unmapped_ok: p.get("unmapped_type").is_some(),
                missing: p.get("missing").cloned(),
            })
        }
        _ => Err(format!("malformed sort [{v}]").into()),
    }
}
