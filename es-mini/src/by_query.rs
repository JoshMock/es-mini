//! `_update_by_query` and `_delete_by_query`: one search over the targets, then a write per hit.

use crate::{AppState, error::AppError, indices::parse_body, params, search::query_from_params};
use axum::{
    Json,
    body::Bytes,
    extract::{Path, Query, State},
    http::{StatusCode, Uri},
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
) -> Result<(StatusCode, Json<Value>), AppError> {
    by_query(state, name, uri, params, body, false)
}

pub async fn delete_by_query(
    state: State<AppState>,
    name: Path<String>,
    uri: Uri,
    params: Query<HashMap<String, String>>,
    body: Bytes,
) -> Result<(StatusCode, Json<Value>), AppError> {
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
) -> Result<(StatusCode, Json<Value>), AppError> {
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
    let batch = match params.get("scroll_size") {
        Some(v) => v.parse().ok().filter(|&n| n > 0).ok_or_else(|| {
            AppError::IllegalArgument(format!(
                "Failed to parse int parameter [scroll_size] with value [{v}]"
            ))
        })?,
        None => BATCH,
    };
    let mut req = SearchRequest::parse(&body).map_err(AppError::from)?;
    if let Some(q) = params.get("q") {
        req.query = query_from_params(q, &params);
    }
    req.from = 0;
    req.size = max_docs.unwrap_or(usize::MAX);

    let mut indices = state.write();
    let resolved = crate::indices::data_targets(&indices, &name, &params)?;
    let split = crate::search::split(&indices, &resolved);
    let targets = crate::search::targets(&indices, &resolved, &split);
    let r = es_query::search_many(&targets, &req)?;
    // Like ES, a failed shard aborts before any write; the response has the highest status.
    let (mut status, mut failures) = (StatusCode::OK, vec![]);
    for (index, shard, e) in &r.failures {
        let (s, _, f) = crate::error::shard_failure(index, *shard, e.clone());
        status = status.max(s);
        let (node, code) = (crate::nodes::NODE_ID, s.as_u16());
        let reason = &f["reason"];
        failures.push(
            json!({"index": index, "shard": shard, "node": node, "status": code, "reason": reason}),
        );
    }
    let hits: Vec<(String, String, Option<String>, Value)> = match failures.is_empty() {
        true => (r.hits.into_iter())
            .map(|h| {
                let (index, id) = (h.index.to_owned(), h.id.to_owned());
                (index, id, h.doc.routing.clone(), h.doc.source.clone())
            })
            .collect(),
        false => vec![],
    };
    // Like ES, a document that fails to index ends the request after its batch.
    let (mut written, mut batches) = (0, 0);
    for chunk in hits.chunks(batch) {
        batches += 1;
        for (name, id, routing, source) in chunk {
            let index = indices.get_mut(name).expect("searched index exists");
            let routing = routing.as_deref();
            if delete {
                index.delete_routed(id, routing);
            } else if let Err(e) = index.map_dynamic(id, source) {
                let (s, error) = AppError::from(e).parts();
                status = status.max(s);
                let cause = json!({"type": error["type"], "reason": error["reason"]});
                let code = s.as_u16();
                failures.push(json!({"index": name, "id": id, "cause": cause, "status": code}));
                continue;
            } else {
                index.put_auto(id, routing, source.clone(), -1);
            }
            written += 1;
        }
        if !failures.is_empty() {
            break;
        }
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

    let mut resp = json!({
        "took": start.elapsed().as_millis() as u64,
        "timed_out": false,
        "total": hits.len(),
        "deleted": if delete { written } else { 0 },
        "batches": batches,
        "version_conflicts": 0,
        "noops": 0,
        "retries": {"bulk": 0, "search": 0},
        "throttled_millis": 0,
        "requests_per_second": -1.0,
        "throttled_until_millis": 0,
        "failures": failures
    });
    if !delete {
        resp["updated"] = written.into();
    }
    Ok((status, Json(resp)))
}
