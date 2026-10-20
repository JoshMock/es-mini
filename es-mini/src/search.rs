use crate::{AppState, error::AppError, indices::parse_body};
use axum::{
    Json,
    body::Bytes,
    extract::{Path, Query, State},
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

// ponytail: one shard per target index is reported as a single shard
fn shards() -> Value {
    json!({"total": 1, "successful": 1, "skipped": 0, "failed": 0})
}

pub async fn search(
    State(state): State<AppState>,
    Path(name): Path<String>,
    body: Bytes,
) -> Result<Json<Value>, AppError> {
    let start = Instant::now();
    let req = SearchRequest::parse(&parse_body(&body)?).map_err(AppError::ParsingException)?;
    let indices = state.read().expect("lock poisoned");
    let resolved = aliases::resolve_read(&indices, &name)?;
    let targets = targets(&indices, &resolved);
    let r = es_query::search_many(&targets, &req).map_err(AppError::ParsingException)?;
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
    Ok(Json(json!({
        "took": start.elapsed().as_millis() as u64,
        "timed_out": false,
        "_shards": shards(),
        "hits": {"total": {"value": r.total, "relation": "eq"}, "max_score": r.max_score, "hits": hits}
    })))
}

pub async fn count(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    body: Bytes,
) -> Result<Json<Value>, AppError> {
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
            .map_or(Ok(es_query::Query::MatchAll), parse_query)
            .map_err(AppError::ParsingException)?,
    };
    let indices = state.read().expect("lock poisoned");
    let resolved = aliases::resolve_read(&indices, &name)?;
    let n = es_query::count_many(&targets(&indices, &resolved), &query)
        .map_err(AppError::ParsingException)?;
    Ok(Json(json!({"count": n, "_shards": shards()})))
}

pub async fn esql(State(state): State<AppState>, body: Bytes) -> Result<Json<Value>, AppError> {
    let start = Instant::now();
    let body = parse_body(&body)?;
    let query = body["query"]
        .as_str()
        .ok_or_else(|| AppError::ParsingException("[query] is required".into()))?;
    let r = esql::run(&state.read().expect("lock poisoned"), query).map_err(|e| match e {
        EsqlError::Parse(m) => AppError::ParsingException(m),
        EsqlError::Verification(m) => AppError::Verification(m),
    })?;
    Ok(Json(json!({
        "took": start.elapsed().as_millis() as u64,
        "is_partial": false,
        "columns": r.columns,
        "values": r.values,
    })))
}
