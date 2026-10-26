//! Search contexts that expire: a `?scroll=` search keeps the rest of its hits, and a point in
//! time keeps a snapshot of its indices, in memory.

use crate::{AppState, error::AppError, indices::parse_body};
use axum::{
    Extension, Json,
    body::Bytes,
    extract::{Path, Query, State},
    http::{StatusCode, Uri},
};
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A search's remaining hits, rendered when it ran, so later writes do not change them.
// ponytail: hits are materialized up front; memory grows with the match count of open scrolls
struct Scroll {
    /// The first response, without hits, as the template of every page.
    template: Value,
    rest: VecDeque<Value>,
    size: usize,
    expires: Instant,
}

/// The indices a point in time was opened on: name, snapshot, and alias filter.
pub(crate) type Snapshot = Arc<Vec<(String, es_index::Index, Option<Value>)>>;

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

/// Keeps every hit of `resp` after the first `size` in a new scroll context, and returns the
/// first page with its `_scroll_id`.
pub(crate) fn open(contexts: &Contexts, mut resp: Value, size: usize, keep: Duration) -> Value {
    let mut rest: VecDeque<Value> = resp["hits"]["hits"]
        .take()
        .as_array_mut()
        .map_or_else(VecDeque::new, |a| std::mem::take(a).into());
    let mut store = contexts.lock().expect("lock poisoned");
    let id = store.next_id();
    let page = rest.drain(..size.min(rest.len())).collect::<Vec<_>>();
    store.live().insert(
        id,
        Scroll {
            template: resp.clone(),
            rest,
            size,
            expires: Instant::now() + keep,
        },
    );
    resp["hits"]["hits"] = page.into();
    resp["_scroll_id"] = encode(id).into();
    resp
}

// ponytail: ids are opaque to clients, so a prefixed counter stands in for ES's base64 encoding
fn encode(id: u64) -> String {
    format!("esmini-scroll-{id}")
}

fn decode(id: &str) -> Result<u64, AppError> {
    id.strip_prefix("esmini-scroll-")
        .and_then(|n| n.parse().ok())
        .ok_or_else(|| AppError::IllegalArgument("Cannot parse scroll id".into()))
}

fn decode_pit(id: &str) -> Result<u64, AppError> {
    id.strip_prefix("esmini-pit-")
        .and_then(|n| n.parse().ok())
        // ponytail: message not checked against Elasticsearch
        .ok_or_else(|| AppError::IllegalArgument(format!("invalid id: [{id}]")))
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
    let targets: Vec<_> = {
        let shared = state.read();
        let indices = shared.all();
        crate::indices::data_targets(&indices, &name, &params)?
            .into_iter()
            .map(|t| (t.index.clone(), indices[&t.index].snapshot(), t.filter))
            .collect()
    };
    let n = targets.len();
    let mut store = contexts.lock().expect("lock poisoned");
    let id = store.next_id();
    store.live_pits().insert(
        id,
        Pit {
            targets: Arc::new(targets),
            expires: Instant::now() + keep,
        },
    );
    Ok(Json(
        json!({"id": format!("esmini-pit-{id}"), "_shards": crate::search::shards(n)}),
    ))
}

/// The snapshot of the point in time `pit` (`{id, keep_alive}`), renewing its keep-alive.
pub(crate) fn pit(contexts: &Contexts, pit: &Value) -> Result<Snapshot, AppError> {
    let id = pit["id"]
        .as_str()
        .ok_or_else(|| AppError::ParsingException("[pit] requires an [id]".into()))?;
    let n = decode_pit(id)?;
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
    let n = decode(id)?;
    let keep = scroll.as_deref().map(keep_alive).transpose()?;
    let mut store = contexts.lock().expect("lock poisoned");
    let s = store
        .live()
        .get_mut(&n)
        .ok_or(AppError::SearchContextMissing(n))?;
    if let Some(keep) = keep {
        s.expires = Instant::now() + keep;
    }
    let mut resp = s.template.clone();
    let size = s.size.min(s.rest.len());
    resp["hits"]["hits"] = s.rest.drain(..size).collect::<Vec<_>>().into();
    resp["took"] = (start.elapsed().as_millis() as u64).into();
    resp["_scroll_id"] = id.clone().into();
    Ok(Json(resp))
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
