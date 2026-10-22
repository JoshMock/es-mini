//! `_update_by_query` and `_delete_by_query`: one search over the targets, then a write per hit.

use crate::{AppState, error::AppError, indices::parse_body, params, search::query_from_params};
use axum::{
    Json,
    body::Bytes,
    extract::{Path, Query, State},
    http::Uri,
};
use es_query::SearchRequest;
use serde_json::{Value, json};
use std::{collections::HashMap, time::Instant};

const PARAMS: [&str; 26] = [
    "allow_no_indices",
    "analyze_wildcard",
    "analyzer",
    "conflicts",
    "default_operator",
    "df",
    "expand_wildcards",
    "ignore_unavailable",
    "lenient",
    "max_docs",
    "preference",
    "q",
    "refresh",
    "request_cache",
    "requests_per_second",
    "routing",
    "scroll",
    "scroll_size",
    "search_timeout",
    "search_type",
    "slices",
    "stats",
    "timeout",
    "version",
    "wait_for_active_shards",
    "wait_for_completion",
];

/// Elasticsearch's default `scroll_size`: hits are written in batches of this many.
const BATCH: usize = 1000;

pub async fn update_by_query(
    state: State<AppState>,
    name: Path<String>,
    uri: Uri,
    params: Query<HashMap<String, String>>,
    body: Bytes,
) -> Result<Json<Value>, AppError> {
    by_query(state, name, uri, params, body, false)
}

pub async fn delete_by_query(
    state: State<AppState>,
    name: Path<String>,
    uri: Uri,
    params: Query<HashMap<String, String>>,
    body: Bytes,
) -> Result<Json<Value>, AppError> {
    by_query(state, name, uri, params, body, true)
}

// ponytail: the search and the writes share one write lock, so no document can change between
// them and `version_conflicts` is always 0; `conflicts` is only validated
fn by_query(
    State(state): State<AppState>,
    Path(name): Path<String>,
    uri: Uri,
    Query(params): Query<HashMap<String, String>>,
    body: Bytes,
    delete: bool,
) -> Result<Json<Value>, AppError> {
    params::check(&uri, &params, &PARAMS)?;
    if params
        .get("wait_for_completion")
        .is_some_and(|v| v == "false")
    {
        return Err(AppError::IllegalArgument(
            "wait_for_completion=false is not supported: es-mini has no tasks API".into(),
        ));
    }
    let start = Instant::now();
    let mut body = parse_body(&body)?;
    let obj = body
        .as_object_mut()
        .ok_or_else(|| AppError::Parse("request body must be an object".into()))?;
    if obj.contains_key("script") {
        return Err(AppError::IllegalArgument(
            "scripts are not supported: es-mini cannot run [script]".into(),
        ));
    }
    let conflicts = obj.remove("conflicts");
    match params
        .get("conflicts")
        .map(String::as_str)
        .or(conflicts.as_ref().and_then(Value::as_str))
    {
        None | Some("abort" | "proceed") => {}
        Some(c) => {
            return Err(AppError::IllegalArgument(format!(
                "conflicts may only be \"proceed\" or \"abort\" but was [{c}]"
            )));
        }
    }
    let max_docs =
        match (params.get("max_docs"), obj.remove("max_docs")) {
            (Some(p), _) => Some(p.parse::<usize>().map_err(|_| {
                AppError::IllegalArgument(format!("failed to parse [max_docs] [{p}]"))
            })?),
            (None, Some(v)) => Some(v.as_u64().ok_or_else(|| {
                AppError::IllegalArgument(format!("[max_docs] must be a positive number, got {v}"))
            })? as usize),
            (None, None) => None,
        };
    let mut req = SearchRequest::parse(&body).map_err(AppError::from)?;
    if let Some(q) = params.get("q") {
        req.query = query_from_params(q, &params);
    }
    req.from = 0;
    req.size = max_docs.unwrap_or(usize::MAX);

    let mut indices = state.write().expect("lock poisoned");
    let resolved = crate::indices::data_targets(&indices, &name, &params)?;
    let targets = crate::search::targets(&indices, &resolved);
    let hits: Vec<(String, String, Value)> = es_query::search_many(&targets, &req)?
        .hits
        .into_iter()
        .map(|h| (h.index.to_owned(), h.id.to_owned(), h.doc.source.clone()))
        .collect();
    for (index, id, source) in &hits {
        let index = indices.get_mut(index).expect("searched index exists");
        match delete {
            true => index.delete(id),
            false => index.put(id, source.clone()),
        };
    }
    if params
        .get("refresh")
        .is_some_and(|v| v.is_empty() || v == "true")
    {
        for t in &resolved {
            indices
                .get_mut(&t.index)
                .expect("resolved index exists")
                .refresh();
        }
    }

    let total = hits.len();
    let mut resp = json!({
        "took": start.elapsed().as_millis() as u64,
        "timed_out": false,
        "total": total,
        "deleted": if delete { total } else { 0 },
        "batches": total.div_ceil(BATCH),
        "version_conflicts": 0,
        "noops": 0,
        "retries": {"bulk": 0, "search": 0},
        "throttled_millis": 0,
        "requests_per_second": -1.0,
        "throttled_until_millis": 0,
        "failures": []
    });
    if !delete {
        resp["updated"] = total.into();
    }
    Ok(Json(resp))
}
