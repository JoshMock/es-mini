use crate::aliases::{parse_alias, reject_bad_scalars, resolve_date_math};
use crate::{AppState, error::AppError, params};
use axum::{
    Extension, Json,
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
    // the multi-fields are checked after their parent's type
    let mut own = spec.clone();
    own.as_object_mut().map(|o| o.remove("fields"));
    if !matches!(spec["type"].as_str(), Some("object" | "nested"))
        && FieldMapping::deserialize(&own).is_err()
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

/// Refuses a runtime field of `mappings` whose type Elasticsearch does not know.
fn check_runtime_types(mappings: &Value) -> Result<(), AppError> {
    for (field, spec) in mappings["runtime"].as_object().into_iter().flatten() {
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
    Ok(())
}

/// A mapping body that does not parse, as Elasticsearch reports it.
fn mapping_error(e: serde_json::Error) -> AppError {
    let e = e.to_string();
    if !e.starts_with(es_index::mapping::BAD_DYNAMIC) {
        return AppError::MapperParsing(e);
    }
    let convert = "Could not convert [dynamic.dynamic] to boolean";
    let cause = json!({"type": "illegal_argument_exception", "reason": convert,
        "caused_by": {"type": "illegal_argument_exception", "reason": e}});
    let error = AppError::MapperParsing(format!("Failed to parse mapping: {convert}"));
    AppError::Caused(Box::new(error), cause)
}

/// Port of `MetadataCreateIndexService.validateIndexOrAliasName`: why `name` is refused.
pub(crate) fn index_or_alias_name_error(name: &str) -> Option<String> {
    if name.contains([' ', '"', '*', ',', '/', '<', '>', '?', '\\', '|']) {
        return Some(
            r#"must not contain the following characters [' ','"','*',',','/','<','>','?','\','|']"#
                .into(),
        );
    }
    if name.contains('#') {
        return Some("must not contain '#'".into());
    }
    if name.contains(':') {
        return Some("must not contain ':'".into());
    }
    if name.starts_with(['_', '-', '+']) {
        return Some("must not start with '_', '-', or '+'".into());
    }
    if name.len() > 255 {
        return Some(format!("index name is too long, ({} > 255)", name.len()));
    }
    if name == "." || name == ".." {
        return Some("must not be '.' or '..'".into());
    }
    None
}

/// Port of `MetadataCreateIndexService.validateIndexName`'s name rules.
pub fn validate_index_name(name: &str) -> Result<(), AppError> {
    let reason = index_or_alias_name_error(name)
        .or_else(|| (name.to_lowercase() != name).then(|| "must be lowercase".into()));
    match reason {
        Some(reason) => Err(AppError::InvalidIndexName {
            index: name.to_owned(),
            reason,
        }),
        None => Ok(()),
    }
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
    crate::security::refuse_restricted(&state, &name, "indices:admin/create", privileges)?;
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
        check_runtime_types(&body["mappings"])?;
        index.mappings = Mappings::deserialize(&body["mappings"]).map_err(mapping_error)?;
    }
    if let Some((reason, cause)) = es_index::disk::index_sort_error(&index, &name) {
        let error = AppError::IllegalArgument(reason);
        return Err(match cause {
            Some(c) => AppError::Caused(
                Box::new(error),
                json!({"type": "illegal_argument_exception", "reason": c}),
            ),
            None => error,
        });
    }
    for (alias, meta) in body["aliases"].as_object().into_iter().flatten() {
        reject_bad_scalars(alias, meta)?;
        index
            .aliases
            .insert(resolve_date_math(alias)?, parse_alias(meta)?);
    }
    // From its descriptor, as ES does; `PUT /.security` (the alias name) is an ordinary index.
    if name == crate::security::INDEX {
        index = crate::security::managed_index(&name).expect("a managed index");
    }
    crate::system::hide(&mut index, &name)?;
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
        crate::aliases::validate_alias_name(alias)?;
        if indices.contains_key(alias) || *alias == name {
            return Err(AppError::InvalidAliasName {
                alias: alias.clone(),
                reason: crate::aliases::SAME_NAME.into(),
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
    authn: Option<Extension<crate::security::Authn>>,
    Path(name): Path<String>,
    uri: Uri,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<Value>, AppError> {
    check(&uri, &params, &["timeout", "master_timeout"])?;
    if authn.is_some() {
        let privileges = "delete_index,manage,all";
        crate::security::refuse_restricted(&state, &name, "indices:admin/delete", privileges)?;
    }
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

/// Search/count targets for `expr`, honoring the request's indices options and `routing`.
pub fn data_targets(
    indices: &aliases::Map<impl aliases::IndexMeta>,
    expr: &str,
    params: &HashMap<String, String>,
) -> Result<Vec<aliases::Target>, AppError> {
    let mut targets = aliases::resolve(
        indices,
        expr,
        &aliases::IndicesOptions::DATA.with_params(params),
    )?;
    let routing = params.get("routing").map(String::as_str);
    targets.iter_mut().for_each(|t| t.route(routing));
    Ok(targets)
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
    for n in &names {
        indices.get_mut(n).expect("resolved index exists").refresh();
    }
    Ok(Json(
        json!({"_shards": broadcast_shards(names.iter().map(|n| &indices[n]))}),
    ))
}

/// The `_shards` header of a broadcast to every shard copy of `indices`: their unassigned
/// replicas count in the total, as Elasticsearch counts them on one node.
fn broadcast_shards<'a>(indices: impl Iterator<Item = &'a Index>) -> Value {
    let (mut total, mut successful) = (0, 0);
    for index in indices {
        successful += index.number_of_shards();
        total += index.number_of_shards() * (1 + index.number_of_replicas());
    }
    json!({"total": total, "successful": successful, "failed": 0})
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
    for n in &names {
        indices
            .get_mut(n)
            .expect("resolved index exists")
            .flush_requested = true;
    }
    Ok(Json(
        json!({"_shards": broadcast_shards(names.iter().map(|n| &indices[n]))}),
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
            Update::Internal => {
                return Err(AppError::IllegalArgument(format!(
                    "can not update internal setting [{key}]; this setting is managed via a dedicated API"
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
    check_runtime_types(&body)?;
    let new = Mappings::deserialize(&body).map_err(mapping_error)?;
    let mut indices = state.write();
    let names = read_indices(&indices, &name, &params)?;
    let merged = (names.iter())
        .map(|n| indices[n].mappings.merged(&new))
        .collect::<Result<Vec<_>, _>>()
        .map_err(AppError::IllegalArgument)?;
    for (n, m) in names.into_iter().zip(merged) {
        indices
            .get_mut(&n)
            .expect("resolved index exists")
            .set_mappings(m);
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
// ponytail: only docs, indexing's index_total and delete_total, and request_cache are counted; the rest of indexing and the other sections report zeros (es-mini keeps no such counters)
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
    let section = |docs: usize,
                   (index_total, delete_total): (u64, u64),
                   (hits, misses, evictions, bytes): (u64, u64, u64, usize)| {
        let mut s = Map::new();
        if wanted("docs") {
            s.insert("docs".into(), json!({"count": docs, "deleted": 0}));
        }
        let zeros = |names: &[&str]| {
            Value::Object(names.iter().map(|k| (k.to_string(), json!(0))).collect())
        };
        if wanted("indexing") {
            let mut ix = zeros(&[
                "index_total",
                "index_time_in_millis",
                "index_current",
                "index_failed",
                "delete_total",
                "delete_time_in_millis",
                "delete_current",
                "noop_update_total",
                "throttle_time_in_millis",
            ]);
            ix["index_total"] = json!(index_total);
            ix["delete_total"] = json!(delete_total);
            s.insert("indexing".into(), ix);
        }
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
    let (mut total, mut writes_total, mut cache_total) = (0, (0, 0), (0, 0, 0, 0));
    let mut per_index = Map::new();
    for n in &names {
        let docs = indices[n].docs().count();
        total += docs;
        let cache = (indices[n].shards().iter().map(|s| s.request_cache_stats()))
            .fold((0, 0, 0, 0), |a, c| {
                (a.0 + c.0, a.1 + c.1, a.2 + c.2, a.3 + c.3)
            });
        cache_total = (
            cache_total.0 + cache.0,
            cache_total.1 + cache.1,
            cache_total.2 + cache.2,
            cache_total.3 + cache.3,
        );
        let writes = (indices[n].shards().iter().map(|s| s.write_totals()))
            .fold((0, 0), |a, w| (a.0 + w.0, a.1 + w.1));
        writes_total = (writes_total.0 + writes.0, writes_total.1 + writes.1);
        let s = section(docs, writes, cache);
        let mut stats = json!({"primaries": s, "total": s});
        if level == "shards" {
            let routing = json!({"state": "STARTED", "primary": true, "node": crate::nodes::NODE_ID, "relocating_node": null});
            // ponytail: no commit, seq_no, retention_leases or shard_path sections
            let shards: Map<String, Value> = (0..indices[n].number_of_shards())
                .map(|i| {
                    let shard = indices[n].shards().get(i);
                    let docs = shard.map_or(0, |s| s.docs().count());
                    let writes = shard.map_or((0, 0), |s| s.write_totals());
                    let cache = shard.map_or((0, 0, 0, 0), |s| s.request_cache_stats());
                    let mut copy = Map::from_iter([("routing".to_string(), routing.clone())]);
                    copy.extend(
                        section(docs, writes, cache)
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
    let all = section(total, writes_total, cache_total);
    let mut out = json!({
        "_shards": broadcast_shards(names.iter().map(|n| &*indices[n])),
        "_all": {"primaries": all, "total": all},
    });
    if level != "cluster" {
        out["indices"] = Value::Object(per_index);
    }
    Ok(Json(out))
}

/// `GET /_cat/shards[/{index}]`: one row per shard (see [`cat_table`]).
// ponytail: path.data, path.state and unassigned.for report no value; store sizes and every counter es-mini keeps none of (merges, search, get, flush, ...) are 0 like _stats
pub async fn cat_shards(
    State(state): State<AppState>,
    path: Option<Path<String>>,
    uri: Uri,
    Query(params): Query<HashMap<String, String>>,
) -> Result<axum::response::Response, AppError> {
    check(&uri, &params, &["format", "v", "h", "s", "bytes", "help"])?;
    // name, aliases and description, as RestShardsAction declares them
    const COLUMNS: [(&str, &str, &str); 79] = [
        ("index", "i,idx", "index name"),
        ("shard", "s,sh", "shard name"),
        ("prirep", "p,pr,primaryOrReplica", "primary or replica"),
        ("state", "st", "shard state"),
        ("docs", "d,dc", "number of docs in shard"),
        (
            "store",
            "sto",
            "store size of shard (how much disk it uses)",
        ),
        ("dataset", "", "total size of dataset"),
        ("ip", "", "ip of node where it lives"),
        ("id", "", "unique id of node where it lives"),
        ("node", "n", "name of node where it lives"),
        ("unassigned.reason", "ur", "reason shard became unassigned"),
        ("unassigned.at", "ua", "time shard became unassigned (UTC)"),
        ("unassigned.for", "uf", "time has been unassigned"),
        (
            "unassigned.details",
            "ud",
            "additional details as to why the shard became unassigned",
        ),
        ("recoverysource.type", "rs", "recovery source type"),
        ("completion.size", "cs,completionSize", "size of completion"),
        (
            "fielddata.memory_size",
            "fm,fielddataMemory",
            "used fielddata cache",
        ),
        (
            "fielddata.evictions",
            "fe,fielddataEvictions",
            "fielddata evictions",
        ),
        (
            "query_cache.memory_size",
            "qcm,queryCacheMemory",
            "used query cache",
        ),
        (
            "query_cache.evictions",
            "qce,queryCacheEvictions",
            "query cache evictions",
        ),
        ("flush.total", "ft,flushTotal", "number of flushes"),
        (
            "flush.total_time",
            "ftt,flushTotalTime",
            "time spent in flush",
        ),
        ("get.current", "gc,getCurrent", "number of current get ops"),
        ("get.time", "gti,getTime", "time spent in get"),
        ("get.total", "gto,getTotal", "number of get ops"),
        (
            "get.exists_time",
            "geti,getExistsTime",
            "time spent in successful gets",
        ),
        (
            "get.exists_total",
            "geto,getExistsTotal",
            "number of successful gets",
        ),
        (
            "get.missing_time",
            "gmti,getMissingTime",
            "time spent in failed gets",
        ),
        (
            "get.missing_total",
            "gmto,getMissingTotal",
            "number of failed gets",
        ),
        (
            "indexing.delete_current",
            "idc,indexingDeleteCurrent",
            "number of current deletions",
        ),
        (
            "indexing.delete_time",
            "idti,indexingDeleteTime",
            "time spent in deletions",
        ),
        (
            "indexing.delete_total",
            "idto,indexingDeleteTotal",
            "number of delete ops",
        ),
        (
            "indexing.index_current",
            "iic,indexingIndexCurrent",
            "number of current indexing ops",
        ),
        (
            "indexing.index_time",
            "iiti,indexingIndexTime",
            "time spent in indexing",
        ),
        (
            "indexing.index_total",
            "iito,indexingIndexTotal",
            "number of indexing ops",
        ),
        (
            "indexing.index_failed",
            "iif,indexingIndexFailed",
            "number of failed indexing ops",
        ),
        (
            "indexing.index_failed_due_to_version_conflict",
            "iifvc,indexingIndexFailedDueToVersionConflict",
            "number of failed indexing ops due to version conflict",
        ),
        (
            "merges.current",
            "mc,mergesCurrent",
            "number of current merges",
        ),
        (
            "merges.current_docs",
            "mcd,mergesCurrentDocs",
            "number of current merging docs",
        ),
        (
            "merges.current_size",
            "mcs,mergesCurrentSize",
            "size of current merges",
        ),
        (
            "merges.total",
            "mt,mergesTotal",
            "number of completed merge ops",
        ),
        ("merges.total_docs", "mtd,mergesTotalDocs", "docs merged"),
        ("merges.total_size", "mts,mergesTotalSize", "size merged"),
        (
            "merges.total_time",
            "mtt,mergesTotalTime",
            "time spent in merges",
        ),
        ("refresh.total", "rto,refreshTotal", "total refreshes"),
        ("refresh.time", "rti,refreshTime", "time spent in refreshes"),
        (
            "refresh.external_total",
            "rto,refreshTotal",
            "total external refreshes",
        ),
        (
            "refresh.external_time",
            "rti,refreshTime",
            "time spent in external refreshes",
        ),
        (
            "refresh.listeners",
            "rli,refreshListeners",
            "number of pending refresh listeners",
        ),
        (
            "search.fetch_current",
            "sfc,searchFetchCurrent",
            "current fetch phase ops",
        ),
        (
            "search.fetch_time",
            "sfti,searchFetchTime",
            "time spent in fetch phase",
        ),
        (
            "search.fetch_total",
            "sfto,searchFetchTotal",
            "total fetch ops",
        ),
        (
            "search.open_contexts",
            "so,searchOpenContexts",
            "open search contexts",
        ),
        (
            "search.query_current",
            "sqc,searchQueryCurrent",
            "current query phase ops",
        ),
        (
            "search.query_time",
            "sqti,searchQueryTime",
            "time spent in query phase",
        ),
        (
            "search.query_total",
            "sqto,searchQueryTotal",
            "total query phase ops",
        ),
        (
            "search.scroll_current",
            "scc,searchScrollCurrent",
            "open scroll contexts",
        ),
        (
            "search.scroll_time",
            "scti,searchScrollTime",
            "time scroll contexts held open",
        ),
        (
            "search.scroll_total",
            "scto,searchScrollTotal",
            "completed scroll contexts",
        ),
        ("segments.count", "sc,segmentsCount", "number of segments"),
        (
            "segments.memory",
            "sm,segmentsMemory",
            "memory used by segments",
        ),
        (
            "segments.index_writer_memory",
            "siwm,segmentsIndexWriterMemory",
            "memory used by index writer",
        ),
        (
            "segments.version_map_memory",
            "svmm,segmentsVersionMapMemory",
            "memory used by version map",
        ),
        (
            "segments.fixed_bitset_memory",
            "sfbm,fixedBitsetMemory",
            "memory used by fixed bit sets for nested object field types and type filters for types referred in _parent fields",
        ),
        ("seq_no.max", "sqm,maxSeqNo", "max sequence number"),
        (
            "seq_no.local_checkpoint",
            "sql,localCheckpoint",
            "local checkpoint",
        ),
        (
            "seq_no.global_checkpoint",
            "sqg,globalCheckpoint",
            "global checkpoint",
        ),
        ("warmer.current", "wc,warmerCurrent", "current warmer ops"),
        ("warmer.total", "wto,warmerTotal", "total warmer ops"),
        (
            "warmer.total_time",
            "wtt,warmerTotalTime",
            "time spent in warmers",
        ),
        ("path.data", "pd,dataPath", "shard data path"),
        ("path.state", "ps,statsPath", "shard state path"),
        (
            "bulk.total_operations",
            "bto,bulkTotalOperations",
            "number of bulk shard ops",
        ),
        (
            "bulk.total_time",
            "btti,bulkTotalTime",
            "time spend in shard bulk",
        ),
        (
            "bulk.total_size_in_bytes",
            "btsi,bulkTotalSizeInBytes",
            "total size in bytes of shard bulk",
        ),
        (
            "bulk.avg_time",
            "bati,bulkAvgTime",
            "average time spend in shard bulk",
        ),
        (
            "bulk.avg_size_in_bytes",
            "basi,bulkAvgSizeInBytes",
            "avg size in bytes of shard bulk",
        ),
        (
            "dense_vector.value_count",
            "dvc,denseVectorCount",
            "number of indexed dense vectors in shard",
        ),
        (
            "sparse_vector.value_count",
            "svc,sparseVectorCount",
            "number of indexed sparse vectors in shard",
        ),
    ];
    if let Some(unit) = params.get("bytes")
        && !["b", "kb", "mb", "gb", "tb", "pb"].contains(&unit.as_str())
    {
        return Err(AppError::IllegalArgument(format!(
            "failed to parse setting [bytes] with value [{unit}] as a size in bytes"
        )));
    }
    let shared = state.read();
    let expr = path.map_or("_all".to_string(), |Path(p)| p);
    // Elasticsearch lists hidden indices' shards too.
    let opts = aliases::IndicesOptions {
        expand_hidden: true,
        ..aliases::IndicesOptions::READ
    };
    let names: Vec<String> = (aliases::resolve(&*shared, &expr, &opts)?.into_iter())
        .map(|t| t.index)
        .collect();
    let indices = shared.lock(&names);
    // Each primary is followed by its replicas, unassigned on this one node; their empty cells
    // are null in JSON.
    // with `bytes`, sizes are bare numbers of that unit
    let size = if params.contains_key("bytes") {
        "0"
    } else {
        "0b"
    };
    let rows: Vec<Vec<String>> = (names.iter())
        .flat_map(|n| (0..indices[n].number_of_shards()).map(move |i| (n, i)))
        .flat_map(|(n, i)| {
            let shard = indices[n].shards().get(i);
            let docs = shard.map_or(0, |s| s.docs().count());
            let (index_total, delete_total) = shard.map_or((0, 0), |s| s.write_totals());
            let max_seq_no = shard.map_or(-1, |s| s.max_seq_no());
            let primary: Vec<String> = (COLUMNS.iter())
                .map(|(c, _, _)| match *c {
                    "index" => n.to_string(),
                    "shard" => i.to_string(),
                    "prirep" => "p".into(),
                    "state" => "STARTED".into(),
                    "docs" => docs.to_string(),
                    "ip" => "127.0.0.1".into(),
                    "id" => crate::nodes::NODE_ID.into(),
                    "node" => crate::CLUSTER_NAME.into(),
                    "indexing.index_total" => index_total.to_string(),
                    "indexing.delete_total" => delete_total.to_string(),
                    "seq_no.max" | "seq_no.local_checkpoint" | "seq_no.global_checkpoint" => {
                        max_seq_no.to_string()
                    }
                    "unassigned.reason"
                    | "unassigned.at"
                    | "unassigned.for"
                    | "unassigned.details"
                    | "recoverysource.type"
                    | "path.data"
                    | "path.state" => String::new(),
                    c if c.ends_with("_in_bytes") => "0".into(),
                    c if c.contains("time") => "0s".into(),
                    c if c.contains("size") || c.contains("memory") || c == "store" => size.into(),
                    "dataset" => size.into(),
                    _ => "0".into(),
                })
                .collect();
            let created = indices[n]
                .settings
                .get(es_index::index_settings::CREATION_DATE)
                .and_then(|d| d.parse::<i64>().ok())
                .map_or(String::new(), es_index::iso_date);
            let replica: Vec<String> = (COLUMNS.iter())
                .map(|(c, _, _)| match *c {
                    "index" => n.to_string(),
                    "shard" => i.to_string(),
                    "prirep" => "r".into(),
                    "state" => "UNASSIGNED".into(),
                    "unassigned.reason" => "INDEX_CREATED".into(),
                    "unassigned.at" => created.clone(),
                    "recoverysource.type" => "peer".into(),
                    _ => String::new(),
                })
                .collect();
            let replicas = indices[n].number_of_replicas();
            std::iter::once(primary)
                .chain(std::iter::repeat_n(replica, replicas))
                .collect::<Vec<_>>()
        })
        .collect();
    // docs through the last numeric column align right, as RestShardsAction declares
    let right = |c: usize| {
        (4..=6).contains(&c)
            || (COLUMNS[c].0.contains('.')
                && !COLUMNS[c].0.starts_with("unassigned.")
                && !COLUMNS[c].0.starts_with("path.")
                && COLUMNS[c].0 != "recoverysource.type")
    };
    // every column but the node id is shown by default
    let default = |c: usize| c <= 9 && COLUMNS[c].0 != "id";
    Ok(cat_view(&COLUMNS, rows, right, default, &params))
}

/// [`cat_table`] after the `help`, `h` (columns by name or alias, unknown ones ignored) and `s`
/// (`col[:asc|:desc]`, numeric when both cells are numbers) parameters, for `columns` given as
/// (name, comma-separated aliases, description).
pub(crate) fn cat_view(
    columns: &[(&str, &str, &str)],
    mut rows: Vec<Vec<String>>,
    right: impl Fn(usize) -> bool,
    default: impl Fn(usize) -> bool,
    params: &HashMap<String, String>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if params.get("help").is_some_and(|v| v != "false") {
        let w = |f: fn(&(&str, &str, &str)) -> usize| columns.iter().map(f).max().unwrap_or(0);
        let (nw, aw, dw) = (w(|c| c.0.len()), w(|c| c.1.len()), w(|c| c.2.len()));
        let out: String = (columns.iter())
            .map(|(n, a, d)| format!("{n:<nw$} | {a:<aw$} | {d:<dw$}\n"))
            .collect();
        return ([("content-type", "text/plain; charset=UTF-8")], out).into_response();
    }
    let find = |name: &str| {
        let name = name.trim();
        columns
            .iter()
            .position(|(n, a, _)| *n == name || a.split(',').any(|a| a == name))
    };
    for key in (params.get("s").into_iter().flat_map(|s| s.split(',')))
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
    {
        let (name, desc) = key
            .strip_suffix(":desc")
            .map_or((key, false), |n| (n, true));
        let Some(c) = find(name.strip_suffix(":asc").unwrap_or(name)) else {
            continue;
        };
        rows.sort_by(|a, b| {
            let ord = match (a[c].parse::<f64>(), b[c].parse::<f64>()) {
                (Ok(x), Ok(y)) => x.total_cmp(&y),
                _ => a[c].cmp(&b[c]),
            };
            if desc { ord.reverse() } else { ord }
        });
    }
    let shown: Vec<usize> = match params.get("h") {
        Some(h) => (h.split(','))
            .flat_map(|h| match h.contains('*') {
                true => (0..columns.len())
                    .filter(|&c| aliases::glob(h.trim(), columns[c].0))
                    .collect(),
                false => find(h).into_iter().collect::<Vec<_>>(),
            })
            .collect(),
        None => (0..columns.len()).filter(|&c| default(c)).collect(),
    };
    let names: Vec<&str> = shown.iter().map(|&c| columns[c].0).collect();
    let rows = (rows.into_iter())
        .map(|r| shown.iter().map(|&c| r[c].clone()).collect())
        .collect();
    cat_table(&names, rows, |c| right(shown[c]), params)
}

/// `GET /_cat/indices[/{index}]`: one row per index (see [`cat_table`]).
// ponytail: only the default columns; no h, s, bytes, help or health/pri filters. Store sizes
// are 0b like _stats, and closed indices still show their doc counts.
pub async fn cat_indices(
    State(state): State<AppState>,
    path: Option<Path<String>>,
    uri: Uri,
    Query(params): Query<HashMap<String, String>>,
) -> Result<axum::response::Response, AppError> {
    check(&uri, &params, &["format", "v"])?;
    const COLUMNS: [&str; 11] = [
        "health",
        "status",
        "index",
        "uuid",
        "pri",
        "rep",
        "docs.count",
        "docs.deleted",
        "store.size",
        "pri.store.size",
        "dataset.size",
    ];
    let shared = state.read();
    let expr = path.map_or("_all".to_string(), |Path(p)| p);
    let names = read_indices(&*shared, &expr, &params)?;
    let indices = shared.lock(&names);
    let rows = (names.iter())
        .map(|n| {
            let ix = &indices[n];
            let replicas = ix.number_of_replicas();
            [
                crate::status(ix.number_of_shards() * replicas),
                if ix.closed { "close" } else { "open" },
                n,
                ix.settings.get(index_settings::UUID).unwrap_or(""),
                &ix.number_of_shards().to_string(),
                &replicas.to_string(),
                &ix.docs().count().to_string(),
                "0",
                "0b",
                "0b",
                "0b",
            ]
            .map(str::to_string)
            .to_vec()
        })
        .collect();
    // pri through dataset.size align right, as RestIndicesAction declares
    Ok(cat_table(&COLUMNS, rows, |c| c >= 4, &params))
}

/// A `_cat` response: `rows` as JSON objects (empty cells null) with `format=json`, else as an
/// aligned text table (with a header row if `v`), its `right` columns aligned right.
pub(crate) fn cat_table(
    columns: &[&str],
    rows: Vec<Vec<String>>,
    right: impl Fn(usize) -> bool,
    params: &HashMap<String, String>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if params.get("format").map(String::as_str) == Some("json") {
        let rows: Vec<Map<String, Value>> = (rows.into_iter())
            .map(|r| {
                (columns.iter())
                    .map(|c| c.to_string())
                    .zip(r.into_iter().map(|c| (!c.is_empty()).then_some(c).into()))
                    .collect()
            })
            .collect();
        return Json(rows).into_response();
    }
    let header = params.get("v").is_some_and(|v| v != "false");
    let table: Vec<Vec<String>> = (header
        .then(|| columns.iter().map(|c| c.to_string()).collect())
        .into_iter())
    .chain(rows)
    .collect();
    let width = |c: usize| table.iter().map(|r| r[c].len()).max().unwrap_or(0);
    let last = columns.len() - 1;
    let mut out = String::new();
    for row in &table {
        for (c, cell) in row.iter().enumerate() {
            let w = width(c);
            match (right(c), c == last) {
                (true, true) => out += &format!("{cell:>w$}"),
                (true, false) => out += &format!("{cell:>w$} "),
                (false, true) => out += cell,
                (false, false) => out += &format!("{cell:<w$} "),
            }
        }
        out.push('\n');
    }
    ([("content-type", "text/plain; charset=UTF-8")], out).into_response()
}
