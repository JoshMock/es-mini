use crate::aliases::{parse_alias, reject_bad_scalars, resolve_date_math};
use crate::{AppState, error::AppError, params};
use axum::{
    Json,
    body::Bytes,
    extract::{Path, Query, State},
    http::{StatusCode, Uri},
};
use es_index::aliases;
use es_index::index_settings::{self, Update};
use es_index::{FieldMapping, Index, Mappings};
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
    if !matches!(spec["type"].as_str(), Some("object" | "nested"))
        && FieldMapping::deserialize(spec).is_err()
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
    let name = resolve_date_math(&name)?;
    let privileges = "create_index,manage,all";
    crate::security::refuse_restricted(&name, "indices:admin/create", privileges)?;
    validate_index_name(&name)?;
    let body = parse_body(&body)?;
    let mut index = Index::default();
    index.settings.record_creation(&name);
    check_setting_lists(&body["settings"])?;
    index.settings.merge(&body["settings"]);
    for (key, value) in es_index::flatten_settings(&body["settings"]) {
        let key = format!("index.{}", key.strip_prefix("index.").unwrap_or(&key));
        if let Some(setting) = index_settings::get(&key)
            && !value.is_null()
        {
            (setting.validate_value(&es_index::setting_value(value)))
                .map_err(AppError::IllegalArgument)?;
        }
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
            .insert(resolve_date_math(alias)?, parse_alias(meta)?);
    }
    let mut indices = state.write();
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
    let mut indices = state.write();
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
    indices: &aliases::Map<impl aliases::IndexMeta>,
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
    indices: &aliases::Map<impl aliases::IndexMeta>,
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
    let shared = state.read();
    let names = read_indices(&*shared, &name, &params)?;
    let indices = shared.lock(&names);
    let body: Map<String, Value> = (names.into_iter())
        .map(|n| {
            let i = &indices[&n];
            let mut settings = i.settings.to_json();
            if params.get("human").is_some_and(|v| v == "true") {
                let created = i
                    .settings
                    .get(es_index::index_settings::CREATION_DATE)
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
    match aliases::resolve_read(&*state.read(), &name) {
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
    let mut indices = state.write();
    let names = read_indices(&indices, &name, &params)?;
    let mut total = 0;
    for n in &names {
        let index = indices.get_mut(n).expect("resolved index exists");
        index.refresh();
        total += index.number_of_shards();
    }
    Ok(Json(
        json!({"_shards": {"total": total, "successful": total, "failed": 0}}),
    ))
}

pub async fn flush_all(
    state: State<AppState>,
    uri: Uri,
    params: Query<HashMap<String, String>>,
) -> Result<Json<Value>, AppError> {
    flush(state, Path("_all".into()), uri, params).await
}

/// `_flush`: the persist after the request commits every shard of the indices (a no-op without
/// a data directory).
pub async fn flush(
    State(state): State<AppState>,
    Path(name): Path<String>,
    uri: Uri,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<Value>, AppError> {
    check(&uri, &params, &["force", "wait_if_ongoing"])?;
    let mut indices = state.write();
    let names = read_indices(&indices, &name, &params)?;
    let mut total = 0;
    for n in &names {
        let index = indices.get_mut(n).expect("resolved index exists");
        index.flush_requested = true;
        total += index.number_of_shards();
    }
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
        let keep = |k: &str| filter.split(',').any(|f| es_index::aliases::glob(f, k));
        let mut out = json!({"settings": i.settings.to_json_where(keep)});
        if include_defaults {
            out["defaults"] = i.settings.defaults().to_json_where(keep);
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
    let shared = state.read();
    let names = read_indices(&*shared, &name, &params)?;
    let indices = shared.lock(&names);
    let out: Map<String, Value> = (names.iter())
        .map(|n| (n.clone(), render(&indices[n])))
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
    let mut indices = state.write();
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

const REINDEX_DOCS: &str =
    "See https://www.elastic.co/docs/api/doc/elasticsearch/operation/operation-reindex";

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
    let settings = body.get("settings").unwrap_or(&body);
    check_setting_lists(settings)?;
    // Non-dynamic keys in the request, with their new values and whether they are final.
    let mut skipped = Vec::new();
    // Registered settings the request sets, with their new values.
    let mut values = Vec::new();
    // ponytail: the registry lacks some rarely used IndexScopedSettings (merge, store, time series); they fail as unknown
    for (key, value) in es_index::flatten_settings(settings) {
        let short = key.strip_prefix("index.").unwrap_or(&key);
        let key = format!("index.{short}");
        let value = (!value.is_null()).then(|| es_index::setting_value(value));
        let update = match index_settings::get(&key) {
            Some(s) => {
                if let Some(v) = &value {
                    values.push((s, v.clone()));
                }
                s.update
            }
            None => {
                return Err(AppError::IllegalArgument(format!(
                    "unknown setting [{key}] please check that any required plugins are installed, or check the breaking changes documentation for removed settings"
                )));
            }
        };
        match update {
            Update::Dynamic => {}
            Update::Private => {
                return Err(AppError::IllegalArgument(format!(
                    "can not update private setting [{key}]; this setting is managed by Elasticsearch"
                )));
            }
            Update::Static | Update::Final => {
                skipped.push((key, value, update == Update::Final));
            }
        }
    }
    let reopen = params.get("reopen").map(String::as_str) == Some("true");
    let mut indices = state.write();
    let names = read_indices(&indices, &name, &params)?;
    // Indices that take the non-dynamic settings: closed ones, and open ones `reopen` closes
    // because a value changes; open indices keep the rest unchanged.
    let mut as_closed = Vec::new();
    let mut open = Vec::new();
    for n in &names {
        let index = &indices[n];
        let changes = skipped
            .iter()
            .any(|(k, v, _)| v.as_ref() != index.settings.flat().get(k));
        if index.closed || (reopen && changes) {
            as_closed.push(n);
        } else {
            open.push(n);
        }
    }
    if !skipped.is_empty() && !open.is_empty() && !reopen {
        let list = |final_only: bool| {
            let keys: Vec<&str> = skipped
                .iter()
                .filter(|(_, _, f)| *f || !final_only)
                .map(|(k, _, _)| k.as_str())
                .collect();
            format!("[{}]", keys.join(", "))
        };
        let open: Vec<String> = open
            .iter()
            .map(|n| {
                format!(
                    "[{n}/{}]",
                    indices[*n]
                        .settings
                        .get(es_index::index_settings::UUID)
                        .unwrap_or("")
                )
            })
            .collect();
        let path = if skipped.iter().any(|(_, _, f)| *f) {
            format!(
                "The setting(s) [{}] cannot be modified on an index once it is created. You will need to create a new index with the desired setting(s) and reindex your data. {REINDEX_DOCS}",
                list(true)
            )
        } else {
            format!(
                "You can either resubmit the update with `?reopen=true`, or create a new index with the desired setting(s) and reindex your data. {REINDEX_DOCS}"
            )
        };
        return Err(AppError::IllegalArgument(format!(
            "Can't update non dynamic setting(s) [{}] for open indices [{}]. {path}",
            list(false),
            open.join(", ")
        )));
    }
    if let Some(n) = as_closed.first()
        && let Some((k, _, _)) = skipped.iter().find(|(_, _, f)| *f)
    {
        return Err(AppError::IllegalArgument(format!(
            "final {n} setting [{k}], not updateable"
        )));
    }
    for (setting, value) in &values {
        setting
            .validate_value(value)
            .map_err(AppError::IllegalArgument)?;
    }
    for n in names {
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
    let shared = state.read();
    let names = read_indices(&*shared, &name, &params)?;
    let indices = shared.lock(&names);
    let out: Map<String, Value> = (names.iter())
        .map(|n| (n.clone(), json!({"mappings": indices[n].mappings})))
        .collect();
    Ok(Json(Value::Object(out)))
}

/// Refuses settings whose lists hold objects, nulls or lists, as Settings.fromXContent does.
// ponytail: keys render in request order; Elasticsearch renders them in HashMap order
fn check_setting_lists(settings: &Value) -> Result<(), AppError> {
    match es_index::setting_lists_ok(settings) {
        true => Ok(()),
        false => Err(AppError::SettingsLists(settings.to_string())),
    }
}

/// `PUT|POST /{index}/_mapping`: merges the body into each index's mappings ([`Mappings::merged`]).
pub async fn put_mapping(
    State(state): State<AppState>,
    Path(name): Path<String>,
    uri: Uri,
    Query(params): Query<HashMap<String, String>>,
    body: Bytes,
) -> Result<Json<Value>, AppError> {
    check(&uri, &params, &["master_timeout", "timeout"])?;
    let body = parse_body(&body)?;
    for (field, spec) in body["properties"].as_object().into_iter().flatten() {
        check_field_type(field, spec)?;
    }
    let new = Mappings::deserialize(&body).map_err(|e| AppError::MapperParsing(e.to_string()))?;
    let mut indices = state.write();
    let names = read_indices(&indices, &name, &params)?;
    let merged = (names.iter())
        .map(|n| indices[n].mappings.merged(&new))
        .collect::<Result<Vec<_>, _>>()
        .map_err(AppError::IllegalArgument)?;
    for (n, m) in names.into_iter().zip(merged) {
        indices.get_mut(&n).expect("resolved index exists").mappings = m;
    }
    Ok(Json(json!({"acknowledged": true})))
}

pub async fn get_all_mappings(
    state: State<AppState>,
    uri: Uri,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<Value>, AppError> {
    get_mapping(state, Path("_all".to_string()), uri, Query(params)).await
}

/// `GET [/{index}]/_stats[/{metric}]`: every metric section Elasticsearch reports.
// ponytail: only docs and request_cache are counted; the other sections report zeros (es-mini keeps no such counters)
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
    let level = params.get("level").map_or("indices", String::as_str);
    if !["cluster", "indices", "shards"].contains(&level) {
        return Err(AppError::IllegalArgument(format!(
            "level parameter must be one of [cluster] or [indices] or [shards] but was [{level}]"
        )));
    }
    let path = path.map(|Path(p)| p).unwrap_or_default();
    let metric = path.get("metric").map_or("_all", String::as_str);
    let wanted = |m: &str| metric == "_all" || metric.split(',').any(|x| x == m);
    let shared = state.read();
    let names = read_indices(
        &*shared,
        path.get("index").map_or("_all", String::as_str),
        &params,
    )?;
    let indices = shared.lock(&names);
    let section = |docs: usize, (hits, misses, evictions, bytes): (u64, u64, u64, usize)| {
        let mut s = Map::new();
        if wanted("docs") {
            s.insert("docs".into(), json!({"count": docs, "deleted": 0}));
        }
        let zeros = |names: &[&str]| {
            Value::Object(names.iter().map(|k| (k.to_string(), json!(0))).collect())
        };
        let mut zero = |m: &str, names: &[&str]| {
            if wanted(m) {
                s.insert(m.into(), zeros(names));
            }
        };
        zero(
            "store",
            &[
                "size_in_bytes",
                "total_data_set_size_in_bytes",
                "reserved_in_bytes",
            ],
        );
        zero(
            "indexing",
            &[
                "index_total",
                "index_time_in_millis",
                "index_current",
                "index_failed",
                "delete_total",
                "delete_time_in_millis",
                "delete_current",
                "noop_update_total",
                "throttle_time_in_millis",
            ],
        );
        zero(
            "get",
            &[
                "total",
                "time_in_millis",
                "exists_total",
                "exists_time_in_millis",
                "missing_total",
                "missing_time_in_millis",
                "current",
            ],
        );
        zero(
            "search",
            &[
                "open_contexts",
                "query_total",
                "query_time_in_millis",
                "query_current",
                "fetch_total",
                "fetch_time_in_millis",
                "fetch_current",
                "scroll_total",
                "scroll_time_in_millis",
                "scroll_current",
                "suggest_total",
                "suggest_time_in_millis",
                "suggest_current",
            ],
        );
        zero(
            "merges",
            &[
                "current",
                "current_docs",
                "current_size_in_bytes",
                "total",
                "total_time_in_millis",
                "total_docs",
                "total_size_in_bytes",
                "total_stopped_time_in_millis",
                "total_throttled_time_in_millis",
                "total_auto_throttle_in_bytes",
            ],
        );
        zero(
            "refresh",
            &[
                "total",
                "total_time_in_millis",
                "external_total",
                "external_total_time_in_millis",
                "listeners",
            ],
        );
        zero("flush", &["total", "periodic", "total_time_in_millis"]);
        zero("warmer", &["current", "total", "total_time_in_millis"]);
        zero(
            "query_cache",
            &[
                "memory_size_in_bytes",
                "total_count",
                "hit_count",
                "miss_count",
                "cache_size",
                "cache_count",
                "evictions",
            ],
        );
        zero("completion", &["size_in_bytes"]);
        zero(
            "segments",
            &[
                "count",
                "memory_in_bytes",
                "terms_memory_in_bytes",
                "stored_fields_memory_in_bytes",
                "term_vectors_memory_in_bytes",
                "norms_memory_in_bytes",
                "points_memory_in_bytes",
                "doc_values_memory_in_bytes",
                "index_writer_memory_in_bytes",
                "version_map_memory_in_bytes",
                "fixed_bit_set_memory_in_bytes",
            ],
        );
        zero(
            "translog",
            &[
                "operations",
                "size_in_bytes",
                "uncommitted_operations",
                "uncommitted_size_in_bytes",
                "earliest_last_modified_age",
            ],
        );
        zero(
            "recovery",
            &[
                "current_as_source",
                "current_as_target",
                "throttle_time_in_millis",
            ],
        );
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
                json!({"memory_size_in_bytes": bytes, "evictions": evictions, "hit_count": hits, "miss_count": misses}),
            );
        }
        Value::Object(s)
    };
    let (mut total, mut cache_total) = (0, (0, 0, 0, 0));
    let mut per_index = Map::new();
    for n in &names {
        let docs = indices[n].docs().count();
        total += docs;
        let cache = indices[n].request_cache_stats();
        cache_total = (
            cache_total.0 + cache.0,
            cache_total.1 + cache.1,
            cache_total.2 + cache.2,
            cache_total.3 + cache.3,
        );
        let s = section(docs, cache);
        let mut stats = json!({"primaries": s, "total": s});
        if level == "shards" {
            let routing = json!({"state": "STARTED", "primary": true, "node": crate::nodes::NODE_ID, "relocating_node": null});
            // ponytail: the request cache is counted per index, so shards report it empty; no
            // commit, seq_no, retention_leases or shard_path sections
            let shards: Map<String, Value> = (0..indices[n].number_of_shards())
                .map(|i| {
                    let docs = indices[n].shards().get(i).map_or(0, |s| s.docs().count());
                    let mut copy = Map::from_iter([("routing".to_string(), routing.clone())]);
                    copy.extend(
                        section(docs, (0, 0, 0, 0))
                            .as_object()
                            .cloned()
                            .unwrap_or_default(),
                    );
                    (i.to_string(), json!([copy]))
                })
                .collect();
            stats["shards"] = Value::Object(shards);
        }
        per_index.insert(n.clone(), stats);
    }
    let all = section(total, cache_total);
    let shards: usize = names.iter().map(|n| indices[n].number_of_shards()).sum();
    let mut out = json!({
        "_shards": {"total": shards, "successful": shards, "failed": 0},
        "_all": {"primaries": all, "total": all},
    });
    if level != "cluster" {
        out["indices"] = Value::Object(per_index);
    }
    Ok(Json(out))
}

/// `GET /_cat/shards[/{index}]`: one row per shard, as JSON with `format=json`, else as an
/// aligned text table (with a header row if `v`).
// ponytail: only the default columns; no h, s, bytes or help. Store sizes are 0b like _stats.
pub async fn cat_shards(
    State(state): State<AppState>,
    path: Option<Path<String>>,
    uri: Uri,
    Query(params): Query<HashMap<String, String>>,
) -> Result<axum::response::Response, AppError> {
    use axum::response::IntoResponse;
    check(&uri, &params, &["format", "v"])?;
    const COLUMNS: [&str; 9] = [
        "index", "shard", "prirep", "state", "docs", "store", "dataset", "ip", "node",
    ];
    let shared = state.read();
    let expr = path.map_or("_all".to_string(), |Path(p)| p);
    let names = read_indices(&*shared, &expr, &params)?;
    let indices = shared.lock(&names);
    let rows: Vec<Vec<String>> = (names.iter())
        .flat_map(|n| (0..indices[n].number_of_shards()).map(move |i| (n, i)))
        .map(|(n, i)| {
            let docs = indices[n].shards().get(i).map_or(0, |s| s.docs().count());
            let (i, docs) = (i.to_string(), docs.to_string());
            [
                n,
                &i,
                "p",
                "STARTED",
                &docs,
                "0b",
                "0b",
                "127.0.0.1",
                crate::CLUSTER_NAME,
            ]
            .map(str::to_string)
            .to_vec()
        })
        .collect();
    if params.get("format").map(String::as_str) == Some("json") {
        let rows: Vec<Map<String, Value>> = (rows.into_iter())
            .map(|r| {
                COLUMNS
                    .iter()
                    .map(|c| c.to_string())
                    .zip(r.into_iter().map(Value::from))
                    .collect()
            })
            .collect();
        return Ok(Json(rows).into_response());
    }
    let header = params.get("v").is_some_and(|v| v != "false");
    let table: Vec<Vec<String>> = (header
        .then(|| COLUMNS.map(str::to_string).to_vec())
        .into_iter())
    .chain(rows)
    .collect();
    let width = |c: usize| table.iter().map(|r| r[c].len()).max().unwrap_or(0);
    let mut out = String::new();
    for row in &table {
        for (c, cell) in row.iter().enumerate() {
            let w = width(c);
            // docs, store and dataset align right, as RestShardsAction declares
            match c {
                4..=6 => out += &format!("{cell:>w$} "),
                8 => out += cell,
                _ => out += &format!("{cell:<w$} "),
            }
        }
        out.push('\n');
    }
    Ok(([("content-type", "text/plain; charset=UTF-8")], out).into_response())
}
