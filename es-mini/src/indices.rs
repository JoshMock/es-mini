use crate::aliases::{parse_alias, reject_bad_scalars, resolve_date_math};
use crate::{AppState, error::AppError};
use axum::{
    Json,
    body::Bytes,
    extract::{Path, Query, State},
    http::StatusCode,
};
use es_index::{FieldMapping, Index, Mappings};
use serde::Deserialize;
use serde_json::{Value, json};

pub fn parse_body(body: &Bytes) -> Result<Value, AppError> {
    if body.is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_slice(body).map_err(|e| AppError::Parse(e.to_string()))
}

fn check_field_type(field: &str, spec: &Value) -> Result<(), AppError> {
    if FieldMapping::deserialize(spec).is_err()
        && let Some(t) = spec["type"].as_str()
    {
        return Err(AppError::MapperParsing(format!(
            "Failed to parse mapping: The mapper type [{t}] declared on field [{field}] does not exist. It might have been created within a future version or requires a plugin to be installed. Check the documentation."
        )));
    }
    for (sub, sub_spec) in spec["fields"].as_object().into_iter().flatten() {
        check_field_type(sub, sub_spec)?;
    }
    Ok(())
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
    if index.settings.get("index.soft_deletes.enabled") == Some("false") {
        return Err(AppError::IllegalArgument(
            "Creating indices with soft-deletes disabled is no longer supported. Please do not specify a value for setting [index.soft_deletes.enabled]."
                .into(),
        ));
    }
    if !body["mappings"].is_null() {
        for (field, spec) in body["mappings"]["properties"]
            .as_object()
            .into_iter()
            .flatten()
        {
            if field.is_empty() {
                return Err(AppError::IllegalArgument(
                    "field name cannot be an empty string".into(),
                ));
            }
            check_field_type(field, spec)?;
        }
        for (field, spec) in body["mappings"]["runtime"]
            .as_object()
            .into_iter()
            .flatten()
        {
            const TYPES: [&str; 9] = [
                "boolean",
                "composite",
                "date",
                "double",
                "geo_point",
                "ip",
                "keyword",
                "long",
                "lookup",
            ];
            if let Some(t) = spec["type"].as_str()
                && !TYPES.contains(&t)
            {
                return Err(AppError::MapperParsing(format!(
                    "Failed to parse mapping: The mapper type [{t}] declared on runtime field [{field}] does not exist. It might have been created within a future version or requires a plugin to be installed. Check the documentation."
                )));
            }
        }
        index.mappings = Mappings::deserialize(&body["mappings"])
            .map_err(|e| AppError::MapperParsing(e.to_string()))?;
    }
    for (alias, meta) in body["aliases"].as_object().into_iter().flatten() {
        reject_bad_scalars(alias, meta)?;
        index
            .aliases
            .insert(resolve_date_math(alias), parse_alias(meta)?);
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
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Value>, AppError> {
    let ignore = params
        .get("ignore_unavailable")
        .is_some_and(|v| v == "true");
    let mut indices = state.write().expect("lock poisoned");
    let mut to_remove = Vec::new();
    for n in name.split(',') {
        if n.contains('*') {
            // Wildcards match concrete indices only, never aliases.
            let before = to_remove.len();
            to_remove.extend(
                indices
                    .keys()
                    .filter(|k| es_index::aliases::glob(n, k))
                    .cloned(),
            );
            if to_remove.len() == before
                && params.get("allow_no_indices").is_some_and(|v| v == "false")
            {
                return Err(AppError::IndexNotFound(n.to_string()));
            }
        } else if indices.contains_key(n) {
            to_remove.push(n.to_string());
        } else if ignore {
            continue;
        } else if es_index::aliases::is_alias(&indices, n) {
            return Err(AppError::IllegalArgument(format!(
                "The provided expression [{n}] matches an alias, specify the corresponding concrete indices instead."
            )));
        } else {
            return Err(AppError::IndexNotFound(n.to_string()));
        }
    }
    for n in to_remove {
        indices.remove(&n);
    }
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
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Value>, AppError> {
    let indices = state.read().expect("lock poisoned");
    let name = if params
        .get("ignore_unavailable")
        .is_some_and(|v| v == "true")
    {
        let found = |p: &&str| es_index::aliases::resolve_read(&indices, p).is_ok();
        name.split(',').filter(found).collect::<Vec<_>>().join(",")
    } else {
        name
    };
    let body: serde_json::Map<String, Value> = es_index::aliases::resolve_read(&indices, &name)?
        .into_iter()
        .map(|t| {
            let i = &indices[&t.index];
            let body = json!({"aliases": i.aliases, "mappings": i.mappings, "settings": i.settings.to_json()});
            (t.index, body)
        })
        .collect();
    Ok(Json(Value::Object(body)))
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
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Value>, AppError> {
    let (settings, auto_text) = with_index(&state, name.clone(), |i| {
        (
            i.settings.to_json(),
            i.settings
                .get("index.mapping.dynamic_strings.auto_text")
                .is_none(),
        )
    })?;
    let mut out = json!({"settings": settings});
    if params.get("include_defaults").is_some_and(|v| v == "true") {
        // ponytail: only defaults for settings we model; add more as tests need them
        let mut defaults = json!({});
        if auto_text {
            defaults = json!({"index": {"mapping": {"dynamic_strings": {"auto_text": "true"}}}});
        }
        out["defaults"] = defaults;
    }
    Ok(Json(json!({ name: out })))
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
