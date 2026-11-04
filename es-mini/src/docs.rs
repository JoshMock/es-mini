use crate::{
    AppState,
    error::AppError,
    params,
    security::{Authn, mget_shard},
};
use axum::{
    Extension, Json,
    body::Bytes,
    extract::{Path, Query, State},
    http::{StatusCode, Uri},
};
use es_index::{DocResult, FieldMapping, Index, WriteResult, aliases, auto_id};
use serde_json::{Value, json};
use std::collections::HashMap;

type Reply = Result<(StatusCode, Json<Value>), AppError>;

pub fn parse_json(body: &[u8]) -> Result<Value, AppError> {
    if body.is_empty() {
        return Err(AppError::Parse("request body is required".into()));
    }
    serde_json::from_slice(body).map_err(|e| AppError::Parse(e.to_string()))
}

/// A document source: malformed JSON is a `document_parsing_exception`, an empty body is required.
fn parse_source(body: &[u8]) -> Result<Value, AppError> {
    match body.is_empty() {
        true => parse_json(body),
        false => serde_json::from_slice(body)
            .map_err(|e| AppError::DocumentParsing(format!("failed to parse: {e}"))),
    }
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

/// Runs `f` against the write target `name` resolves to (an index, created if missing as ES
/// auto-create does, or an alias's write index). Returns the result, shard total, and index name.
fn write<T>(
    state: &AppState,
    name: &str,
    f: impl FnOnce(&mut Index) -> T,
) -> Result<(T, u64, String), AppError> {
    let shared = state.read();
    // A write to an existing index locks only that index; auto-creating one locks them all.
    let target = aliases::resolve_write(&*shared, name)?;
    let exists = shared.contains_key(&target)
        || [crate::security::INDEX, crate::security::ALIAS].contains(&&*target);
    let (action, privileges) = match exists {
        true => (
            "indices:data/write/bulk[s]",
            "create_doc,create,delete,index,write,all",
        ),
        false => (
            "indices:admin/auto_create",
            "auto_configure,create_index,manage,all",
        ),
    };
    crate::security::refuse_restricted(state, &target, action, privileges)?;
    if let Some(mut index) = shared.write(&target) {
        return Ok(apply(&mut index, target, f));
    }
    drop(shared);
    let mut indices = state.write();
    // Resolved again: another request may have created the index meanwhile.
    let mut target = aliases::resolve_write(&*indices, name)?;
    if !indices.contains_key(&target) {
        crate::indices::validate_index_name(&target)?;
        if let Some(index) = crate::security::managed_index(&target) {
            target = crate::security::INDEX.to_owned();
            indices.entry(target.clone()).or_insert(index);
        } else {
            let mut created = Index::default();
            created.settings.record_creation(&target);
            crate::system::hide(&mut created, &target)?;
            indices.insert(target.clone(), created);
        }
    }
    let index = indices.get_mut(&target).expect("created above");
    Ok(apply(index, target, f))
}

/// [`write`] of document `id` sent to `name` with `routing`, which `f` gets as [`routing`]
/// resolves it.
fn write_doc<T>(
    state: &AppState,
    name: &str,
    (id, routing): (&str, Option<&str>),
    f: impl FnOnce(&mut Index, Option<&str>) -> T,
) -> Result<(T, u64, String), AppError> {
    let (r, total, index) = write(state, name, |i| {
        let Some(routing) = self::routing(i, name, routing)? else {
            return Ok::<_, AppError>(None);
        };
        let routing = routing.map(str::to_owned);
        Ok(Some(f(i, routing.as_deref())))
    })?;
    match r? {
        Some(r) => Ok((r, total, index)),
        None => Err(routing_missing(index, id)),
    }
}

/// The routing of a document request with `routing` sent to `name` that reached index `i`:
/// that of the alias `name` if it has one. `Ok(None)` if the request lacks the routing `i`'s
/// mapping requires.
fn routing<'a>(
    i: &'a Index,
    name: &str,
    routing: Option<&'a str>,
) -> Result<Option<Option<&'a str>>, AppError> {
    let routing = match i.aliases.get(name) {
        Some(alias) => alias
            .index_routing(name, routing)
            .map_err(AppError::IllegalArgument)?,
        None => routing,
    };
    Ok((routing.is_some() || !i.mappings.routing_required()).then_some(routing))
}

