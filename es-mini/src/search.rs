use crate::{AppState, error::AppError, indices::parse_body, params};
use axum::{
    Json,
    body::Bytes,
    extract::{Path, Query, State},
    http::Uri,
};
use es_index::{Indices, aliases};
use es_query::{
    SearchRequest,
    esql::{self, EsqlError},
    parse_query,
};
use serde_json::{Value, json};
use std::{collections::HashMap, time::Instant};

fn targets<'a>(indices: &'a Indices, resolved: &'a [aliases::Target]) -> Vec<es_query::Target<'a>> {
    resolved
        .iter()
        .map(|t| (t.index.as_str(), &indices[&t.index], t.filter.as_ref()))
        .collect()
}

const SEARCH_PARAMS: [&str; 46] = [
    "_slice",
    "_source",
    "_source_exclude_vectors",
    "_source_excludes",
    "_source_includes",
    "allow_no_indices",
    "allow_partial_search_results",
    "analyze_wildcard",
    "analyzer",
    "batched_reduce_size",
    "ccs_minimize_roundtrips",
    "default_operator",
    "df",
    "docvalue_fields",
    "expand_wildcards",
    "explain",
    "force_synthetic_source",
    "from",
    "ignore_throttled",
    "ignore_unavailable",
    "include_named_queries_score",
    "lenient",
    "max_concurrent_shard_requests",
    "pre_filter_shard_size",
    "preference",
    "q",
    "request_cache",
    "rest_total_hits_as_int",
    "routing",
    "scroll",
    "search_type",
    "seq_no_primary_term",
    "size",
    "sort",
    "stats",
    "stored_fields",
    "suggest_field",
    "suggest_mode",
    "suggest_size",
    "suggest_text",
    "terminate_after",
    "timeout",
    "track_scores",
    "track_total_hits",
    "typed_keys",
    "version",
];

const COUNT_PARAMS: [&str; 15] = [
    "allow_no_indices",
    "analyze_wildcard",
    "analyzer",
    "default_operator",
    "df",
    "expand_wildcards",
    "ignore_throttled",
    "ignore_unavailable",
    "lenient",
    "min_score",
    "preference",
    "q",
    "routing",
    "stats",
    "terminate_after",
];

// ponytail: every index has exactly one shard, so total is the number of targets
pub(crate) fn shards(n: usize) -> Value {
    json!({"total": n, "successful": n, "skipped": 0, "failed": 0})
}

pub async fn search_all(
    state: State<AppState>,
    uri: Uri,
    params: Query<HashMap<String, String>>,
    body: Bytes,
) -> Result<Json<Value>, AppError> {
    search(state, Path("_all".into()), uri, params, body).await
}

pub async fn search(
    State(state): State<AppState>,
    Path(name): Path<String>,
    uri: Uri,
    Query(params): Query<HashMap<String, String>>,
    body: Bytes,
) -> Result<Json<Value>, AppError> {
    params::check(&uri, &params, &SEARCH_PARAMS)?;
    let start = Instant::now();
    let mut req = SearchRequest::parse(&parse_body(&body)?).map_err(AppError::from)?;
    if let Some(q) = params.get("q") {
        req.query = es_query::parse_query_string(
            q,
            params.get("df").map_or("_all", String::as_str),
            params
                .get("default_operator")
                .is_some_and(|o| o.eq_ignore_ascii_case("and")),
        );
    }
    let indices = state.read().expect("lock poisoned");
    let resolved = crate::indices::data_targets(&indices, &name, &params)?;
    let targets = targets(&indices, &resolved);
    for (_, index, _) in &targets {
        let max = index
            .settings
            .get("index.max_result_window")
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(10_000);
        if req.from + req.size > max {
            return Err(AppError::IllegalArgument(format!(
                "Result window is too large, from + size must be less than or equal to: [{max}] but was [{}]. See the scroll api for a more efficient way to request large data sets. This limit can be set by changing the [index.max_result_window] index level setting.",
                req.from + req.size
            )));
        }
    }
    es_query::check_sortable(&targets, &req)?;
    let r = es_query::search_many(&targets, &req)?;
    let hits: Vec<Value> = r
        .hits
        .iter()
        .map(|h| {
            let mut hit =
                json!({"_index": h.index, "_id": h.id, "_score": h.score, "_source": h.doc.source});
            if !h.sort.is_empty() {
                hit["sort"] = h.sort.clone().into();
            }
            hit
        })
        .collect();
    let as_int = params
        .get("rest_total_hits_as_int")
        .is_some_and(|v| v == "true");
    let track = params
        .get("track_total_hits")
        .or(req.track_total_hits.as_ref())
        .map(String::as_str);
    let limit = match track {
        Some("true" | "-1") => None,
        None if as_int => None,
        None => Some(Some(10_000)),
        Some("false") => Some(None),
        Some(n) => match n.parse::<u64>() {
            Ok(n) if as_int => {
                return Err(AppError::IllegalArgument(format!(
                    "[rest_total_hits_as_int] cannot be used if the tracking of total hits is not accurate, got {n}"
                )));
            }
            Ok(n) => Some(Some(n)),
            Err(_) => {
                return Err(AppError::IllegalArgument(format!(
                    "[track_total_hits] parameter must be positive or equals to -1, got {n}"
                )));
            }
        },
    };
    let total = match limit {
        Some(None) if as_int => json!(-1),
        Some(None) => Value::Null,
        Some(Some(n)) if r.total as u64 > n => json!({"value": n, "relation": "gte"}),
        _ if as_int => json!(r.total),
        _ => json!({"value": r.total, "relation": "eq"}),
    };
    let mut hits = json!({"total": total, "max_score": r.max_score, "hits": hits});
    if total.is_null() {
        hits.as_object_mut().unwrap().remove("total");
    }
    let resp = json!({
        "took": start.elapsed().as_millis() as u64,
        "timed_out": false,
        "_shards": shards(targets.len()),
        "hits": hits
    });
    Ok(Json(resp))
}

