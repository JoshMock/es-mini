use crate::{AppState, error::AppError};
use axum::{
    Json,
    body::Bytes,
    extract::{Path, Query, State},
    http::StatusCode,
};
use es_index::{DocResult, Index, WriteResult};
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

/// Runs `f` against the named index, creating it first if missing (ES auto-create).
fn write<T>(state: &AppState, name: &str, f: impl FnOnce(&mut Index) -> T) -> (T, u64) {
    let mut indices = state.write().expect("lock poisoned");
    let index = indices.entry(name.to_owned()).or_default();
    let replicas = index
        .settings
        .get("index.number_of_replicas")
        .and_then(|r| r.parse().ok())
        .unwrap_or(0);
    (f(index), 1 + replicas)
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
    let (w, total) = write(state, index, |i| match create {
        true => i.create(id, source),
        false => Ok(i.put(id, source)),
    });
    let w = w.map_err(|current_version| AppError::VersionConflict {
        id: id.to_owned(),
        current_version,
    })?;
    Ok(reply(index, id, w, total))
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
) -> Reply {
    let indices = state.read().expect("lock poisoned");
    let i = indices
        .get(&index)
        .ok_or_else(|| AppError::IndexNotFound(index.clone()))?;
    Ok(match i.get(&id) {
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
    let (w, total) = write(state, index, |i| i.delete(id));
    Ok(reply(index, id, w, total))
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
    let (w, total) = write(state, index, |i| i.update(id, &partial, upsert));
    let w = w.ok_or_else(|| AppError::DocumentMissing(id.to_owned()))?;
    Ok(reply(index, id, w, total))
}
