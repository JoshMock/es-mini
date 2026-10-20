use crate::aliases::parse_alias;
use crate::{AppState, error::AppError};
use axum::{
    Json,
    body::Bytes,
    extract::{Path, State},
    http::StatusCode,
};
use es_index::{Index, Mappings};
use serde::Deserialize;
use serde_json::{Value, json};

pub fn parse_body(body: &Bytes) -> Result<Value, AppError> {
    if body.is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_slice(body).map_err(|e| AppError::Parse(e.to_string()))
}

pub async fn create(
    State(state): State<AppState>,
    Path(name): Path<String>,
    body: Bytes,
) -> Result<Json<Value>, AppError> {
    let body = parse_body(&body)?;
    let mut index = Index::default();
    index.settings.merge(&body["settings"]);
    if let Some(l) = index.settings.get("index.mapping.array_objects.limit")
        && !l.parse::<u64>().is_ok_and(|n| n > 0)
    {
        return Err(AppError::IllegalArgument(format!(
            "Failed to parse value [{l}] for setting [index.mapping.array_objects.limit] must be >= 1"
        )));
    }
    if !body["mappings"].is_null() {
        index.mappings = Mappings::deserialize(&body["mappings"])
            .map_err(|e| AppError::MapperParsing(e.to_string()))?;
    }
    for (alias, meta) in body["aliases"].as_object().into_iter().flatten() {
        index.aliases.insert(alias.clone(), parse_alias(meta)?);
    }
    let mut indices = state.write().expect("lock poisoned");
    if indices.contains_key(&name) {
        return Err(AppError::IndexExists(name));
    }
    if es_index::aliases::is_alias(&indices, &name) {
        return Err(AppError::InvalidIndexName {
            index: name,
            reason: "already exists as alias".into(),
        });
    }
    for alias in index.aliases.keys() {
        if indices.contains_key(alias) || *alias == name {
            return Err(AppError::InvalidAliasName {
                alias: alias.clone(),
                reason: "an index or data stream exists with the same name as the alias".into(),
            });
        }
    }
    indices.insert(name.clone(), index);
    Ok(Json(
        json!({"acknowledged": true, "shards_acknowledged": true, "index": name}),
    ))
}

pub async fn delete(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, AppError> {
    state
        .write()
        .expect("lock poisoned")
        .remove(&name)
        .ok_or(AppError::IndexNotFound(name))?;
    Ok(Json(json!({"acknowledged": true})))
}

/// Runs `f` against the named index, or fails with `index_not_found_exception`.
fn with_index<T>(
    state: &AppState,
    name: String,
    f: impl FnOnce(&mut Index) -> T,
) -> Result<T, AppError> {
    let mut indices = state.write().expect("lock poisoned");
    match indices.get_mut(&name) {
        Some(index) => Ok(f(index)),
        None => Err(AppError::IndexNotFound(name)),
    }
}

pub async fn get(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, AppError> {
    let body = with_index(
        &state,
        name.clone(),
        |i| json!({"aliases": i.aliases, "mappings": i.mappings, "settings": i.settings.to_json()}),
    )?;
    Ok(Json(json!({ name: body })))
}

pub async fn head(State(state): State<AppState>, Path(name): Path<String>) -> StatusCode {
    match state.read().expect("lock poisoned").contains_key(&name) {
        true => StatusCode::OK,
        false => StatusCode::NOT_FOUND,
    }
}

pub async fn refresh(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, AppError> {
    with_index(&state, name, Index::refresh)?;
    Ok(Json(
        json!({"_shards": {"total": 1, "successful": 1, "failed": 0}}),
    ))
}

pub async fn get_settings(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, AppError> {
    let settings = with_index(&state, name.clone(), |i| i.settings.to_json())?;
    Ok(Json(json!({ name: {"settings": settings} })))
}

pub async fn put_settings(
    State(state): State<AppState>,
    Path(name): Path<String>,
    body: Bytes,
) -> Result<Json<Value>, AppError> {
    let body = parse_body(&body)?;
    // ponytail: no static-vs-dynamic setting checks; ES rejects e.g. number_of_shards on an open index
    let settings = body.get("settings").unwrap_or(&body);
    with_index(&state, name, |i| i.settings.merge(settings))?;
    Ok(Json(json!({"acknowledged": true})))
}

pub async fn get_mapping(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<Value>, AppError> {
    let mappings = with_index(&state, name.clone(), |i| json!(i.mappings))?;
    Ok(Json(json!({ name: {"mappings": mappings} })))
}
