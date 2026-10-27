use crate::{AppState, error::AppError, params};
use axum::{
    Json,
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

/// Where (1-based line, and column in bytes, as Jackson counts) the first value equal to `v`
/// under the dotted key `path` starts in the JSON document `raw`.
pub(crate) fn locate(raw: &[u8], path: &str, v: &Value) -> Option<(usize, usize)> {
    fn skip_ws(raw: &[u8], at: &mut usize) {
        while raw.get(*at).is_some_and(u8::is_ascii_whitespace) {
            *at += 1;
        }
    }
    /// One past the string starting at `start`.
    fn string_end(raw: &[u8], start: usize) -> usize {
        let mut at = start + 1;
        while at < raw.len() && raw[at] != b'"' {
            at += if raw[at] == b'\\' { 2 } else { 1 };
        }
        at + 1
    }
    /// Walks the value at `at`, found under `here`, to the end of it.
    fn walk(raw: &[u8], at: &mut usize, here: &str, path: &str, v: &Value) -> Option<usize> {
        skip_ws(raw, at);
        let start = *at;
        match *raw.get(start)? {
            open @ (b'{' | b'[') => {
                *at += 1;
                while *at < raw.len() {
                    skip_ws(raw, at);
                    if matches!(raw.get(*at), Some(b'}' | b']')) {
                        *at += 1;
                        return None;
                    }
                    let child = match open {
                        b'{' => {
                            let end = string_end(raw, *at);
                            let key: String = serde_json::from_slice(raw.get(*at..end)?).ok()?;
                            *at = end;
                            skip_ws(raw, at);
                            *at += 1; // the colon
                            match here {
                                "" => key,
                                _ => format!("{here}.{key}"),
                            }
                        }
                        _ => here.to_owned(),
                    };
                    if let Some(found) = walk(raw, at, &child, path, v) {
                        return Some(found);
                    }
                    skip_ws(raw, at);
                    if raw.get(*at) == Some(&b',') {
                        *at += 1;
                    }
                }
                return None;
            }
            b'"' => *at = string_end(raw, start),
            _ => {
                while raw.get(*at).is_some_and(|b| !b",]} \t\r\n".contains(b)) {
                    *at += 1;
                }
            }
        }
        let value = serde_json::from_slice::<Value>(raw.get(start..*at)?).ok();
        (here == path && value.as_ref() == Some(v)).then_some(start)
    }
    let start = walk(raw, &mut 0, "", path, v)?;
    let line_start = raw[..start]
        .iter()
        .rposition(|b| *b == b'\n')
        .map_or(0, |i| i + 1);
    let line = 1 + raw[..start].iter().filter(|b| **b == b'\n').count();
    Some((line, start - line_start + 1))
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
    if let Some(mut index) = shared.write(&target) {
        return Ok(apply(&mut index, target, f));
    }
    drop(shared);
    let mut indices = state.write();
    // Resolved again: another request may have created the index meanwhile.
    let target = aliases::resolve_write(&*indices, name)?;
    if !indices.contains_key(&target) {
        crate::indices::validate_index_name(&target)?;
    }
    let index = indices.entry(target.clone()).or_insert_with(|| {
        let mut created = Index::default();
        created.settings.record_creation(&target);
        created
    });
    Ok(apply(index, target, f))
}

/// `f`'s result on `index`, with its shard total and name.
fn apply<T>(index: &mut Index, name: String, f: impl FnOnce(&mut Index) -> T) -> (T, u64, String) {
    let replicas = index
        .settings
        .get(es_index::index_settings::NUMBER_OF_REPLICAS)
        .and_then(|r| r.parse().ok())
        .unwrap_or(0);
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
    id: &str,
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
                let at = locate(raw, name, v).map_or(String::new(), |(l, c)| format!("[{l}:{c}] "));
                let error = AppError::DocumentParsing(format!(
                    "{at}failed to parse field [{name}] of type [{}] in document with id '{id}'. \
                     Preview of field's value: '{text}'",
                    m.type_name(),
                ));
                let mut cause = json!({
                    "type": "illegal_argument_exception",
                    "reason": format!("failed to parse date field [{text}] with format [{format}]"),
                });
                // ponytail: a one-pattern format's date_time_parse_exception cause (naming where
                // the text stopped parsing) is left out
                if format.contains("||") {
                    cause["caused_by"] = json!({
                        "type": "date_time_parse_exception",
                        "reason": "Failed to parse with all enclosed parsers",
                    });
                }
                return Err(AppError::Caused(Box::new(error), cause));
            }
        }
        // only once the document is accepted; dynamic dates never fail the check above
        i.map_dynamic(id, &source)
            .map_err(AppError::DocumentParsing)?;
        Ok(match create {
            true => i.create_auto(id, source, auto_id_timestamp),
            false => Ok(i.put_auto(id, source, auto_id_timestamp)),
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
        put_or_create(&state, &index, &id, parse_source(&body)?, &body, create, -1)
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
        put_or_create(&state, &index, &id, parse_source(&body)?, &body, true, ts)
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
        put_or_create(&state, &index, &id, parse_source(&body)?, &body, true, -1)
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
    path: Path<(String, String)>,
    uri: Uri,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<Value>, AppError> {
    if params.get("_source").is_some_and(|v| v == "false") {
        return Err(AppError::ActionRequestValidation(
            "fetching source can not be disabled".into(),
        ));
    }
    let (_, Json(mut r)) = get(state, path, uri, Query(params)).await?;
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
    Path((index, id)): Path<(String, String)>,
    uri: Uri,
    Query(params): Query<HashMap<String, String>>,
) -> Reply {
    params::check(&uri, &params, &GET_PARAMS)?;
    let shared = state.read();
    let index = single_index(&*shared, &index)?;
    let i = shared.read(&index).expect("resolved");
    // A non-realtime get sees what search sees, unless it asks for a refresh first.
    let realtime = params.get("realtime").is_none_or(|v| v != "false")
        || params.get("refresh").is_some_and(|v| v != "false");
    let found = i.get(&id).filter(|d| realtime || i.searchable(d.seq_no));
    Ok(match found {
        Some(d) => {
            let mut r = json!({
                "_index": index, "_id": id, "_version": d.version, "_seq_no": d.seq_no,
                "_primary_term": 1, "found": true
            });
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
    uri: Uri,
    Query(params): Query<HashMap<String, String>>,
    body: Bytes,
) -> Reply {
    let default_index = path.map(|Path(i)| i);
    let body = parse_json(&body)?;
    let missing = |what: &str| AppError::ActionRequestValidation(format!("{what} is missing"));
    let items: Vec<(Option<String>, String)> = if let Some(docs) = body["docs"].as_array() {
        let mut v = Vec::new();
        for d in docs {
            let id = d["_id"].as_str().ok_or_else(|| missing("id"))?;
            v.push((d["_index"].as_str().map(String::from), id.to_string()));
        }
        v
    } else if let Some(ids) = body["ids"].as_array() {
        ids.iter()
            .map(|i| (None, i.as_str().unwrap_or_default().to_string()))
            .collect()
    } else {
        return Err(AppError::ActionRequestValidation(
            "no documents to get".into(),
        ));
    };
    let mut docs = Vec::new();
    for (index, id) in items {
        let Some(index) = index.or_else(|| default_index.clone()) else {
            return Err(missing("index"));
        };
        let r = get(
            State(state.clone()),
            Path((index.clone(), id.clone())),
            uri.clone(),
            Query(params.clone()),
        )
        .await;
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
            .map_err(AppError::DocumentParsing)
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
