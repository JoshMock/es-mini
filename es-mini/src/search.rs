use crate::{
    AppState,
    error::AppError,
    indices::parse_body,
    params,
    scroll::Contexts,
    security::{Authn, esql_from, msearch_item, pit_indices},
};
use axum::{
    Extension, Json,
    body::Bytes,
    extract::{OriginalUri, Path, Query, State},
    http::{StatusCode, Uri, uri::InvalidUri},
};
use es_index::aliases;
use es_query::{
    SearchRequest,
    esql::{self, EsqlError},
    parse_query,
};
use serde_json::{Value, json};
use std::borrow::Borrow;
use std::{collections::HashMap, sync::Arc, time::Instant};

/// The shards of each multi-shard index of `resolved`, copied to be searched one by one (see
/// [`es_index::Index::shard_snapshots`]); none for an index of one shard, searched in place.
pub(crate) fn split(
    indices: &aliases::Map<impl Borrow<es_index::Index>>,
    resolved: &[aliases::Target],
) -> Vec<Vec<es_index::Index>> {
    (resolved.iter())
        .map(|t| {
            let index = indices[&t.index].borrow();
            match index.number_of_shards() {
                1 => vec![],
                _ => index.shard_snapshots(),
            }
        })
        .collect()
}

/// Whether a search of `t` reads shard `n` of its index's `of`: every shard without routing,
/// else those its routing values route to.
pub(crate) fn routed(t: &aliases::Target, n: usize, of: usize) -> bool {
    let shard = |r: &String| es_index::routing::shard_id("", Some(r), of as u32) as usize;
    (t.routing.as_ref()).is_none_or(|rs| rs.iter().any(|r| shard(r) == n))
}

/// A search target per routed shard of `resolved`, from `split` where it has them, in
/// Elasticsearch's shard order (shard number, then index name), which breaks ties between hits.
/// ponytail: shard numbers in failures and profiles count the shards searched, so a routed
/// search reports its shards from 0; pass shard numbers in es_query::Target if that matters.
pub(crate) fn targets<'a>(
    indices: &'a aliases::Map<impl Borrow<es_index::Index>>,
    resolved: &'a [aliases::Target],
    split: &'a [Vec<es_index::Index>],
) -> Vec<es_query::Target<'a>> {
    let mut targets: Vec<_> = (resolved.iter().zip(split))
        .flat_map(|(t, shards)| {
            let (name, filter) = (t.index.as_str(), t.filter.as_ref());
            match &shards[..] {
                [] => vec![(0, (name, indices[&t.index].borrow(), filter))],
                _ => (shards.iter().enumerate())
                    .filter(|&(n, _)| routed(t, n, shards.len()))
                    .map(|(n, s)| (n, (name, s, filter)))
                    .collect(),
            }
        })
        .collect();
    targets.sort_by_key(|&(n, (name, ..))| (n, name));
    targets.into_iter().map(|(_, t)| t).collect()
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

/// The `_shards` header of a search over `n` shards that all succeeded.
pub(crate) fn shards(n: usize) -> Value {
    json!({"total": n, "successful": n, "skipped": 0, "failed": 0})
}