pub async fn count_all(
    state: State<AppState>,
    uri: Uri,
    params: Query<HashMap<String, String>>,
    body: Bytes,
) -> Result<Json<Value>, AppError> {
    count(state, Path("_all".into()), uri, params, body).await
}

pub async fn count(
    State(state): State<AppState>,
    Path(name): Path<String>,
    uri: Uri,
    Query(params): Query<HashMap<String, String>>,
    body: Bytes,
) -> Result<Json<Value>, AppError> {
    params::check(&uri, &params, &COUNT_PARAMS)?;
    if params
        .get("terminate_after")
        .and_then(|v| v.parse::<i64>().ok())
        .is_some_and(|n| n < 0)
    {
        return Err(AppError::IllegalArgument(
            "terminateAfter must be > 0".into(),
        ));
    }
    let body = parse_body(&body)?;
    if let Some(k) = body
        .as_object()
        .and_then(|o| o.keys().find(|k| *k != "query"))
    {
        return Err(AppError::ParsingException(format!(
            "request does not support [{k}]"
        )));
    }
    let query = match params.get("q") {
        Some(q) => es_query::parse_query_string(
            q,
            params.get("df").map_or("_all", String::as_str),
            params
                .get("default_operator")
                .is_some_and(|o| o.eq_ignore_ascii_case("and")),
        ),
        None => body
            .get("query")
            .map_or(Ok(es_query::Query::MatchAll), parse_query)?,
    };
    let indices = state.read().expect("lock poisoned");
    let resolved = crate::indices::data_targets(&indices, &name, &params)?;
    let n = es_query::count_many(&targets(&indices, &resolved), &query)?;
    let resp = json!({"count": n, "_shards": shards(resolved.len())});
    Ok(Json(resp))
}

pub async fn esql(
    State(state): State<AppState>,
    uri: Uri,
    Query(params): Query<HashMap<String, String>>,
    body: Bytes,
) -> Result<Json<Value>, AppError> {
    params::check(
        &uri,
        &params,
        &["delimiter", "drop_null_columns", "allow_partial_results"],
    )?;
    let start = Instant::now();
    let body = parse_body(&body)?;
    let query = body["query"]
        .as_str()
        .ok_or_else(|| AppError::ParsingException("[query] is required".into()))?;
    let r = esql::run(&state.read().expect("lock poisoned"), query).map_err(|e| match e {
        EsqlError::Parse(m) => AppError::ParsingException(m),
        EsqlError::Verification(m) => AppError::Verification(m),
        EsqlError::IndexClosed(i) => AppError::IndexClosed(i),
    })?;
    Ok(Json(json!({
        "took": start.elapsed().as_millis() as u64,
        "is_partial": false,
        "columns": r.columns,
        "values": r.values,
        "documents_found": r.documents_found,
        "values_loaded": r.values_loaded,
    })))
}
