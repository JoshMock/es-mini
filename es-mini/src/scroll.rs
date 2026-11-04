//! Search contexts that expire: a `?scroll=` search and a point in time keep a snapshot of
//! their indices (see [`es_index::Index::snapshot`]).

use crate::{AppState, error::AppError, indices::parse_body, security::Authn};
use axum::{
    Extension, Json,
    body::Bytes,
    extract::{Path, Query, State},
    http::{StatusCode, Uri},
};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A search's snapshot, which each page searches again, so later writes do not change it.
// ponytail: a page reruns the query, costing a search of the hits before it, and reads `now`
// again where ES fixes it when the scroll opens
struct Scroll {
    /// The first response, without hits, as the template of every page.
    template: Value,
    targets: Snapshot,
    /// The search without aggregations, from the next page.
    req: es_query::SearchRequest,
    expires: Instant,
    /// Who opened it ([`Authn::owner`]; `None` without security): only they may continue it.
    owner: Option<String>,
}

/// The shards a point in time was opened on: index name, shard number, snapshot, and alias
/// filter.
pub(crate) type Snapshot = Arc<Vec<(String, usize, es_index::Index, Option<Value>)>>;

struct Pit {
    targets: Snapshot,
    expires: Instant,
}

#[derive(Default)]
pub(crate) struct Store {
    next: u64,
    scrolls: HashMap<u64, Scroll>,
    pits: HashMap<u64, Pit>,
}

impl Store {
    fn live(&mut self) -> &mut HashMap<u64, Scroll> {
        self.prune();
        &mut self.scrolls
    }

    fn live_pits(&mut self) -> &mut HashMap<u64, Pit> {
        self.prune();
        &mut self.pits
    }

    fn prune(&mut self) {
        let now = Instant::now();
        self.scrolls.retain(|_, s| s.expires > now);
        self.pits.retain(|_, p| p.expires > now);
    }

    fn next_id(&mut self) -> u64 {
        self.next += 1;
        self.next
    }
}

pub(crate) type Contexts = Arc<Mutex<Store>>;

/// The `scroll` keep-alive parameter.
pub(crate) fn keep_alive(v: &str) -> Result<Duration, AppError> {
    es_index::parse_time(v).ok_or_else(|| {
        AppError::Parse(format!(
            "failed to parse setting [scroll] with value [{v}] as a time value: unit is missing or unrecognized"
        ))
    })
}

/// Opens a scroll context over `targets` for the pages of `req` after `resp`, its first, and
/// returns that page with its `_scroll_id`.
pub(crate) fn open(
    contexts: &Contexts,
    mut resp: Value,
    targets: Snapshot,
    mut req: es_query::SearchRequest,
    keep: Duration,
    owner: Option<String>,
) -> Value {
    let mut template = resp.clone();
    template["hits"]["hits"] = json!([]);
    req.from += req.size;
    req.aggs.clear();
    req.pipelines.clear();
    req.profile = false;
    let mut store = contexts.lock().expect("lock poisoned");
    let id = store.next_id();
    store.live().insert(
        id,
        Scroll {
            template,
            targets,
            req,
            expires: Instant::now() + keep,
            owner,
        },
    );
    resp["_scroll_id"] = encode(id).into();
    resp
}

/// The snapshots of the indices `name` resolves to, for a point in time or a scroll.
pub(crate) fn snapshot(
    state: &AppState,
    name: &str,
    params: &HashMap<String, String>,
) -> Result<Snapshot, AppError> {
    let shared = state.read();
    let resolved = crate::indices::data_targets(&*shared, name, params)?;
    let indices = shared.lock(resolved.iter().map(|t| &t.index));
    let mut shards: Vec<_> = (resolved.iter())
        .flat_map(|t| {
            let shards = indices[&t.index].shard_snapshots();
            let of = shards.len();
            (shards.into_iter().enumerate())
                .filter(move |&(n, _)| crate::search::routed(t, n, of))
                .map(move |(n, s)| (t.index.clone(), n, s, t.filter.clone()))
        })
        .collect();
    // in shard order, as crate::search::targets
    shards.sort_by(|(a, n, ..), (b, m, ..)| (n, a).cmp(&(m, b)));
    Ok(Arc::new(shards))
}

// ponytail: ids are opaque to clients, so a prefixed counter stands in for ES's base64 encoding
fn encode(id: u64) -> String {
    format!("esmini-scroll-{id}")
}

// Ids es-mini issued are checked first: some lengths of them are not valid base64.
fn decode(id: &str) -> Result<u64, AppError> {
    if let Some(n) = id
        .strip_prefix("esmini-scroll-")
        .and_then(|n| n.parse().ok())
    {
        return Ok(n);
    }
    let error = AppError::IllegalArgument("Cannot parse scroll id".into());
    Err(match base64_error(id) {
        Some(e) => {
            let cause = json!({"type": "illegal_argument_exception", "reason": e});
            AppError::Caused(Box::new(error), cause)
        }
        None => error,
    })
}

