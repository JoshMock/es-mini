use crate::{AppState, error::AppError, params};
use axum::{
    Json,
    body::Bytes,
    extract::{Path, Query, State},
    http::{StatusCode, Uri},
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

/// Counts objects held in arrays, at any depth.
fn array_objects(v: &Value, in_array: bool) -> usize {
    match v {
        Value::Object(m) => {
            usize::from(in_array) + m.values().map(|v| array_objects(v, false)).sum::<usize>()
        }
        Value::Array(a) => a.iter().map(|v| array_objects(v, true)).sum(),
        _ => 0,
    }
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
    if !indices.contains_key(&target) {
        crate::indices::validate_index_name(&target)?;
    }
    let index = indices.entry(target.clone()).or_insert_with(|| {
        let mut created = Index::default();
        created.settings.record_creation(&target);
        created
    });
    let replicas = index
        .settings
        .get("index.number_of_replicas")
        .and_then(|r| r.parse().ok())
        .unwrap_or(0);
    Ok((f(index), 1 + replicas, target))
}

/// The one concrete index a single-document read targets.
fn single_index(indices: &es_index::Indices, name: &str) -> Result<String, AppError> {
    let targets = aliases::resolve_data(indices, name)?;
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
    if id.len() > 512 {
        return Err(AppError::IllegalArgument(format!(
            "id [{id}] is too long, must be no longer than 512 bytes but was: {}",
            id.len()
        )));
    }
    let source = check_source(source)?;
    let (w, total, index) = write(state, index, |i| {
        let limit = i
            .settings
            .get("index.mapping.array_objects.limit")
            .and_then(|l| l.parse().ok())
            .unwrap_or(10000);
        if array_objects(&source, false) > limit {
            return Err(AppError::DocumentParsing(format!(
                "failed to parse: The total number of objects across all arrays in a document has exceeded the allowed limit of [{limit}]."
            )));
        }
        i.map_dynamic(&source);
        Ok(match create {
            true => i.create(id, source),
            false => Ok(i.put(id, source)),
        })
    })?;
    let w = w?.map_err(|current_version| AppError::VersionConflict {
        id: id.to_owned(),
        current_version,
    })?;
    Ok(reply(&index, id, w, total))
}

fn force_refresh(
    state: &AppState,
    params: &HashMap<String, String>,
    index: &str,
    reply: &mut (StatusCode, Json<Value>),
) -> Result<(), AppError> {
    if params
        .get("refresh")
        .is_some_and(|v| v.is_empty() || v == "true")
    {
        write(state, index, Index::refresh)?;
        reply.1.0["forced_refresh"] = true.into();
    }
    Ok(())
}

fn require_alias(
    state: &AppState,
    params: &HashMap<String, String>,
    index: &str,
) -> Result<(), AppError> {
    let flagged = params.get("require_alias").is_some_and(|v| v == "true");
    match flagged && !aliases::is_alias(&state.read().expect("lock poisoned"), index) {
        true => Err(AppError::RequireAlias(index.to_owned())),
        false => Ok(()),
    }
}

/// Applies the `require_alias` check before, and `refresh` after, a single-document write.
fn with_write_params(
    state: &AppState,
    params: &HashMap<String, String>,
    index: &str,
    op: impl FnOnce() -> Reply,
) -> Reply {
    require_alias(state, params, index)?;
    let mut reply = op()?;
    force_refresh(state, params, index, &mut reply)?;
    Ok(reply)
}

const WRITE_PARAMS: [&str; 14] = [
    "routing",
    "refresh",
    "op_type",
    "version",
    "version_type",
    "if_seq_no",
    "if_primary_term",
    "pipeline",
    "require_alias",
    "require_data_stream",
    "timeout",
    "wait_for_active_shards",
    "include_source_on_error",
    "master_timeout",
];

pub async fn index_with_id(
    State(state): State<AppState>,
    Path((index, id)): Path<(String, String)>,
    uri: Uri,
    Query(params): Query<HashMap<String, String>>,
    body: Bytes,
) -> Reply {
    params::check(&uri, &params, &WRITE_PARAMS)?;
    let create = params.get("op_type").is_some_and(|o| o == "create");
    with_write_params(&state, &params, &index, || {
        put_or_create(&state, &index, &id, parse_json(&body)?, create)
    })
}

pub async fn index_auto_id(
    State(state): State<AppState>,
    Path(index): Path<String>,
    uri: Uri,
    Query(params): Query<HashMap<String, String>>,
    body: Bytes,
) -> Reply {
    params::check(&uri, &params, &WRITE_PARAMS)?;
    with_write_params(&state, &params, &index, || {
        put_or_create(&state, &index, &auto_id(), parse_json(&body)?, true)
    })
}

pub async fn create(
    State(state): State<AppState>,
    Path((index, id)): Path<(String, String)>,
    uri: Uri,
    Query(params): Query<HashMap<String, String>>,
    body: Bytes,
) -> Reply {
    params::check(&uri, &params, &WRITE_PARAMS)?;
    with_write_params(&state, &params, &index, || {
        put_or_create(&state, &index, &id, parse_json(&body)?, true)
    })
}

const GET_PARAMS: [&str; 11] = [
    "routing",
    "refresh",
    "realtime",
    "preference",
    "version",
    "version_type",
    "stored_fields",
    "_source",
    "_source_includes",
    "_source_excludes",
    "force_synthetic_source",
];

pub async fn get(
    State(state): State<AppState>,
    Path((index, id)): Path<(String, String)>,
    uri: Uri,
    Query(params): Query<HashMap<String, String>>,
) -> Reply {
    params::check(&uri, &params, &GET_PARAMS)?;
    let indices = state.read().expect("lock poisoned");
    let index = single_index(&indices, &index)?;
    let i = &indices[&index];
    // A non-realtime get sees what search sees, unless it asks for a refresh first.
    let realtime = params.get("realtime").is_none_or(|v| v != "false")
        || params.get("refresh").is_some_and(|v| v != "false");
    Ok(match i.get(&id).filter(|d| realtime || i.searchable(d)) {
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

const DELETE_PARAMS: [&str; 10] = [
    "routing",
    "refresh",
    "version",
    "version_type",
    "if_seq_no",
    "if_primary_term",
    "require_alias",
    "timeout",
    "wait_for_active_shards",
    "master_timeout",
];

pub async fn delete(
    State(state): State<AppState>,
    Path((index, id)): Path<(String, String)>,
    uri: Uri,
    Query(params): Query<HashMap<String, String>>,
) -> Reply {
    params::check(&uri, &params, &DELETE_PARAMS)?;
    with_write_params(&state, &params, &index, || delete_doc(&state, &index, &id))
}

pub fn delete_doc(state: &AppState, index: &str, id: &str) -> Reply {
    let (w, total, index) = write(state, index, |i| i.delete(id))?;
    Ok(reply(&index, id, w, total))
}

const UPDATE_PARAMS: [&str; 13] = [
    "routing",
    "refresh",
    "retry_on_conflict",
    "if_seq_no",
    "if_primary_term",
    "require_alias",
    "timeout",
    "wait_for_active_shards",
    "_source",
    "_source_includes",
    "_source_excludes",
    "lang",
    "include_source_on_error",
];

pub async fn update(
    State(state): State<AppState>,
    Path((index, id)): Path<(String, String)>,
    uri: Uri,
    Query(params): Query<HashMap<String, String>>,
    body: Bytes,
) -> Reply {
    params::check(&uri, &params, &UPDATE_PARAMS)?;
    with_write_params(&state, &params, &index, || {
        update_doc(&state, &index, &id, &parse_json(&body)?)
    })
}

pub fn update_doc(state: &AppState, index: &str, id: &str, body: &Value) -> Reply {
    const FIELDS: [&str; 8] = [
        "doc",
        "upsert",
        "doc_as_upsert",
        "detect_noop",
        "script",
        "scripted_upsert",
        "_source",
        "lang",
    ];
    if let Some(k) = body
        .as_object()
        .and_then(|o| o.keys().find(|k| !FIELDS.contains(&k.as_str())))
    {
        let near = FIELDS.iter().find(|f| edit_distance(f, k) <= 2);
        let hint = near.map_or(String::new(), |f| format!(" did you mean [{f}]?"));
        return Err(AppError::Parse(format!(
            "[UpdateRequest] unknown field [{k}]{hint}"
        )));
    }
    let partial = body.get("doc").cloned().unwrap_or_else(|| json!({}));
    let upsert = match body["doc_as_upsert"].as_bool() == Some(true) {
        true => Some(partial.clone()),
        false => body.get("upsert").cloned(),
    };
    let detect_noop = body["detect_noop"].as_bool() != Some(false);
    let (w, total, index) = write(state, index, |i| {
        i.update(id, &partial, upsert, detect_noop)
    })?;
    let w = w.ok_or_else(|| AppError::DocumentMissing(id.to_owned()))?;
    Ok(reply(&index, id, w, total))
}

fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut prev = std::mem::replace(&mut row[0], i + 1);
        for (j, cb) in b.iter().enumerate() {
            let cur = (prev + usize::from(ca != *cb))
                .min(row[j] + 1)
                .min(row[j + 1] + 1);
            prev = std::mem::replace(&mut row[j + 1], cur);
        }
    }
    row[b.len()]
}
