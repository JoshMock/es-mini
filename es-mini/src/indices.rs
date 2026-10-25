use crate::aliases::{parse_alias, reject_bad_scalars, resolve_date_math};
use crate::{AppState, error::AppError, params};
use axum::{
    Json,
    body::Bytes,
    extract::{Path, Query, State},
    http::{StatusCode, Uri},
};
use es_index::{FieldMapping, Index, Mappings};
use es_index::{Indices, aliases};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::collections::HashMap;

/// `IndicesOptions` parameters, accepted by every index-addressed API.
const INDICES_OPTIONS: [&str; 3] = ["ignore_unavailable", "allow_no_indices", "expand_wildcards"];

/// Rejects parameters outside `allowed` and the indices options.
pub fn check(
    uri: &Uri,
    params: &HashMap<String, String>,
    allowed: &[&str],
) -> Result<(), AppError> {
    let all: Vec<&str> = [&INDICES_OPTIONS[..], allowed].concat();
    params::check(uri, params, &all)
}

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

/// Port of `MetadataCreateIndexService.validateIndexOrAliasName` (index-name rules).
pub fn validate_index_name(name: &str) -> Result<(), AppError> {
    let bad = |reason: String| AppError::InvalidIndexName {
        index: name.to_owned(),
        reason,
    };
    if name.contains([' ', '"', '*', ',', '/', '<', '>', '?', '\\', '|']) {
        return Err(bad(
            r#"must not contain the following characters [' ','"','*',',','/','<','>','?','\','|']"#
                .into(),
        ));
    }
    if name.contains('#') {
        return Err(bad("must not contain '#'".into()));
    }
    if name.contains(':') {
        return Err(bad("must not contain ':'".into()));
    }
    if name.starts_with(['_', '-', '+']) {
        return Err(bad("must not start with '_', '-', or '+'".into()));
    }
    if name.len() > 255 {
        return Err(bad(format!(
            "index name is too long, ({} > 255)",
            name.len()
        )));
    }
    if name == "." || name == ".." {
        return Err(bad("must not be '.' or '..'".into()));
    }
    if name.to_lowercase() != name {
        return Err(bad("must be lowercase".into()));
    }
    Ok(())
}