fn routing_missing(index: String, id: &str) -> AppError {
    AppError::RoutingMissing {
        index,
        id: id.to_owned(),
    }
}

/// `f`'s result on `index`, with its shard total and name.
fn apply<T>(index: &mut Index, name: String, f: impl FnOnce(&mut Index) -> T) -> (T, u64, String) {
    let replicas = index.number_of_replicas() as u64;
    (f(index), 1 + replicas, name)
}

/// The one concrete index a single-document read targets.
pub(crate) fn single_index(
    indices: &es_index::aliases::Map<impl aliases::IndexMeta>,
    name: &str,
) -> Result<String, AppError> {
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
    (id, routing): (&str, Option<&str>),
    source: Value,
    raw: &[u8],
    create: bool,
    auto_id_timestamp: i64,
) -> Reply {
    if id.len() > 512 {
        return Err(AppError::IllegalArgument(format!(
            "id [{id}] is too long, must be no longer than 512 bytes but was: {}",
            id.len()
        )));
    }
    let source = check_source(source)?;
    let (w, total, index) = write_doc(state, index, (id, routing), |i, routing| {
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
        for (name, m) in &i.mappings.properties {
            let (format, locale, default) = match m {
                FieldMapping::Date { format, locale, .. } => {
                    (format, locale, "strict_date_optional_time||epoch_millis")
                }
                FieldMapping::DateNanos { format, locale } => (
                    format,
                    locale,
                    "strict_date_optional_time_nanos||epoch_millis",
                ),
                _ => continue,
            };
            let (format, locale) = (format.as_deref().unwrap_or(default), locale.as_deref());
            let values = es_index::postings::values(&source, name);
            if let Some(v) = values
                .iter()
                .find(|v| es_query::bad_date(Some(format), locale, v))
            {
                let text = es_index::postings::text(v);
                let at = es_index::mapping::locate(raw, name, v, false)
                    .map_or(String::new(), |(l, c)| format!("[{l}:{c}] "));
                let error = AppError::DocumentParsing(format!(
                    "{at}failed to parse field [{name}] of type [{}] in document with id '{id}'. \
                     Preview of field's value: '{text}'",
                    m.type_name(),
                ));
                let mut cause = json!({
                    "type": "illegal_argument_exception",
                    "reason": format!("failed to parse date field [{text}] with format [{format}]"),
                });
                // ponytail: a value that parses but does not resolve gets this cause, where ES's
                // is a date_time_exception naming the field out of range
                if let Some(reason) = es_index::date_parse_error(format, locale, &text) {
                    cause["caused_by"] = json!({
                        "type": "date_time_parse_exception",
                        "reason": reason,
                    });
                }
                return Err(AppError::Caused(Box::new(error), cause));
            }
        }
        // only once the document is accepted; dynamic dates never fail the check above
        i.map_dynamic_in(id, &source, Some(raw))?;
        Ok(match create {
            true => i.create_auto(id, routing, source, auto_id_timestamp),
            false => Ok(i.put_auto(id, routing, source, auto_id_timestamp)),
        })
    })?;
    let w = w?.map_err(|current_version| AppError::VersionConflict {
        index: index.clone(),
        id: id.to_owned(),
        current_version,
    })?;
    Ok(reply(&index, id, w, total))
}

pub(crate) fn force_refresh(
    state: &AppState,
    params: &HashMap<String, String>,
    index: &str,
    reply: &mut (StatusCode, Json<Value>),
) -> Result<(), AppError> {
    match params.get("refresh").map(String::as_str) {
        Some("" | "true") => {
            write(state, index, Index::refresh)?;
            reply.1.0["forced_refresh"] = true.into();
        }
        // ponytail: refreshes now instead of waiting for the scheduled refresh, which exposes
        // other writes up to one interval early
        Some("wait_for") => {
            write(state, index, Index::refresh)?;
        }
        _ => {}
    }
    Ok(())
}

