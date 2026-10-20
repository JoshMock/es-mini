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
    state: State<AppState>,
    Path(name): Path<String>,
    params: Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Value>, AppError> {
    settings_response(state, name, "*", params)
}

pub async fn get_settings_named(
    state: State<AppState>,
    Path((name, filter)): Path<(String, String)>,
    params: Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Value>, AppError> {
    settings_response(state, name, &filter, params)
}

fn settings_response(
    State(state): State<AppState>,
    name: String,
    filter: &str,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Value>, AppError> {
    let include_defaults = params.get("include_defaults").is_some_and(|v| v == "true");
    let render = |i: &Index| {
        let mut out =
            json!({"settings": i.settings.to_json_where(|k| es_index::aliases::glob(filter, k))});
        if include_defaults {
            // ponytail: only defaults for settings we model; add more as tests need them
            let mut defaults = json!({});
            if i.settings
                .get("index.mapping.dynamic_strings.auto_text")
                .is_none()
            {
                defaults =
                    json!({"index": {"mapping": {"dynamic_strings": {"auto_text": "true"}}}});
            }
            out["defaults"] = defaults;
        }
        out
    };
    // ponytail: single wildcard pattern only; no comma lists
    if name == "_all" || name.contains('*') {
        let indices = state.read().expect("lock poisoned");
        let pat = if name == "_all" { "*" } else { &name };
        let out: serde_json::Map<String, Value> = indices
            .iter()
            .filter(|(k, _)| es_index::aliases::glob(pat, k))
            .map(|(k, i)| (k.clone(), render(i)))
            .collect();
        return Ok(Json(Value::Object(out)));
    }
    let out = with_index(&state, name.clone(), |i| render(i))?;
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
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Value>, AppError> {
    let ignore = params
        .get("ignore_unavailable")
        .is_some_and(|v| v == "true");
    if name == "_all" || name.contains('*') {
        let name = if name == "_all" { "*" } else { &name };
        // ponytail: single wildcard pattern only; no comma lists
        let indices = state.read().expect("lock poisoned");
        let out: serde_json::Map<String, Value> = indices
            .iter()
            .filter(|(k, _)| es_index::aliases::glob(name, k))
            .map(|(k, i)| (k.clone(), json!({"mappings": i.mappings})))
            .collect();
        return Ok(Json(Value::Object(out)));
    }
    let mut out = serde_json::Map::new();
    for n in name.split(',') {
        match with_index(&state, n.to_string(), |i| json!(i.mappings)) {
            Ok(m) => out.insert(n.to_string(), json!({"mappings": m})),
            Err(_) if ignore && params.get("allow_no_indices").is_none_or(|v| v != "false") => None,
            Err(e) => return Err(e),
        };
    }
    Ok(Json(Value::Object(out)))
}

pub async fn get_all_mappings(state: State<AppState>) -> Result<Json<Value>, AppError> {
    get_mapping(state, Path("_all".to_string()), Query(Default::default())).await
}
