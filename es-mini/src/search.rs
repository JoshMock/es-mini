use crate::{AppState, error::AppError, indices::parse_body, params, scroll::Contexts};
use axum::{
    Extension, Json,
    body::Bytes,
    extract::{Path, Query, State},
    http::Uri,
};
use es_index::{Indices, aliases};
use es_query::{
    SearchRequest,
    esql::{self, EsqlError},
    parse_query,
};
use serde_json::{Value, json};
use std::{collections::HashMap, time::Instant};

pub(crate) fn targets<'a>(
    indices: &'a Indices,
    resolved: &'a [aliases::Target],
) -> Vec<es_query::Target<'a>> {
    resolved
        .iter()
        .map(|t| (t.index.as_str(), &indices[&t.index], t.filter.as_ref()))
        .collect()
}

const SEARCH_PARAMS: [&str; 46] = [
    "_slice",
    "_source",
    "_source_exclude_vectors",
    "_source_excludes",
    "_source_includes",
    "allow_no_indices",
    "allow_partial_search_results",
    "analyze_wildcard",
    "analyzer",
    "batched_reduce_size",
    "ccs_minimize_roundtrips",
    "default_operator",
    "df",
    "docvalue_fields",
    "expand_wildcards",
    "explain",
    "force_synthetic_source",
    "from",
    "ignore_throttled",
    "ignore_unavailable",
    "include_named_queries_score",
    "lenient",
    "max_concurrent_shard_requests",
    "pre_filter_shard_size",
    "preference",
    "q",
    "request_cache",
    "rest_total_hits_as_int",
    "routing",
    "scroll",
    "search_type",
    "seq_no_primary_term",
    "size",
    "sort",
    "stats",
    "stored_fields",
    "suggest_field",
    "suggest_mode",
    "suggest_size",
    "suggest_text",
    "terminate_after",
    "timeout",
    "track_scores",
    "track_total_hits",
    "typed_keys",
    "version",
];

const COUNT_PARAMS: [&str; 15] = [
    "allow_no_indices",
    "analyze_wildcard",
    "analyzer",
    "default_operator",
    "df",
    "expand_wildcards",
    "ignore_throttled",
    "ignore_unavailable",
    "lenient",
    "min_score",
    "preference",
    "q",
    "routing",
    "stats",
    "terminate_after",
];

// ponytail: every index has exactly one shard, so total is the number of targets
pub(crate) fn shards(n: usize) -> Value {
    json!({"total": n, "successful": n, "skipped": 0, "failed": 0})
}

/// The `profile` section: one shard per target, ordered by id like ES. ponytail: only
/// aggregations are profiled; there is no `searches` (query) or `fetch` profile.
fn profile(targets: &[es_query::Target], aggs: Vec<Value>) -> Value {
    let mut shards: Vec<Value> = (targets.iter().zip(aggs))
        .map(|((index, ..), aggs)| {
            json!({
                "id": format!("[{}][{index}][0]", crate::nodes::NODE_ID),
                "node_id": crate::nodes::NODE_ID,
                "shard_id": 0,
                "index": index,
                "cluster": "(local)",
                "aggregations": aggs,
            })
        })
        .collect();
    shards.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    json!({"shards": shards})
}

/// Replaces each terms lookup (`"terms": {field: {"index", "id", "path"}}`) in a request body
/// with the values at `path` in that document (none if it is missing), as Elasticsearch fetches
/// them before searching.
fn terms_lookups(indices: &Indices, v: &mut Value) -> Result<(), AppError> {
    match v {
        Value::Array(a) => a.iter_mut().try_for_each(|x| terms_lookups(indices, x)),
        Value::Object(o) => {
            if let Some(Value::Object(terms)) = o.get_mut("terms") {
                for spec in terms.values_mut() {
                    let Some((Some(index), Some(id), Some(path))) = spec.as_object().map(|l| {
                        let s = |k| l.get(k).and_then(Value::as_str);
                        (s("index"), s("id"), s("path"))
                    }) else {
                        continue;
                    };
                    let index = &indices[&crate::docs::single_index(indices, index)?];
                    *spec = match index.get(id) {
                        Some(d) => es_index::postings::values(&d.source, path)
                            .into_iter()
                            .cloned()
                            .collect(),
                        None => Value::Array(vec![]),
                    };
                }
            }
            o.values_mut().try_for_each(|x| terms_lookups(indices, x))
        }
        _ => Ok(()),
    }
}