pub async fn create(
    State(state): State<AppState>,
    Path(name): Path<String>,
    uri: Uri,
    Query(params): Query<HashMap<String, String>>,
    body: Bytes,
) -> Result<Json<Value>, AppError> {
    params::check(
        &uri,
        &params,
        &["timeout", "master_timeout", "wait_for_active_shards"],
    )?;
    validate_index_name(&resolve_date_math(&name))?;
    let body = parse_body(&body)?;
    let mut index = Index::default();
    index.settings.record_creation(&name);
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
    uri: Uri,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<Value>, AppError> {
    check(&uri, &params, &["timeout", "master_timeout"])?;
    let mut indices = state.write().expect("lock poisoned");
    let opts = aliases::IndicesOptions::DELETE.with_params(&params);
    let to_remove: Vec<String> = aliases::resolve(&indices, &name, &opts)?
        .into_iter()
        .map(|t| t.index)
        .collect();
    for n in to_remove {
        indices.remove(&n);
    }
    Ok(Json(json!({"acknowledged": true})))
}

/// Concrete indices a metadata request names (GET index, mappings, settings, open/close, ...),
/// honoring its `ignore_unavailable`, `allow_no_indices` and `expand_wildcards`.
fn read_indices(
    indices: &Indices,
    expr: &str,
    params: &HashMap<String, String>,
) -> Result<Vec<String>, AppError> {
    let opts = aliases::IndicesOptions::READ.with_params(params);
    Ok(aliases::resolve(indices, expr, &opts)?
        .into_iter()
        .map(|t| t.index)
        .collect())
}

/// Search/count targets for `expr`, honoring the request's indices options.
pub fn data_targets(
    indices: &Indices,
    expr: &str,
    params: &HashMap<String, String>,
) -> Result<Vec<aliases::Target>, AppError> {
    Ok(aliases::resolve(
        indices,
        expr,
        &aliases::IndicesOptions::DATA.with_params(params),
    )?)
}

const GET_PARAMS: [&str; 5] = [
    "flat_settings",
    "include_defaults",
    "local",
    "master_timeout",
    "features",
];

pub async fn get(
    State(state): State<AppState>,
    Path(name): Path<String>,
    uri: Uri,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<Value>, AppError> {
    check(&uri, &params, &GET_PARAMS)?;
    let indices = state.read().expect("lock poisoned");
    let body: Map<String, Value> = read_indices(&indices, &name, &params)?
        .into_iter()
        .map(|n| {
            let i = &indices[&n];
            let mut settings = i.settings.to_json();
            if params.get("human").is_some_and(|v| v == "true") {
                let created = i
                    .settings
                    .get("index.creation_date")
                    .and_then(|d| d.parse().ok());
                settings["index"]["creation_date_string"] =
                    json!(es_index::iso_date(created.unwrap_or(0)));
                settings["index"]["version"]["created_string"] = json!("9.5.5");
            }
            let mut out =
                json!({"aliases": i.aliases, "mappings": i.mappings, "settings": settings});
            if let Some(features) = params.get("features") {
                for (k, v) in out.as_object_mut().unwrap() {
                    if !features
                        .split(',')
                        .any(|f| f.trim_end_matches('s') == k.trim_end_matches('s'))
                    {
                        *v = json!({});
                    }
                }
            }
            (n, out)
        })
        .collect();
    Ok(Json(Value::Object(body)))
}

pub async fn head(State(state): State<AppState>, Path(name): Path<String>) -> StatusCode {
    match aliases::resolve_read(&state.read().expect("lock poisoned"), &name) {
        Ok(t) if !t.is_empty() => StatusCode::OK,
        _ => StatusCode::NOT_FOUND,
    }
}

pub async fn refresh_all(
    state: State<AppState>,
    uri: Uri,
    params: Query<HashMap<String, String>>,
) -> Result<Json<Value>, AppError> {
    refresh(state, Path("_all".into()), uri, params).await
}

pub async fn refresh(
    State(state): State<AppState>,
    Path(name): Path<String>,
    uri: Uri,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<Value>, AppError> {
    check(&uri, &params, &[])?;
    let mut indices = state.write().expect("lock poisoned");
    let names = read_indices(&indices, &name, &params)?;
    for n in &names {
        indices.get_mut(n).expect("resolved index exists").refresh();
    }
    let total = names.len();
    Ok(Json(
        json!({"_shards": {"total": total, "successful": total, "failed": 0}}),
    ))
}

pub async fn get_settings(
    state: State<AppState>,
    Path(name): Path<String>,
    uri: Uri,
    params: Query<HashMap<String, String>>,
) -> Result<Json<Value>, AppError> {
    settings_response(state, name, "*", uri, params)
}

pub async fn get_all_settings(
    state: State<AppState>,
    uri: Uri,
    params: Query<HashMap<String, String>>,
) -> Result<Json<Value>, AppError> {
    settings_response(state, "_all".into(), "*", uri, params)
}

pub async fn get_all_settings_named(
    state: State<AppState>,
    Path(filter): Path<String>,
    uri: Uri,
    params: Query<HashMap<String, String>>,
) -> Result<Json<Value>, AppError> {
    settings_response(state, "_all".into(), &filter, uri, params)
}

pub async fn get_settings_named(
    state: State<AppState>,
    Path((name, filter)): Path<(String, String)>,
    uri: Uri,
    params: Query<HashMap<String, String>>,
) -> Result<Json<Value>, AppError> {
    settings_response(state, name, &filter, uri, params)
}

fn settings_response(
    State(state): State<AppState>,
    name: String,
    filter: &str,
    uri: Uri,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<Value>, AppError> {
    check(&uri, &params, &GET_PARAMS)?;
    let filter = if filter == "_all" { "*" } else { filter };
    let include_defaults = params.get("include_defaults").is_some_and(|v| v == "true");
    let flat = params.get("flat_settings").is_some_and(|v| v == "true");
    let render = |i: &Index| {
        let mut out = json!({"settings": i.settings.to_json_where(|k| filter.split(',').any(|f| es_index::aliases::glob(f, k)))});
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
            if i.settings.get("index.refresh_interval").is_none() {
                defaults["index"]["refresh_interval"] = json!("1s");
            }
            out["defaults"] = defaults;
        }
        if flat {
            for v in out.as_object_mut().expect("object").values_mut() {
                let flat = es_index::flatten_settings(v)
                    .into_iter()
                    .map(|(k, x)| (k, x.clone()))
                    .collect();
                *v = Value::Object(flat);
            }
        }
        out
    };
    let indices = state.read().expect("lock poisoned");
    let out: Map<String, Value> = read_indices(&indices, &name, &params)?
        .into_iter()
        .map(|n| (n.clone(), render(&indices[&n])))
        .collect();
    Ok(Json(Value::Object(out)))
}

pub async fn put_settings(
    State(state): State<AppState>,
    Path(name): Path<String>,
    uri: Uri,
    Query(params): Query<HashMap<String, String>>,
    body: Bytes,
) -> Result<Json<Value>, AppError> {
    put_settings_on(state, name, uri, params, body)
}

/// Marks indices closed (or open) and returns their names.
fn set_closed(
    state: &AppState,
    name: &str,
    params: &HashMap<String, String>,
    closed: bool,
) -> Result<Vec<String>, AppError> {
    let mut indices = state.write().expect("lock poisoned");
    let names = read_indices(&indices, name, params)?;
    for n in &names {
        indices.get_mut(n).expect("resolved index exists").closed = closed;
    }
    Ok(names)
}

const OPEN_CLOSE_PARAMS: [&str; 3] = ["timeout", "master_timeout", "wait_for_active_shards"];

pub async fn close(
    State(state): State<AppState>,
    Path(name): Path<String>,
    uri: Uri,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<Value>, AppError> {
    check(&uri, &params, &OPEN_CLOSE_PARAMS)?;
    let closed: Map<String, Value> = set_closed(&state, &name, &params, true)?
        .into_iter()
        .map(|n| (n, json!({"closed": true})))
        .collect();
    Ok(Json(
        json!({"acknowledged": true, "shards_acknowledged": true, "indices": closed}),
    ))
}

pub async fn open(
    State(state): State<AppState>,
    Path(name): Path<String>,
    uri: Uri,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<Value>, AppError> {
    check(&uri, &params, &OPEN_CLOSE_PARAMS)?;
    set_closed(&state, &name, &params, false)?;
    Ok(Json(
        json!({"acknowledged": true, "shards_acknowledged": true}),
    ))
}

pub async fn put_all_settings(
    State(state): State<AppState>,
    uri: Uri,
    Query(params): Query<HashMap<String, String>>,
    body: Bytes,
) -> Result<Json<Value>, AppError> {
    put_settings_on(state, "_all".into(), uri, params, body)
}

const TOP_LEVEL_SETTINGS: [&str; 6] = [
    "codec",
    "number_of_shards",
    "number_of_replicas",
    "refresh_interval",
    "hidden",
    "max_result_window",
];

fn put_settings_on(
    state: AppState,
    name: String,
    uri: Uri,
    params: HashMap<String, String>,
    body: Bytes,
) -> Result<Json<Value>, AppError> {
    check(
        &uri,
        &params,
        &[
            "flat_settings",
            "master_timeout",
            "timeout",
            "preserve_existing",
            "reopen",
        ],
    )?;
    let body = parse_body(&body)?;
    // ponytail: index.codec is the only static setting checked; ES also rejects e.g. number_of_shards
    let settings = body.get("settings").unwrap_or(&body);
    // ponytail: only rejects unknown top-level index.* names; nested groups (index.mapping.*) pass
    for (key, _) in es_index::flatten_settings(settings) {
        let key = key.strip_prefix("index.").unwrap_or(&key);
        if !key.contains('.') && !TOP_LEVEL_SETTINGS.contains(&key) {
            return Err(AppError::IllegalArgument(format!(
                "unknown setting [index.{key}] please check that any required plugins are installed, or check the breaking changes documentation for removed settings"
            )));
        }
        // ponytail: codec is the only static setting modelled; it needs reopen=true
        if key == "codec" && params.get("reopen").map(String::as_str) != Some("true") {
            return Err(AppError::IllegalArgument(format!(
                "Can't update non dynamic settings [[index.{key}]] for open indices [{name}]"
            )));
        }
    }
    let mut indices = state.write().expect("lock poisoned");
    for n in read_indices(&indices, &name, &params)? {
        indices
            .get_mut(&n)
            .expect("resolved index exists")
            .settings
            .merge_with(
                settings,
                params.get("preserve_existing").map(String::as_str) == Some("true"),
            );
    }
    Ok(Json(json!({"acknowledged": true})))
}

pub async fn get_mapping(
    State(state): State<AppState>,
    Path(name): Path<String>,
    uri: Uri,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<Value>, AppError> {
    check(&uri, &params, &["local", "master_timeout"])?;
    let indices = state.read().expect("lock poisoned");
    let out: Map<String, Value> = read_indices(&indices, &name, &params)?
        .into_iter()
        .map(|n| (n.clone(), json!({"mappings": indices[&n].mappings})))
        .collect();
    Ok(Json(Value::Object(out)))
}

pub async fn get_all_mappings(
    state: State<AppState>,
    uri: Uri,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<Value>, AppError> {
    get_mapping(state, Path("_all".to_string()), uri, Query(params)).await
}

/// `GET [/{index}]/_stats[/{metric}]`: the `docs`, `fielddata` and `request_cache` metrics.
// ponytail: other metrics (store, indexing, search, ...) are left out of the response.
pub async fn stats(
    State(state): State<AppState>,
    path: Option<Path<HashMap<String, String>>>,
    uri: Uri,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<Value>, AppError> {
    check(
        &uri,
        &params,
        &[
            "fielddata_fields",
            "fields",
            "level",
            "completion_fields",
            "groups",
        ],
    )?;
    let path = path.map(|Path(p)| p).unwrap_or_default();
    let metric = path.get("metric").map_or("_all", String::as_str);
    let wanted = |m: &str| metric == "_all" || metric.split(',').any(|x| x == m);
    let indices = state.read().expect("lock poisoned");
    let names = read_indices(
        &indices,
        path.get("index").map_or("_all", String::as_str),
        &params,
    )?;
    let section = |docs: usize, (hits, misses, bytes): (u64, u64, usize)| {
        let mut s = Map::new();
        if wanted("docs") {
            s.insert("docs".into(), json!({"count": docs, "deleted": 0}));
        }
        if wanted("fielddata") {
            // es-mini reads keyword values from doc values and builds no fielddata.
            s.insert(
                "fielddata".into(),
                json!({"memory_size_in_bytes": 0, "evictions": 0}),
            );
        }
        if wanted("request_cache") {
            s.insert(
                "request_cache".into(),
                json!({"memory_size_in_bytes": bytes, "evictions": 0, "hit_count": hits, "miss_count": misses}),
            );
        }
        Value::Object(s)
    };
    let (mut total, mut cache_total) = (0, (0, 0, 0));
    let mut per_index = Map::new();
    for n in &names {
        let docs = indices[n].docs().count();
        total += docs;
        let cache = indices[n].request_cache_stats();
        cache_total = (
            cache_total.0 + cache.0,
            cache_total.1 + cache.1,
            cache_total.2 + cache.2,
        );
        let s = section(docs, cache);
        per_index.insert(n.clone(), json!({"primaries": s, "total": s}));
    }
    let all = section(total, cache_total);
    Ok(Json(json!({
        "_shards": {"total": names.len(), "successful": names.len(), "failed": 0},
        "_all": {"primaries": all, "total": all},
        "indices": per_index,
    })))
}
