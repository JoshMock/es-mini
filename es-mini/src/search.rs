use crate::{AppState, error::AppError, indices::parse_body};
use axum::{
    Json,
    body::Bytes,
    extract::{Path, State},
};
use es_query::{
    SearchRequest,
    esql::{self, EsqlError},
    parse_query,
};
use serde_json::{Value, json};
use std::time::Instant;

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
    let index = indices
        .get(&name)
        .ok_or_else(|| AppError::IndexNotFound(name.clone()))?;
    let r = es_query::search(index, &req);
    let hits: Vec<Value> = r
        .hits
        .iter()
        .map(|h| {
            let mut hit =
                json!({"_index": name, "_id": h.id, "_score": h.score, "_source": h.doc.source});
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
    body: Bytes,
) -> Result<Json<Value>, AppError> {
    let body = parse_body(&body)?;
    let query = body
        .get("query")
        .map_or(Ok(es_query::Query::MatchAll), parse_query)
        .map_err(AppError::ParsingException)?;
    let indices = state.read().expect("lock poisoned");
    let index = indices
        .get(&name)
        .ok_or_else(|| AppError::IndexNotFound(name.clone()))?;
    Ok(Json(
        json!({"count": es_query::count(index, &query), "_shards": shards()}),
    ))
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