pub async fn search_all(
    state: State<AppState>,
    contexts: Extension<Contexts>,
    uri: Uri,
    params: Query<HashMap<String, String>>,
    body: Bytes,
) -> Result<Json<Value>, AppError> {
    search(state, contexts, Path("_all".into()), uri, params, body).await
}

/// The `q`/`df`/`default_operator` URI query as a parsed query.
pub(crate) fn query_from_params(q: &str, params: &HashMap<String, String>) -> es_query::Query {
    es_query::parse_query_string(
        q,
        params.get("df").map_or("_all", String::as_str),
        params
            .get("default_operator")
            .is_some_and(|o| o.eq_ignore_ascii_case("and")),
    )
}

pub async fn search(
    State(state): State<AppState>,
    Extension(contexts): Extension<Contexts>,
    Path(name): Path<String>,
    uri: Uri,
    Query(params): Query<HashMap<String, String>>,
    body: Bytes,
) -> Result<Json<Value>, AppError> {
    params::check(&uri, &params, &SEARCH_PARAMS)?;
    let start = Instant::now();
    let mut body = parse_body(&body)?;
    let pit = body.as_object_mut().and_then(|o| o.remove("pit"));
    let pit_id = pit.as_ref().map(|p| p["id"].clone());
    if pit.is_some() && name != "_all" {
        return Err(AppError::ActionRequestValidation(
            "[indices] cannot be used with point in time. Do not specify any index with point in time.".into(),
        ));
    }
    terms_lookups(&state.read().expect("lock poisoned"), &mut body)?;
    let mut req = SearchRequest::parse(&body).map_err(AppError::from)?;
    if let Some(q) = params.get("q") {
        req.query = query_from_params(q, &params);
    }
    req.typed_keys = params.get("typed_keys").is_some_and(|v| v == "true");
    let scroll = params
        .get("scroll")
        .map(|v| crate::scroll::keep_alive(v))
        .transpose()?;
    if scroll.is_some() && req.from > 0 {
        return Err(AppError::ActionRequestValidation(
            "using [from] is not allowed in a scroll context".into(),
        ));
    }
    let snapshot = pit.map(|p| crate::scroll::pit(&contexts, &p)).transpose()?;
    let (indices, resolved);
    let targets = match &snapshot {
        Some(s) => s
            .iter()
            .map(|(n, i, f)| (n.as_str(), i, f.as_ref()))
            .collect(),
        None => {
            indices = state.read().expect("lock poisoned");
            resolved = crate::indices::data_targets(&indices, &name, &params)?;
            targets(&indices, &resolved)
        }
    };
    for (_, index, _) in &targets {
        let max = index
            .settings
            .get("index.max_result_window")
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(10_000);
        if req.from + req.size > max {
            return Err(AppError::IllegalArgument(format!(
                "Result window is too large, from + size must be less than or equal to: [{max}] but was [{}]. See the scroll api for a more efficient way to request large data sets. This limit can be set by changing the [index.max_result_window] index level setting.",
                req.from + req.size
            )));
        }
    }
    es_query::check_sortable(&targets, &req)?;
    let page = req.size;
    if scroll.is_some() {
        req.size = usize::MAX;
    }
    // Like Elasticsearch's IndicesService.canCache; the key is the request after terms lookups.
    // ponytail: only single-index searches are cached; ES caches each shard's result
    let cache = match (
        &targets[..],
        params.get("request_cache").map(String::as_str),
    ) {
        (_, Some("false")) => None,
        _ if scroll.is_some() || snapshot.is_some() || req.profile => None,
        ([(_, index, _)], Some("true")) => Some(index),
        ([(_, index, _)], _) => (page == 0
            && index.settings.get("index.requests.cache.enable") != Some("false"))
        .then_some(index),
        _ => None,
    };
    let respond = || -> Result<Value, AppError> {
        let r = es_query::search_many(&targets, &req)?;
        let hits: Vec<Value> = r
            .hits
            .iter()
            .map(|h| {
                let mut hit =
                    json!({"_index": h.index, "_id": h.id, "_score": h.score, "_source": h.doc.source});
                if !h.sort.is_empty() {
                    hit["sort"] = h.sort.clone().into();
                }
                if !h.matched_queries.is_empty() {
                    hit["matched_queries"] = h.matched_queries.clone().into();
                }
                hit
            })
            .collect();
        let as_int = params
            .get("rest_total_hits_as_int")
            .is_some_and(|v| v == "true");
        let track = params
            .get("track_total_hits")
            .or(req.track_total_hits.as_ref())
            .map(String::as_str);
        let limit = match track {
            Some("true" | "-1") => None,
            None if as_int => None,
            None => Some(Some(10_000)),
            Some("false") => Some(None),
            Some(n) => match n.parse::<u64>() {
                Ok(n) if as_int => {
                    return Err(AppError::IllegalArgument(format!(
                        "[rest_total_hits_as_int] cannot be used if the tracking of total hits is not accurate, got {n}"
                    )));
                }
                Ok(n) => Some(Some(n)),
                Err(_) => {
                    return Err(AppError::IllegalArgument(format!(
                        "[track_total_hits] parameter must be positive or equals to -1, got {n}"
                    )));
                }
            },
        };
        let total = match limit {
            Some(None) if as_int => json!(-1),
            Some(None) => Value::Null,
            Some(Some(n)) if r.total as u64 > n => json!({"value": n, "relation": "gte"}),
            _ if as_int => json!(r.total),
            _ => json!({"value": r.total, "relation": "eq"}),
        };
        let mut hits = json!({"total": total, "max_score": r.max_score, "hits": hits});
        if total.is_null() {
            hits.as_object_mut().unwrap().remove("total");
        }
        let mut resp = json!({
            "took": start.elapsed().as_millis() as u64,
            "timed_out": false,
            "_shards": shards(targets.len()),
            "hits": hits
        });
        if !r.failures.is_empty() {
            let s = &mut resp["_shards"];
            s["successful"] = (targets.len() - r.failures.len()).into();
            s["failed"] = r.failures.len().into();
            let failures = r.failures.iter().map(|(index, e)| {
                let (_, mut reason) = AppError::from(e.clone()).parts();
                reason.as_object_mut().unwrap().remove("root_cause");
                json!({"shard": 0, "index": index, "node": crate::nodes::NODE_ID, "reason": reason})
            });
            s["failures"] = failures.collect();
        }
        if let Some(a) = r.aggregations {
            resp["aggregations"] = a;
        }
        if let Some(p) = r.profile {
            let searched: Vec<_> = (targets.iter())
                .filter(|(n, ..)| !r.failures.iter().any(|(f, _)| f == n))
                .cloned()
                .collect();
            resp["profile"] = profile(&searched, p);
        }
        Ok(resp)
    };
    let mut resp = match cache {
        Some(index) => {
            let mut key: Vec<_> = (params.iter())
                .filter(|(k, _)| !matches!(k.as_str(), "request_cache" | "preference"))
                .collect();
            key.sort();
            index.cached(format!("{body}{key:?}{:?}", targets[0].2), respond)?
        }
        None => respond()?,
    };
    resp["took"] = (start.elapsed().as_millis() as u64).into();
    if let Some(p) = &pit_id {
        resp["pit_id"] = p.clone();
    }
    Ok(Json(match scroll {
        Some(keep) => crate::scroll::open(&contexts, resp, page, keep),
        None => resp,
    }))
}

