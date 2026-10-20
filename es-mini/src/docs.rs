use crate::{AppState, error::AppError};
use axum::{
    Json,
    body::Bytes,
    extract::{Path, Query, State},
    http::StatusCode,
};
use es_index::{DocResult, Index, WriteResult, aliases};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::atomic::{AtomicU32, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

type Reply = Result<(StatusCode, Json<Value>), AppError>;

pub fn parse_json(body: &[u8]) -> Result<Value, AppError> {
    serde_json::from_slice(body).map_err(|e| AppError::Parse(e.to_string()))
}

fn check_source(v: Value) -> Result<Value, AppError> {
    if !v.is_object() {
        return Err(AppError::MapperParsing(
            "failed to parse, document is not an object".into(),
        ));
    }
    Ok(v)
}

// ponytail: time+counter hex ids; ES uses base64 time-based UUIDs, same length
pub fn auto_id() -> String {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        "{:012x}{:08x}",
        nanos & 0xffff_ffff_ffff,
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// Runs `f` against the write target `name` resolves to (an index, created if missing as ES
/// auto-create does, or an alias's write index). Returns the result, shard total, and index name.
fn write<T>(
    state: &AppState,
    name: &str,
    f: impl FnOnce(&mut Index) -> T,
) -> Result<(T, u64, String), AppError> {
    let mut indices = state.write().expect("lock poisoned");
    let target = aliases::resolve_write(&indices, name)?;
    let index = indices.entry(target.clone()).or_default();
    let replicas = index
        .settings
        .get("index.number_of_replicas")
        .and_then(|r| r.parse().ok())
        .unwrap_or(0);
    Ok((f(index), 1 + replicas, target))
}

/// The one concrete index a single-document read targets.
fn single_index(indices: &es_index::Indices, name: &str) -> Result<String, AppError> {
    let targets = aliases::resolve_read(indices, name)?;
    match targets.as_slice() {
        [t] => Ok(t.index.clone()),
        [] => Err(AppError::IndexNotFound(name.to_owned())),
        many => Err(AppError::IllegalArgument(format!(
            "alias [{name}] has more than one index associated with it [{}], can't execute a single index op",
            many.iter()
                .map(|t| t.index.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

fn reply(index: &str, id: &str, w: WriteResult, shards_total: u64) -> (StatusCode, Json<Value>) {
    let status = match w.result {
        DocResult::Created => StatusCode::CREATED,
        DocResult::NotFound => StatusCode::NOT_FOUND,
        _ => StatusCode::OK,
    };
    let body = json!({
        "_index": index, "_id": id, "_version": w.version, "result": w.result,
        // ponytail: single node, so replicas are never assigned
        "_shards": {"total": shards_total, "successful": 1, "failed": 0},
        "_seq_no": w.seq_no, "_primary_term": 1
    });
    (status, Json(body))
}

pub fn put_or_create(
    state: &AppState,
    index: &str,
    id: &str,
    source: Value,
    create: bool,
) -> Reply {
    let source = check_source(source)?;
    let (w, total, index) = write(state, index, |i| match create {
        true => i.create(id, source),
        false => Ok(i.put(id, source)),
    })?;
    let w = w.map_err(|current_version| AppError::VersionConflict {
        id: id.to_owned(),
        current_version,
    })?;
    Ok(reply(&index, id, w, total))
}

pub async fn index_with_id(
    State(state): State<AppState>,
    Path((index, id)): Path<(String, String)>,
    Query(params): Query<HashMap<String, String>>,
    body: Bytes,
) -> Reply {
    let create = params.get("op_type").is_some_and(|o| o == "create");
    put_or_create(&state, &index, &id, parse_json(&body)?, create)
}

pub async fn index_auto_id(
    State(state): State<AppState>,
    Path(index): Path<String>,
    body: Bytes,
) -> Reply {
    put_or_create(&state, &index, &auto_id(), parse_json(&body)?, true)
}

pub async fn create(
    State(state): State<AppState>,
    Path((index, id)): Path<(String, String)>,
    body: Bytes,
) -> Reply {
    put_or_create(&state, &index, &id, parse_json(&body)?, true)
}

pub async fn get(
    State(state): State<AppState>,
    Path((index, id)): Path<(String, String)>,
    Query(params): Query<HashMap<String, String>>,
) -> Reply {
    let indices = state.read().expect("lock poisoned");
    let index = single_index(&indices, &index)?;
    let i = &indices[&index];
    // ponytail: no refresh model; a non-realtime get of an index with refresh disabled sees nothing
    let unrefreshed = params.get("realtime").is_some_and(|v| v == "false")
        && params.get("refresh").is_none_or(|v| v == "false")
        && i.settings.get("index.refresh_interval") == Some("-1");
    Ok(match i.get(&id).filter(|_| !unrefreshed) {
        Some(d) => (
            StatusCode::OK,
            Json(json!({
                "_index": index, "_id": id, "_version": d.version, "_seq_no": d.seq_no,
                "_primary_term": 1, "found": true, "_source": d.source
            })),
        ),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"_index": index, "_id": id, "found": false})),
        ),
    })
}

pub async fn delete(
    State(state): State<AppState>,
    Path((index, id)): Path<(String, String)>,
) -> Reply {
    delete_doc(&state, &index, &id)
}

pub fn delete_doc(state: &AppState, index: &str, id: &str) -> Reply {
    let (w, total, index) = write(state, index, |i| i.delete(id))?;
    Ok(reply(&index, id, w, total))
}

pub async fn update(
    State(state): State<AppState>,
    Path((index, id)): Path<(String, String)>,
    body: Bytes,
) -> Reply {
    update_doc(&state, &index, &id, &parse_json(&body)?)
}

pub fn update_doc(state: &AppState, index: &str, id: &str, body: &Value) -> Reply {
    let partial = body.get("doc").cloned().unwrap_or_else(|| json!({}));
    let upsert = match body["doc_as_upsert"].as_bool() == Some(true) {
        true => Some(partial.clone()),
        false => body.get("upsert").cloned(),
    };
    let (w, total, index) = write(state, index, |i| i.update(id, &partial, upsert))?;
    let w = w.ok_or_else(|| AppError::DocumentMissing(id.to_owned()))?;
    Ok(reply(&index, id, w, total))
}