fn require_alias(
    state: &AppState,
    params: &HashMap<String, String>,
    index: &str,
) -> Result<(), AppError> {
    let flagged = params.get("require_alias").is_some_and(|v| v == "true");
    match flagged && !aliases::is_alias(&*state.read(), index) {
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
        let doc = (id.as_str(), params.get("routing").map(String::as_str));
        put_or_create(&state, &index, doc, parse_source(&body)?, &body, create, -1)
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
        let (id, ts) = auto_id();
        let doc = (id.as_str(), params.get("routing").map(String::as_str));
        put_or_create(&state, &index, doc, parse_source(&body)?, &body, true, ts)
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
        let doc = (id.as_str(), params.get("routing").map(String::as_str));
        put_or_create(&state, &index, doc, parse_source(&body)?, &body, true, -1)
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

/// Applies `_source`, `_source_includes` and `_source_excludes`; `None` means `_source` is omitted.
fn filter_source(source: &Value, params: &HashMap<String, String>) -> Option<Value> {
    let list = |k: &str| params.get(k).filter(|v| !v.is_empty());
    let mut inc = list("_source_includes").cloned();
    match params.get("_source").map(String::as_str) {
        Some("false") => return None,
        Some("true") | Some("") | None => {}
        Some(v) => inc = Some(inc.map_or(v.to_string(), |i| format!("{i},{v}"))),
    }
    let mut filter = inc.unwrap_or_default();
    for e in list("_source_excludes").iter().flat_map(|e| e.split(',')) {
        filter.push_str(&format!(",-{e}"));
    }
    Some(if filter.is_empty() {
        source.clone()
    } else {
        es_index::filter_path::apply(source, &filter)
    })
}

pub async fn get_source(
    state: State<AppState>,
    authn: Option<Extension<Authn>>,
    path: Path<(String, String)>,
    uri: Uri,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<Value>, AppError> {
    if params.get("_source").is_some_and(|v| v == "false") {
        return Err(AppError::ActionRequestValidation(
            "fetching source can not be disabled".into(),
        ));
    }
    let (_, Json(mut r)) = get(state, authn, path, uri, Query(params)).await?;
    let what = match (r.get_mut("_source").map(Value::take), r["found"] == true) {
        (Some(src), _) => return Ok(Json(src)),
        (None, true) => "Source",
        (None, false) => "Document",
    };
    let (index, id) = (r["_index"].as_str(), r["_id"].as_str());
    Err(AppError::ResourceNotFound(format!(
        "{what} not found [{}]/[{}]",
        index.unwrap_or_default(),
        id.unwrap_or_default()
    )))
}

pub async fn get(
    State(state): State<AppState>,
    authn: Option<Extension<Authn>>,
    Path((index, id)): Path<(String, String)>,
    uri: Uri,
    Query(params): Query<HashMap<String, String>>,
) -> Reply {
    params::check(&uri, &params, &GET_PARAMS)?;
    let system = crate::security::restricted_read(&state, authn.as_deref(), &index);
    let shared = state.read();
    let name = index;
    let locked;
    let (index, i): (String, &Index) = match &system {
        Some(i) => (crate::security::INDEX.to_owned(), i),
        None => {
            let index = single_index(&*shared, &name)?;
            locked = shared.read(&index).expect("resolved");
            (index, &locked)
        }
    };
    // A non-realtime get sees what search sees, unless it asks for a refresh first.
    let realtime = params.get("realtime").is_none_or(|v| v != "false")
        || params.get("refresh").is_some_and(|v| v != "false");
    let Some(routing) = routing(i, &name, params.get("routing").map(String::as_str))? else {
        return Err(routing_missing(index, &id));
    };
    let shard = i.shard_of(&id, routing);
    let found = shard
        .and_then(|s| (s.get(&id)).filter(|d| realtime || s.searchable(&i.settings, d.seq_no)));
    Ok(match found {
        Some(d) => {
            let mut r = json!({
                "_index": index, "_id": id, "_version": d.version, "_seq_no": d.seq_no,
                "_primary_term": 1
            });
            if let Some(routing) = &d.routing {
                r["_routing"] = routing.as_str().into();
            }
            r["found"] = true.into();
            if let Some(src) =
                filter_source(&d.source, &params).filter(|_| i.mappings.source_enabled())
            {
                r["_source"] = src;
            }
            (StatusCode::OK, Json(r))
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"_index": index, "_id": id, "found": false})),
        ),
    })
}