pub async fn search_template_all(
    state: State<AppState>,
    contexts: Extension<Contexts>,
    uri: Uri,
    params: Query<HashMap<String, String>>,
    body: Bytes,
) -> Result<Json<Value>, AppError> {
    search_template(state, contexts, Path("_all".into()), uri, params, body).await
}

/// `_search/template`: renders the inline template, then runs it as a `_search`.
pub async fn search_template(
    state: State<AppState>,
    contexts: Extension<Contexts>,
    name: Path<String>,
    uri: Uri,
    params: Query<HashMap<String, String>>,
    body: Bytes,
) -> Result<Json<Value>, AppError> {
    let body = crate::template::search_body(&body)?;
    search(state, contexts, name, uri, params, body).await
}

pub async fn msearch(
    state: State<AppState>,
    contexts: Extension<Contexts>,
    path: Option<Path<String>>,
    uri: Uri,
    body: Bytes,
) -> Result<Json<Value>, AppError> {
    msearch_impl(state, contexts, path, uri, body, false).await
}

pub async fn msearch_template(
    state: State<AppState>,
    contexts: Extension<Contexts>,
    path: Option<Path<String>>,
    uri: Uri,
    body: Bytes,
) -> Result<Json<Value>, AppError> {
    msearch_impl(state, contexts, path, uri, body, true).await
}