/// The `profile` section: one entry per searched shard, ordered by id like ES. ponytail: only
/// aggregations are profiled; there is no `searches` (query) or `fetch` profile.
fn profile(searched: Vec<(&str, usize, Value)>) -> Value {
    let mut shards: Vec<Value> = (searched.into_iter())
        .map(|(index, shard, aggs)| {
            json!({
                "id": format!("[{}][{index}][{shard}]", crate::nodes::NODE_ID),
                "node_id": crate::nodes::NODE_ID,
                "shard_id": shard,
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
fn terms_lookups(shared: &crate::store::Shared, v: &mut Value) -> Result<(), AppError> {
    match v {
        Value::Array(a) => a.iter_mut().try_for_each(|x| terms_lookups(shared, x)),
        Value::Object(o) => {
            if let Some(Value::Object(terms)) = o.get_mut("terms") {
                for spec in terms.values_mut() {
                    let Some((Some(index), Some(id), Some(path), routing)) =
                        spec.as_object().map(|l| {
                            let s = |k| l.get(k).and_then(Value::as_str);
                            (s("index"), s("id"), s("path"), s("routing"))
                        })
                    else {
                        continue;
                    };
                    let index = crate::docs::single_index(&**shared, index)?;
                    let index = shared.read(&index).expect("resolved");
                    *spec = match index.shard_of(id, routing).and_then(|s| s.get(id)) {
                        Some(d) => es_index::postings::values(&d.source, path)
                            .into_iter()
                            .cloned()
                            .collect(),
                        None => Value::Array(vec![]),
                    };
                }
            }
            o.values_mut().try_for_each(|x| terms_lookups(shared, x))
        }
        _ => Ok(()),
    }
}

pub async fn search_all(
    state: State<AppState>,
    contexts: Extension<Contexts>,
    authn: Option<Extension<Authn>>,
    uri: Uri,
    params: Query<HashMap<String, String>>,
    body: Bytes,
) -> Result<Json<Value>, AppError> {
    search(
        state,
        contexts,
        authn,
        OriginalUri(uri),
        Path("_all".into()),
        params,
        body,
    )
    .await
}

/// The `q`/`df`/`default_operator` URI query as a parsed query.
pub(crate) fn query_from_params(
    q: &str,
    params: &HashMap<String, String>,
) -> Result<es_query::Query, es_query::QueryError> {
    es_query::parse_query_string(
        q,
        params.get("df").map_or("_all", String::as_str),
        params
            .get("default_operator")
            .is_some_and(|o| o.eq_ignore_ascii_case("and")),
    )
}

const PIT_WITH_INDICES: &str =
    "[indices] cannot be used with point in time. Do not specify any index with point in time.";

pub async fn search(
    State(state): State<AppState>,
    Extension(contexts): Extension<Contexts>,
    authn: Option<Extension<Authn>>,
    // As sent: authorization may have rewritten the path (not the query) to name indices.
    OriginalUri(uri): OriginalUri,
    Path(name): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    body: Bytes,
) -> Result<Json<Value>, AppError> {
    params::check(&uri, &params, &SEARCH_PARAMS)?;
    let start = Instant::now();
    let raw = body.clone();
    let mut body = parse_body(&body)?;
    let pit = body.get("pit").cloned();
    let pit_id = pit.as_ref().map(|p| p["id"].clone());
    // Authorization narrows an absent index expression to the key's indices.
    let first = uri.path().split('/').find(|s| !s.is_empty());
    if pit.is_some() && !matches!(first, Some("_search" | "_all") | None) {
        return Err(AppError::ActionRequestValidation(PIT_WITH_INDICES.into()));
    }
    terms_lookups(&state.read(), &mut body)?;
    let mut req = SearchRequest::parse(&body).map_err(AppError::from)?;
    if let Some(q) = params.get("q") {
        req.query = query_from_params(q, &params)?;
    }
    req.typed_keys = params
        .get("typed_keys")
        .is_some_and(|v| v.is_empty() || v == "true");
    req.dfs = params.get("search_type").map(String::as_str) == Some("dfs_query_then_fetch");
    let scroll = params
        .get("scroll")
        .map(|v| crate::scroll::keep_alive(v))
        .transpose()?;
    if scroll.is_some() && req.from > 0 {
        return Err(AppError::ActionRequestValidation(
            "using [from] is not allowed in a scroll context".into(),
        ));
    }
    let system = crate::security::restricted_read(&state, authn.as_deref(), &name)
        .map(|i| Arc::new(vec![(crate::security::INDEX.to_owned(), i, None)]));
    let snapshot = match pit {
        None if system.is_some() => system,
        Some(p) => {
            let snapshot = crate::scroll::pit(&contexts, &p, &raw)?;
            let names: Vec<&str> = snapshot.iter().map(|(n, ..)| n.as_str()).collect();
            pit_indices(authn.as_deref(), &names)?;
            Some(snapshot)
        }
        None if scroll.is_some() => Some(crate::scroll::snapshot(&state, &name, &params)?),
        None => None,
    };
    let (shared, indices, resolved, split);
    // Like Elasticsearch's IndicesService.canCache, per shard of each index searched.
    // ponytail: each shard caches the merged response rather than its own result, so the
    // counts and invalidation are per shard but a miss on any shard recomputes the search
    let mut cache: Vec<(&es_index::Index, &es_index::Shard)> = vec![];
    let mut uncached = false;
    // the routing of each index searched, which picks its shards
    let mut routing = String::new();
    let flag = params.get("request_cache").map(String::as_str);
    let cacheable = |index: &es_index::Index| match flag {
        _ if scroll.is_some() || snapshot.is_some() || req.profile => false,
        Some(v) => v == "true",
        None => req.size == 0 && index.settings.get("index.requests.cache.enable") != Some("false"),
    };
    let targets = match &snapshot {
        Some(s) => s
            .iter()
            .map(|(n, i, f)| (n.as_str(), i, f.as_ref()))
            .collect(),
        None => {
            shared = state.read();
            resolved = crate::indices::data_targets(&*shared, &name, &params)?;
            indices = shared.lock(resolved.iter().map(|t| &t.index));
            split = self::split(&indices, &resolved);
            routing = format!(
                "{:?}",
                resolved.iter().map(|t| &t.routing).collect::<Vec<_>>()
            );
            for t in &resolved {
                let index: &es_index::Index = &indices[&t.index];
                let of = index.number_of_shards();
                let shards = (index.shards().iter().enumerate())
                    .filter(|&(n, _)| routed(t, n, of))
                    .map(|(_, s)| (index, s));
                match cacheable(index) {
                    true => cache.extend(shards),
                    false => uncached = true,
                }
            }
            targets(&indices, &resolved, &split)
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
    let as_int = params
        .get("rest_total_hits_as_int")
        .is_some_and(|v| v == "true");
    let track = params
        .get("track_total_hits")
        .or(req.track_total_hits.as_ref())
        .map(String::as_str);
    // validated after the search runs, so that its errors come first
    let limit = (|| -> Result<_, AppError> {
        Ok(match track {
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
        })
    })();
    req.total_up_to = match limit {
        Ok(Some(Some(n))) => Some(n as usize),
        Ok(Some(None)) => Some(0),
        _ => None,
    };
    let respond = || -> Result<Value, AppError> {
        let r = es_query::search_many(&targets, &req)?;
        let hits = render_hits(&r.hits);
        let limit = limit?;
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
            let failures = (r.failures.iter()).map(|(i, n, e)| (*i, *n, e.clone()));
            s["failures"] = crate::error::shard_failures(failures).2.into();
        }
        if let Some(a) = r.aggregations {
            resp["aggregations"] = a;
        }
        if let Some(p) = r.profile {
            resp["profile"] = profile(p);
        }
        Ok(resp)
    };
    let mut resp = if cache.is_empty() {
        respond()?
    } else {
        // the request after terms lookups, and the shards it searches
        let mut key: Vec<_> = (params.iter())
            .filter(|(k, _)| !matches!(k.as_str(), "request_cache" | "preference"))
            .collect();
        key.sort();
        let searched: Vec<_> = targets.iter().map(|(name, _, f)| (name, f)).collect();
        let key = format!("{body}{key:?}{searched:?}{routing}");
        let found: Vec<_> = (cache.iter())
            .map(|(index, s)| s.cache_get(&index.settings, &index.mappings, &key))
            .collect();
        match &found[..] {
            [(Some(v), _), ..] if !uncached && found.iter().all(|f| f.0.is_some()) => v.clone(),
            _ => {
                let v = respond()?;
                for ((index, s), (_, visible)) in cache.iter().zip(&found) {
                    s.cache_put(&index.mappings, *visible, key.clone(), &v);
                }
                v
            }
        }
    };
    resp["took"] = (start.elapsed().as_millis() as u64).into();
    if let Some(p) = &pit_id {
        resp["pit_id"] = p.clone();
    }
    Ok(Json(match (scroll, snapshot) {
        (Some(keep), Some(snapshot)) => {
            let owner = authn.map(|a| a.owner());
            crate::scroll::open(&contexts, resp, snapshot, req, keep, owner)
        }
        _ => resp,
    }))
}

/// The `hits.hits` of a search response.
pub(crate) fn render_hits(hits: &[es_query::Hit]) -> Vec<Value> {
    hits.iter()
        .map(|h| {
            let mut hit = json!({"_index": h.index, "_id": h.id, "_score": h.score});
            if let Some(r) = &h.doc.routing {
                hit["_routing"] = r.as_str().into();
            }
            hit["_source"] = h.doc.source.clone();
            if !h.sort.is_empty() {
                hit["sort"] = h.sort.clone().into();
            }
            if !h.matched_queries.is_empty() {
                hit["matched_queries"] = h.matched_queries.clone().into();
            }
            if !h.fields.is_empty() {
                hit["fields"] = h.fields.clone().into();
            }
            hit
        })
        .collect()
}

pub async fn search_template_all(
    state: State<AppState>,
    contexts: Extension<Contexts>,
    authn: Option<Extension<Authn>>,
    uri: Uri,
    params: Query<HashMap<String, String>>,
    body: Bytes,
) -> Result<Json<Value>, AppError> {
    search_template(
        state,
        contexts,
        authn,
        OriginalUri(uri),
        Path("_all".into()),
        params,
        body,
    )
    .await
}

/// `_search/template`: renders the inline template, then runs it as a `_search`.
pub async fn search_template(
    state: State<AppState>,
    contexts: Extension<Contexts>,
    authn: Option<Extension<Authn>>,
    uri: OriginalUri,
    name: Path<String>,
    params: Query<HashMap<String, String>>,
    body: Bytes,
) -> Result<Json<Value>, AppError> {
    let body = crate::template::search_body(&body)?;
    search(state, contexts, authn, uri, name, params, body).await
}

pub async fn msearch(
    state: State<AppState>,
    contexts: Extension<Contexts>,
    path: Option<Path<String>>,
    authn: Option<Extension<Authn>>,
    uri: Uri,
    body: Bytes,
) -> Result<Json<Value>, AppError> {
    msearch_impl(state, contexts, path, authn, uri, body, false).await
}

pub async fn msearch_template(
    state: State<AppState>,
    contexts: Extension<Contexts>,
    path: Option<Path<String>>,
    authn: Option<Extension<Authn>>,
    uri: Uri,
    body: Bytes,
) -> Result<Json<Value>, AppError> {
    msearch_impl(state, contexts, path, authn, uri, body, true).await
}

/// `_msearch`: NDJSON header/body pairs, each run as a `_search`; a failing search fails only its own entry.
/// With `template`, each body is a search template rendered first.
async fn msearch_impl(
    State(state): State<AppState>,
    Extension(contexts): Extension<Contexts>,
    path: Option<Path<String>>,
    authn: Option<Extension<Authn>>,
    uri: Uri,
    body: Bytes,
    template: bool,
) -> Result<Json<Value>, AppError> {
    let start = Instant::now();
    let named = path.is_some();
    let default = path.map_or("_all".to_string(), |Path(p)| p);
    let text = std::str::from_utf8(&body)
        .map_err(|e| AppError::IllegalArgument(format!("request body is not UTF-8: {e}")))?;
    // Like Elasticsearch's request validation: an index beside any point in time (in the path or
    // that search's header) fails the whole msearch, not just its entry.
    let mut pairs = text.lines().filter(|l| !l.trim().is_empty());
    while let (false, Some(header), Some(body)) = (template, pairs.next(), pairs.next()) {
        let json = |l: &str| serde_json::from_str::<Value>(l).unwrap_or_default();
        if json(body).get("pit").is_some() && (named || json(header).get("index").is_some()) {
            return Err(AppError::ActionRequestValidation(PIT_WITH_INDICES.into()));
        }
    }
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
            // The search as if sent alone: whether it names indices, before authorization
            // narrows them, and the msearch's own query parameters.
            let path = if name == "_all" {
                "/_search"
            } else {
                "/named/_search"
            };
            let query = uri.query().map_or(String::new(), |q| format!("?{q}"));
            let original = format!("{path}{query}")
                .parse()
                .map_err(|e: InvalidUri| AppError::IllegalArgument(e.to_string()))?;
            let name = msearch_item(authn.as_deref(), &state, name, &params)?;
            let mut search_body = Bytes::copy_from_slice(search_body.as_bytes());
            if template {
                search_body = crate::template::search_body(&search_body)?;
            }
            search(
                State(state.clone()),
                Extension(contexts.clone()),
                authn.clone(),
                OriginalUri(original),
                Path(name),
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
    let shared = state.read();
    terms_lookups(&shared, &mut body)?;
    let query = match params.get("q") {
        Some(q) => query_from_params(q, &params)?,
        None => body
            .get("query")
            .map_or(Ok(es_query::Query::MatchAll), parse_query)?,
    };
    let resolved = crate::indices::data_targets(&*shared, &name, &params)?;
    let indices = shared.lock(resolved.iter().map(|t| &t.index));
    let split = split(&indices, &resolved);
    let targets = targets(&indices, &resolved, &split);
    let n = es_query::count_many(&targets, &query)?;
    let resp = json!({"count": n, "_shards": shards(targets.len())});
    Ok(Json(resp))
}

pub async fn esql(
    State(state): State<AppState>,
    authn: Option<Extension<Authn>>,
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
        .filter(|q| !q.is_empty())
        .ok_or_else(|| AppError::ActionRequestValidation("[query] is required".into()))?;
    // The narrowing's own error, which ES|QL's errors cannot carry.
    let refused = std::cell::Cell::new(None);
    let shared = state.read();
    let narrow = |expr: &str| {
        esql_from(authn.as_deref(), &shared, expr).map_err(|e| {
            refused.set(Some(e));
            EsqlError::Verification(String::new())
        })
    };
    let r = esql::run_narrowed(&shared.all(), query, &narrow);
    if let Some(e) = refused.take() {
        return Err(e);
    }
    let r = r.map_err(|e| match e {
        EsqlError::Parse(m) => AppError::ParsingException(m),
        EsqlError::Verification(m) => AppError::Verification(m),
        EsqlError::IndexClosed(i) => AppError::IndexClosedBlock(i),
        EsqlError::IndexNotFound(i) => AppError::IndexNotFound(i),
        EsqlError::Failed {
            status,
            kind,
            reason,
        } => AppError::Other(
            StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            kind,
            reason,
        ),
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