/// `_mget`: each requested document is fetched through [`get`]; failures become per-item errors.
pub async fn mget(
    State(state): State<AppState>,
    path: Option<Path<String>>,
    authn: Option<Extension<Authn>>,
    uri: Uri,
    Query(params): Query<HashMap<String, String>>,
    body: Bytes,
) -> Reply {
    let default_index = path.map(|Path(i)| i);
    let body = parse_json(&body)?;
    let missing = |what: &str| AppError::ActionRequestValidation(format!("{what} is missing"));
    let s = |v: &Value| v.as_str().map(String::from);
    let items: Vec<(Option<String>, String, Option<String>)> =
        if let Some(docs) = body["docs"].as_array() {
            let mut v = Vec::new();
            for d in docs {
                let id = d["_id"].as_str().ok_or_else(|| missing("id"))?;
                v.push((s(&d["_index"]), id.to_string(), s(&d["routing"])));
            }
            v
        } else if let Some(ids) = body["ids"].as_array() {
            ids.iter()
                .map(|i| (None, i.as_str().unwrap_or_default().to_string(), None))
                .collect()
        } else {
            return Err(AppError::ActionRequestValidation(
                "no documents to get".into(),
            ));
        };
    let items = (items.into_iter())
        .map(|(index, id, routing)| Some((index.or_else(|| default_index.clone())?, id, routing)))
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| missing("index"))?;
    // Grouped by shard like Elasticsearch's shard requests; items that fail to resolve are in
    // none.
    let concrete: Vec<Option<(String, usize)>> = {
        let shared = state.read();
        (items.iter())
            .map(|(index, id, r)| {
                let c = single_index(&*shared, index)
                    .ok()
                    .filter(|_| !index.contains('*'))?;
                let i = shared.read(&c).expect("resolved");
                let r = routing(&i, index, r.as_deref()).ok()??;
                Some((c.clone(), i.shard_for(id, r)))
            })
            .collect()
    };
    let mut docs = Vec::new();
    for (n, (index, id, routing)) in items.iter().cloned().enumerate() {
        if let Some(c) = &concrete[n] {
            let names: Vec<&str> = (items.iter().zip(&concrete))
                .filter(|(_, other)| other.as_ref() == Some(c))
                .map(|((name, ..), _)| name.as_str())
                .collect();
            if let Err(e) = mget_shard(authn.as_deref(), &names) {
                docs.push(json!({"_index": c.0, "_id": id, "error": e.parts().1}));
                continue;
            }
        }
        let mut params = params.clone();
        params.extend(routing.map(|r| ("routing".to_owned(), r)));
        // Elasticsearch resolves an item's index without expanding wildcards.
        let r = match index.contains('*') {
            true => Err(AppError::IndexNotFound(index.clone())),
            false => {
                let path = Path((index.clone(), id.clone()));
                get(
                    State(state.clone()),
                    authn.clone(),
                    path,
                    uri.clone(),
                    Query(params),
                )
                .await
            }
        };
        docs.push(match r {
            Ok((_, Json(d))) => d,
            Err(e) => {
                let (status, error) = e.parts();
                json!({"_index": index, "_id": id, "error": error, "status": status.as_u16()})
            }
        });
    }
    Ok((StatusCode::OK, Json(json!({"docs": docs}))))
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
    with_write_params(&state, &params, &index, || {
        delete_doc(
            &state,
            &index,
            &id,
            params.get("routing").map(String::as_str),
        )
    })
}

pub fn delete_doc(state: &AppState, index: &str, id: &str, routing: Option<&str>) -> Reply {
    let (w, total, index) = write_doc(state, index, (id, routing), |i, routing| {
        i.delete_routed(id, routing)
    })?;
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
        let routing = params.get("routing").map(String::as_str);
        update_doc(&state, &index, &id, routing, &parse_json(&body)?)
    })
}

pub fn update_doc(
    state: &AppState,
    index: &str,
    id: &str,
    routing: Option<&str>,
    body: &Value,
) -> Reply {
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
    let (w, total, index) = write_doc(state, index, (id, routing), |i, routing| {
        i.update(id, routing, &partial, upsert, detect_noop)
            .map_err(AppError::from)
    })?;
    let w = w?.ok_or_else(|| AppError::DocumentMissing {
        index: index.clone(),
        id: id.to_owned(),
    })?;
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