/// `_msearch`: NDJSON header/body pairs, each run as a `_search`; a failing search fails only its own entry.
/// With `template`, each body is a search template rendered first.
async fn msearch_impl(
    State(state): State<AppState>,
    Extension(contexts): Extension<Contexts>,
    path: Option<Path<String>>,
    uri: Uri,
    body: Bytes,
    template: bool,
) -> Result<Json<Value>, AppError> {
    let start = Instant::now();
    let default = path.map_or("_all".to_string(), |Path(p)| p);
    let text = std::str::from_utf8(&body)
        .map_err(|e| AppError::IllegalArgument(format!("request body is not UTF-8: {e}")))?;
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    let mut responses = Vec::new();
    while let Some(header) = lines.next() {
        let item = async {
            let mut header = parse_body(&Bytes::copy_from_slice(header.as_bytes()))?;
            let search_body = lines.next().ok_or_else(|| {
                AppError::IllegalArgument("msearch request must have a body for each header".into())
            })?;
            let mut name = default.clone();
            let mut params = HashMap::new();
            for (k, v) in header.as_object_mut().into_iter().flatten() {
                let v = match v {
                    Value::String(s) => s.clone(),
                    Value::Array(a) => a
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(","),
                    other => other.to_string(),
                };
                match k.as_str() {
                    "index" => name = v,
                    _ => drop(params.insert(k.clone(), v)),
                }
            }
            let mut search_body = Bytes::copy_from_slice(search_body.as_bytes());
            if template {
                search_body = crate::template::search_body(&search_body)?;
            }
            search(
                State(state.clone()),
                Extension(contexts.clone()),
                Path(name),
                uri.clone(),
                Query(params),
                search_body,
            )
            .await
        }
        .await;
        responses.push(match item {
            Ok(Json(mut v)) => {
                v["status"] = 200.into();
                v
            }
            Err(e) => {
                let (status, error) = e.parts();
                json!({"error": error, "status": status.as_u16()})
            }
        });
    }
    if responses.is_empty() {
        return Err(AppError::ActionRequestValidation(
            "no requests added".into(),
        ));
    }
    Ok(Json(
        json!({"took": start.elapsed().as_millis() as u64, "responses": responses}),
    ))
}

pub async fn count_all(
    state: State<AppState>,
    uri: Uri,
    params: Query<HashMap<String, String>>,
    body: Bytes,
) -> Result<Json<Value>, AppError> {
    count(state, Path("_all".into()), uri, params, body).await
}

pub async fn count(
    State(state): State<AppState>,
    Path(name): Path<String>,
    uri: Uri,
    Query(params): Query<HashMap<String, String>>,
    body: Bytes,
) -> Result<Json<Value>, AppError> {
    params::check(&uri, &params, &COUNT_PARAMS)?;
    if params
        .get("terminate_after")
        .and_then(|v| v.parse::<i64>().ok())
        .is_some_and(|n| n < 0)
    {
        return Err(AppError::IllegalArgument(
            "terminateAfter must be > 0".into(),
        ));
    }
    let mut body = parse_body(&body)?;
    if let Some(k) = body
        .as_object()
        .and_then(|o| o.keys().find(|k| *k != "query"))
    {
        return Err(AppError::ParsingException(format!(
            "request does not support [{k}]"
        )));
    }
    let indices = state.read().expect("lock poisoned");
    terms_lookups(&indices, &mut body)?;
    let query = match params.get("q") {
        Some(q) => query_from_params(q, &params),
        None => body
            .get("query")
            .map_or(Ok(es_query::Query::MatchAll), parse_query)?,
    };
    let resolved = crate::indices::data_targets(&indices, &name, &params)?;
    let n = es_query::count_many(&targets(&indices, &resolved), &query)?;
    let resp = json!({"count": n, "_shards": shards(resolved.len())});
    Ok(Json(resp))
}

pub async fn esql(
    State(state): State<AppState>,
    uri: Uri,
    Query(params): Query<HashMap<String, String>>,
    body: Bytes,
) -> Result<Json<Value>, AppError> {
    params::check(
        &uri,
        &params,
        &["delimiter", "drop_null_columns", "allow_partial_results"],
    )?;
    let start = Instant::now();
    let body = parse_body(&body)?;
    let query = body["query"]
        .as_str()
        .ok_or_else(|| AppError::ParsingException("[query] is required".into()))?;
    let r = esql::run(&state.read().expect("lock poisoned"), query).map_err(|e| match e {
        EsqlError::Parse(m) => AppError::ParsingException(m),
        EsqlError::Verification(m) => AppError::Verification(m),
        EsqlError::IndexClosed(i) => AppError::IndexClosed(i),
    })?;
    Ok(Json(json!({
        "took": start.elapsed().as_millis() as u64,
        "is_partial": false,
        "columns": r.columns,
        "values": r.values,
        "documents_found": r.documents_found,
        "values_loaded": r.values_loaded,
    })))
}