fn decode_pit(id: &str) -> Result<u64, AppError> {
    if let Some(n) = id.strip_prefix("esmini-pit-").and_then(|n| n.parse().ok()) {
        return Ok(n);
    }
    // ponytail: ES reads a base64 id on and fails on what it holds; es-mini, whose ids are
    // not ES's, gives this message for any base64 id it did not issue
    Err(AppError::IllegalArgument(
        base64_error(id).unwrap_or_else(|| format!("invalid id: [{id}]")),
    ))
}

/// The error Java's URL-safe `Base64` decoder throws for `id` (read as ISO-8859-1 bytes),
/// which Elasticsearch reports for an id it cannot decode.
fn base64_error(id: &str) -> Option<String> {
    // characters past U+00FF encode as `?`
    let src: Vec<u8> = id
        .chars()
        .map(|c| u8::try_from(c).unwrap_or(b'?'))
        .collect();
    use es_index::es_codec::base64;
    base64::decode(&src, base64::URL).err()
}

/// `POST /{index}/_pit`: snapshots the indices `index` resolves to.
pub async fn open_pit(
    State(state): State<AppState>,
    Extension(contexts): Extension<Contexts>,
    Path(name): Path<String>,
    uri: Uri,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<Value>, AppError> {
    crate::params::check(
        &uri,
        &params,
        &[
            "keep_alive",
            "ignore_unavailable",
            "expand_wildcards",
            "allow_partial_search_results",
            "routing",
            "preference",
            "max_concurrent_shard_requests",
        ],
    )?;
    let keep = params
        .get("keep_alive")
        .ok_or_else(|| AppError::ActionRequestValidation("[keep_alive] is not specified".into()))?;
    let keep = keep_alive(keep)?;
    let targets = snapshot(&state, &name, &params)?;
    let n = targets.len();
    let mut store = contexts.lock().expect("lock poisoned");
    let id = store.next_id();
    store.live_pits().insert(
        id,
        Pit {
            targets,
            expires: Instant::now() + keep,
        },
    );
    Ok(Json(
        json!({"id": format!("esmini-pit-{id}"), "_shards": crate::search::shards(n)}),
    ))
}

/// The snapshot of the point in time `pit` (`{id, keep_alive}`), renewing its keep-alive.
/// The point in time `pit` names, from the search body `raw`.
pub(crate) fn pit(contexts: &Contexts, pit: &Value, raw: &[u8]) -> Result<Snapshot, AppError> {
    let id = pit["id"]
        .as_str()
        .ok_or_else(|| AppError::ParsingException("[pit] requires an [id]".into()))?;
    let n = decode_pit(id).map_err(|e| {
        let (_, mut cause) = e.parts();
        cause.as_object_mut().map(|c| c.remove("root_cause"));
        let at = es_index::mapping::locate(raw, "pit.id", &pit["id"], false);
        let at = at.map_or(String::new(), |(l, c)| format!("[{l}:{c}] "));
        let error = AppError::XContentParse(format!("{at}[pit] failed to parse field [id]"));
        AppError::Caused(Box::new(error), cause)
    })?;
    let keep = pit["keep_alive"].as_str().map(keep_alive).transpose()?;
    let mut store = contexts.lock().expect("lock poisoned");
    let p = store
        .live_pits()
        .get_mut(&n)
        .ok_or(AppError::SearchContextMissing(n))?;
    if let Some(keep) = keep {
        p.expires = Instant::now() + keep;
    }
    Ok(p.targets.clone())
}

/// `DELETE /_pit`: frees a point in time.
pub async fn close_pit(
    Extension(contexts): Extension<Contexts>,
    body: Bytes,
) -> Result<(StatusCode, Json<Value>), AppError> {
    let body = parse_body(&body)?;
    let id = body["id"]
        .as_str()
        .ok_or_else(|| AppError::ActionRequestValidation("[id] is required".into()))?;
    if id.is_empty() {
        return Err(AppError::IllegalArgument(
            "search context id is is not provided".into(),
        ));
    }
    let n = decode_pit(id)?;
    let freed = usize::from(
        contexts
            .lock()
            .expect("lock poisoned")
            .live_pits()
            .remove(&n)
            .is_some(),
    );
    let status = if freed == 0 {
        StatusCode::NOT_FOUND
    } else {
        StatusCode::OK
    };
    Ok((status, Json(json!({"succeeded": true, "num_freed": freed}))))
}

/// `scroll_id` and `scroll` from the path, query string or body (body wins, as in ES).
fn request(
    path: Option<Path<String>>,
    params: &HashMap<String, String>,
    body: &Bytes,
) -> Result<(Vec<String>, Option<String>), AppError> {
    let body = parse_body(body)?;
    let ids = match &body["scroll_id"] {
        Value::String(s) => vec![s.clone()],
        Value::Array(a) => a
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        _ => path
            .map(|Path(p)| p)
            .or_else(|| params.get("scroll_id").cloned())
            .map(|s| s.split(',').map(str::to_owned).collect())
            .unwrap_or_default(),
    };
    let scroll = body["scroll"]
        .as_str()
        .map(str::to_owned)
        .or_else(|| params.get("scroll").cloned());
    Ok((ids, scroll))
}

/// `GET`/`POST /_search/scroll`: the next page of a scroll context.
pub async fn next(
    Extension(contexts): Extension<Contexts>,
    authn: Option<Extension<Authn>>,
    path: Option<Path<String>>,
    Query(params): Query<HashMap<String, String>>,
    body: Bytes,
) -> Result<Json<Value>, AppError> {
    let start = Instant::now();
    let (ids, scroll) = request(path, &params, &body)?;
    let [id] = ids.as_slice() else {
        return Err(AppError::ActionRequestValidation(
            "scrollId is missing".into(),
        ));
    };
    let keep = scroll.as_deref().map(keep_alive).transpose()?;
    let owner = authn.map(|a| a.owner());
    let mut resp = page(&contexts, decode(id)?, keep, owner)?;
    resp["took"] = (start.elapsed().as_millis() as u64).into();
    resp["_scroll_id"] = id.clone().into();
    Ok(Json(resp))
}

/// The next page of scroll `n` for `owner`, renewing its keep-alive with `keep`. Like
/// Elasticsearch, a scroll that someone else continues is missing to them and freed.
fn page(
    contexts: &Contexts,
    n: u64,
    keep: Option<Duration>,
    owner: Option<String>,
) -> Result<Value, AppError> {
    let (mut resp, targets, req) = {
        let mut store = contexts.lock().expect("lock poisoned");
        let live = store.live();
        if live.get(&n).is_some_and(|s| s.owner != owner) {
            live.remove(&n);
        }
        let s = live.get_mut(&n).ok_or(AppError::SearchContextMissing(n))?;
        if let Some(keep) = keep {
            s.expires = Instant::now() + keep;
        }
        let req = s.req.clone();
        s.req.from += s.req.size;
        (s.template.clone(), s.targets.clone(), req)
    };
    let targets: Vec<_> = (targets.iter())
        .map(|(n, shard, i, f)| (n.as_str(), *shard, i, f.as_ref()))
        .collect();
    let r = es_query::search_many(&targets, &req)?;
    resp["hits"]["hits"] = crate::search::render_hits(&r.hits).into();
    Ok(resp)
}

/// `DELETE /_search/scroll`: frees the given contexts, or all of them for `_all`.
pub async fn clear(
    Extension(contexts): Extension<Contexts>,
    path: Option<Path<String>>,
    Query(params): Query<HashMap<String, String>>,
    body: Bytes,
) -> Result<(StatusCode, Json<Value>), AppError> {
    let (ids, _) = request(path, &params, &body)?;
    if ids.is_empty() {
        return Err(AppError::ActionRequestValidation(
            "no scroll ids specified".into(),
        ));
    }
    let mut store = contexts.lock().expect("lock poisoned");
    let live = store.live();
    let freed = if ids.iter().any(|i| i == "_all") {
        std::mem::take(live).len()
    } else {
        let ids = ids
            .iter()
            .map(|i| decode(i))
            .collect::<Result<Vec<_>, _>>()?;
        ids.iter().filter(|n| live.remove(n).is_some()).count()
    };
    let status = if freed == 0 {
        StatusCode::NOT_FOUND
    } else {
        StatusCode::OK
    };
    Ok((status, Json(json!({"succeeded": true, "num_freed": freed}))))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ids es-mini issued decode even where they are not valid base64 (17-character scroll ids).
    #[test]
    fn issued_ids_decode_at_every_length() {
        for n in 1..2000 {
            assert_eq!(decode(&encode(n)).ok(), Some(n), "scroll {n}");
            assert_eq!(
                decode_pit(&format!("esmini-pit-{n}")).ok(),
                Some(n),
                "pit {n}"
            );
        }
    }

    #[test]
    fn memory_bounds_scroll_keeps_its_snapshot_not_its_hits() {
        let mut ix = es_index::Index::default();
        for n in 0..5 {
            ix.put(&n.to_string(), json!({"n": n}));
        }
        let targets: Snapshot = Arc::new(vec![("i".into(), 0, ix.snapshot(), None)]);
        let req = es_query::SearchRequest::parse(&json!({"size": 2, "sort": ["n"]})).unwrap();
        let contexts = Contexts::default();
        let first = json!({"hits": {"hits": ["first page"]}});
        let first = open(
            &contexts,
            first,
            targets,
            req,
            Duration::from_secs(60),
            None,
        );
        assert_eq!(first["_scroll_id"], "esmini-scroll-1");
        let template = contexts.lock().unwrap().scrolls[&1].template.clone();
        assert_eq!(template["hits"]["hits"], json!([]));
        ix.delete("2");
        let ids = |r: Value| -> Vec<Value> {
            let hits = r["hits"]["hits"].as_array().unwrap().iter();
            hits.map(|h| h["_id"].clone()).collect()
        };
        assert_eq!(ids(page(&contexts, 1, None, None).unwrap()), ["2", "3"]);
        assert_eq!(ids(page(&contexts, 1, None, None).unwrap()), ["4"]);
        assert!(ids(page(&contexts, 1, None, None).unwrap()).is_empty());
    }
}
