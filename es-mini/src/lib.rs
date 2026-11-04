//! es-mini: an Elasticsearch-compatible HTTP server.

mod aliases;
mod bulk;
mod by_query;
mod docs;
mod error;
mod field_caps;
mod indices;
mod nodes;
mod params;
mod scroll;
mod search;
mod security;
mod store;
mod system;
mod template;

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{StatusCode, Uri},
    response::IntoResponse,
    routing::{delete, get, post, put},
};
use error::AppError;
use es_index::Indices;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use store::Store;

type AppState = Arc<Store>;

const CLUSTER_NAME: &str = "es-mini";

async fn info() -> Json<Value> {
    Json(json!({
        "name": CLUSTER_NAME,
        "cluster_name": CLUSTER_NAME,
        "cluster_uuid": "_na_",
        "version": {
            "number": "9.5.5",
            "build_flavor": "default",
            "build_type": "es-mini",
            "build_hash": "unknown",
            "build_date": "1970-01-01T00:00:00.000Z",
            "build_snapshot": false,
            "lucene_version": "10.5.1",
            "minimum_wire_compatibility_version": "8.19.0",
            "minimum_index_compatibility_version": "8.0.0"
        },
        "tagline": "You Know, for Search"
    }))
}

const HEALTH_PARAMS: [&str; 11] = [
    "level",
    "local",
    "timeout",
    "master_timeout",
    "wait_for_status",
    "wait_for_no_relocating_shards",
    "wait_for_no_initializing_shards",
    "wait_for_active_shards",
    "wait_for_nodes",
    "wait_for_events",
    "expand_wildcards",
];

async fn health(
    State(state): State<AppState>,
    index: Option<Path<String>>,
    uri: Uri,
    Query(params): Query<HashMap<String, String>>,
) -> Result<(StatusCode, Json<Value>), AppError> {
    params::check(&uri, &params, &HEALTH_PARAMS)?;
    let store = state.read();
    let names: Vec<String> = match &index {
        None => store.keys().cloned().collect(),
        Some(Path(expr)) => match es_index::aliases::resolve_read(&*store, expr) {
            Ok(targets) => targets.into_iter().map(|t| t.index).collect(),
            Err(_) => {
                let mut body = health_body(0, 0);
                body["status"] = json!("red");
                body["timed_out"] = json!(true);
                return Ok((StatusCode::REQUEST_TIMEOUT, Json(body)));
            }
        },
    };
    let copies: Vec<(usize, usize)> = (names.iter())
        .map(|n| {
            store
                .read(n)
                .map_or((1, 0), |i| (i.number_of_shards(), i.number_of_replicas()))
        })
        .collect();
    let level = params.get("level").map(String::as_str);
    let shards = copies.iter().map(|(n, _)| n).sum();
    let mut body = health_body(shards, copies.iter().map(|(n, r)| n * r).sum());
    if matches!(level, Some("indices" | "shards")) {
        let indices: serde_json::Map<String, Value> = (names.iter().zip(copies))
            .map(|(name, (n, r))| (name.clone(), index_health(n, r, level == Some("shards"))))
            .collect();
        body["indices"] = Value::Object(indices);
    }
    // ponytail: answers at once rather than after `timeout`; replicas never get assigned, so
    // only a concurrent settings change could turn the status green in the meantime
    if params.get("wait_for_status").is_some_and(|s| s == "green") && body["status"] != "green" {
        body["timed_out"] = json!(true);
        return Ok((StatusCode::REQUEST_TIMEOUT, Json(body)));
    }
    Ok((StatusCode::OK, Json(body)))
}

/// `yellow` while replicas are unassigned (they always are, on one node), else `green`.
pub(crate) fn status(unassigned: usize) -> &'static str {
    if unassigned > 0 { "yellow" } else { "green" }
}

/// Cluster health over `shards` active primaries and `unassigned` replicas.
fn health_body(shards: usize, unassigned: usize) -> Value {
    let percent = match shards + unassigned {
        0 => 100.0,
        total => shards as f64 / total as f64 * 100.0,
    };
    json!({
        "cluster_name": CLUSTER_NAME,
        "status": status(unassigned),
        "timed_out": false,
        "number_of_nodes": 1,
        "number_of_data_nodes": 1,
        "active_primary_shards": shards,
        "active_shards": shards,
        "relocating_shards": 0,
        "initializing_shards": 0,
        "unassigned_shards": unassigned,
        "unassigned_primary_shards": 0,
        "delayed_unassigned_shards": 0,
        "number_of_pending_tasks": 0,
        "number_of_in_flight_fetch": 0,
        "task_max_waiting_in_queue_millis": 0,
        "active_shards_percent_as_number": percent
    })
}

/// The health of an index of `n` shards with `replicas` unassigned replicas each, listing each
/// shard's with `shards`.
fn index_health(n: usize, replicas: usize, shards: bool) -> Value {
    let mut h = json!({
        "status": status(replicas),
        "number_of_shards": n,
        "number_of_replicas": replicas,
        "active_primary_shards": n,
        "active_shards": n,
        "relocating_shards": 0,
        "initializing_shards": 0,
        "unassigned_shards": n * replicas,
        "unassigned_primary_shards": 0
    });
    if shards {
        let shard = json!({
            "status": status(replicas),
            "primary_active": true,
            "active_shards": 1,
            "relocating_shards": 0,
            "initializing_shards": 0,
            "unassigned_shards": replicas,
            "unassigned_primary_shards": 0
        });
        h["shards"] = (0..n).map(|i| (i.to_string(), shard.clone())).collect();
    }
    h
}

/// The full HTTP API over a fresh, empty in-memory store.
pub fn app() -> Router {
    app_with(Indices::new())
}

/// The full HTTP API over `indices`, e.g. ones opened with [`es_index::disk::open`].
pub fn app_with(indices: Indices) -> Router {
    with_es_405(router(Arc::new(Store::new(indices)), None))
}

/// The full HTTP API over a fresh, empty in-memory store, configured by node `settings` (by
/// name, as in `elasticsearch.yml` and its keystore): `xpack.security.enabled` and
/// `bootstrap.password`.
pub fn app_with_settings(settings: &HashMap<String, String>) -> Result<Router, String> {
    let state = Arc::new(Store::new(Indices::new()));
    let security = security::from_settings(settings, &state)?;
    let app = with_es_405(router(state, security.clone()));
    Ok(secured(app, security))
}

/// `app` authenticating and authorizing every request when `security` is on.
fn secured(app: Router, security: Option<Arc<security::Security>>) -> Router {
    match security {
        Some(s) => app
            .layer(axum::middleware::from_fn_with_state(
                s.clone(),
                security::authorize,
            ))
            .layer(axum::middleware::from_fn_with_state(
                s,
                security::authenticate,
            )),
        None => app,
    }
}

/// The full HTTP API over the data directory `dir` (created if missing): its indices are loaded
/// now (replaying their translogs), and every successful write request is made durable before
/// it is answered (see [`es_index::disk::persist`]).
pub fn app_persisted(dir: &std::path::Path) -> Result<Router, String> {
    app_persisted_with_settings(dir, &HashMap::new())
}

/// [`app_persisted`] configured by `settings` as [`app_with_settings`] is.
pub fn app_persisted_with_settings(
    dir: &std::path::Path,
    settings: &HashMap<String, String>,
) -> Result<Router, String> {
    persisted(dir, settings).map(|(app, _)| app)
}

/// Serves [`app_persisted`] over `dir`, configured by `settings` as [`app_with_settings`] is, on
/// `listener` until `shutdown` completes, lets in-flight requests finish, then flushes: commits every index's segments and empties its translog, so
/// Elasticsearch opens the directory without replaying anything.
pub async fn serve_persisted(
    listener: tokio::net::TcpListener,
    dir: &std::path::Path,
    settings: &HashMap<String, String>,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<(), String> {
    let (app, state) = persisted(dir, settings)?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await
        .map_err(|e| e.to_string())?;
    es_index::disk::save(dir, &state.write_all()).map_err(|e| e.0)
}

fn persisted(
    dir: &std::path::Path,
    settings: &HashMap<String, String>,
) -> Result<(Router, AppState), String> {
    es_index::disk::recover(dir).map_err(|e| e.0)?;
    let indices = if dir.join("_state").is_dir() {
        es_index::disk::open(dir).map_err(|e| e.0)?
    } else {
        Indices::new()
    };
    let state = Arc::new(Store::new(indices));
    let security = security::from_settings(settings, &state)?;
    spawn_syncer(dir.to_owned(), Arc::downgrade(&state));
    let (dir, saved, group) = (dir.to_owned(), state.clone(), Arc::new(Group::default()));
    let routes = router(state.clone(), security.clone());
    let app = with_es_405(routes.layer(axum::middleware::from_fn(
        move |req: axum::extract::Request, next: axum::middleware::Next| {
            let (dir, state, group) = (dir.clone(), saved.clone(), group.clone());
            async move {
                let route = req.extensions().get::<axum::extract::MatchedPath>();
                let flush = route.is_some_and(|r| r.as_str().ends_with("/_flush"));
                let reads = (req.method().is_safe() && !flush)
                    || route.is_some_and(|r| {
                        let r = r.as_str();
                        r.contains("/_search")
                            || r.contains("/_msearch")
                            || ["/_count", "/_query", "/_field_caps", "/_pit"]
                                .iter()
                                .any(|s| r.ends_with(s))
                    });
                let res = next.run(req).await;
                if reads || !res.status().is_success() {
                    return res;
                }
                let saved = {
                    let dir = dir.clone();
                    tokio::task::spawn_blocking(move || {
                        // The fsync runs without the indices lock, so other writes apply meanwhile.
                        group.persist(|| {
                            let begun = es_index::disk::begin_persist(&dir, &mut state.write_all());
                            begun
                                .and_then(es_index::disk::Pending::finish)
                                .map_err(|e| e.0)
                        })
                    })
                    .await
                };
                match saved.unwrap_or_else(|e| Err(e.to_string())) {
                    Ok(()) => res,
                    // The change stays applied in memory; the next write or sync retries its log.
                    Err(e) => {
                        tracing::error!("saving {}: {e}", dir.display());
                        AppError::Io(format!(
                            "the change was applied but not saved to {}: {e}",
                            dir.display()
                        ))
                        .into_response()
                    }
                }
            }
        },
    )));
    Ok((secured(app, security), state))
}

/// Group commit: one write request at a time persists (a round), covering every write applied
/// before it took the unlogged operations; writes that finish meanwhile wait and share the
/// next round's fsync.
#[derive(Default)]
struct Group {
    rounds: std::sync::Mutex<Rounds>,
    done: std::sync::Condvar,
}

#[derive(Default)]
struct Rounds {
    running: bool,
    /// The result of the round that writes arriving now wait for; it starts once the running
    /// one finishes.
    next: Arc<std::sync::OnceLock<Result<(), String>>>,
}

impl Group {
    /// Returns once a round that began after this call made the caller's writes durable, with
    /// that round's result. `save` runs the round if this caller is the one to start it.
    fn persist(&self, save: impl FnOnce() -> Result<(), String>) -> Result<(), String> {
        let mut r = self.rounds.lock().expect("lock poisoned");
        let mine = r.next.clone();
        while r.running {
            r = self.done.wait(r).expect("lock poisoned");
        }
        if let Some(saved) = mine.get() {
            return saved.clone();
        }
        r.running = true;
        r.next = Arc::default();
        drop(r);
        let saved = save();
        mine.set(saved.clone()).expect("one caller runs each round");
        self.rounds.lock().expect("lock poisoned").running = false;
        self.done.notify_all();
        saved
    }
}

/// Runs [`es_index::disk::sync`] every [`es_index::disk::sync_interval`] until `state` is dropped.
fn spawn_syncer(dir: std::path::PathBuf, state: std::sync::Weak<Store>) {
    use std::time::{Duration, Instant};
    std::thread::spawn(move || {
        let mut last = Instant::now();
        loop {
            // Elasticsearch's smallest index.translog.sync_interval.
            std::thread::sleep(Duration::from_millis(100));
            let Some(state) = state.upgrade() else { return };
            let every = es_index::disk::sync_interval(&state.read().all());
            if last.elapsed() < every {
                continue;
            }
            last = Instant::now();
            // The fsync runs without the indices lock, as for write requests.
            let begun = es_index::disk::begin_sync(&dir, &mut state.write_all());
            if let Err(e) = begun.and_then(es_index::disk::Pending::finish) {
                tracing::error!("syncing {}: {e}", dir.display());
            }
        }
    });
}

/// Applies `filter_path` to every JSON response, errors included, as Java's `RestController` does.
async fn filter_response(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let filter = axum::extract::Query::<HashMap<String, String>>::try_from_uri(req.uri())
        .ok()
        .and_then(|q| q.0.get("filter_path").cloned());
    let res = next.run(req).await;
    let json_body = res
        .headers()
        .get("content-type")
        .is_some_and(|c| c.as_bytes().starts_with(b"application/json"));
    let Some(f) = filter.filter(|_| json_body) else {
        return res;
    };
    let (mut parts, body) = res.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, usize::MAX).await else {
        return parts.status.into_response();
    };
    let Ok(v) = serde_json::from_slice::<Value>(&bytes) else {
        return (parts, bytes).into_response();
    };
    let out = serde_json::to_vec(&es_index::filter_path::apply(&v, &f)).unwrap_or_default();
    parts.headers.remove("content-length");
    (parts, out).into_response()
}

fn router(state: AppState, security: Option<Arc<security::Security>>) -> Router {
    let routes = match security {
        Some(s) => security::routes(s),
        None => Router::new(),
    };
    routes
        .route("/", get(info))
        .route("/_cluster/health", get(health))
        .route("/_cluster/health/{index}", get(health))
        .route("/_cluster/stats", get(nodes::cluster_stats))
        .route("/_cluster/settings", get(nodes::cluster_settings))
        .route("/_nodes", get(nodes::info))
        .route("/_nodes/stats", get(nodes::stats))
        .route("/_nodes/{filter}", get(nodes::info))
        .route(
            "/{index}",
            put(indices::create)
                .get(indices::get)
                .head(indices::head)
                .delete(indices::delete),
        )
        .route(
            "/_settings",
            put(indices::put_all_settings).get(indices::get_all_settings),
        )
        .route("/_settings/{name}", get(indices::get_all_settings_named))
        .route(
            "/{index}/_settings",
            put(indices::put_settings).get(indices::get_settings),
        )
        .route(
            "/{index}/_settings/{name}",
            get(indices::get_settings_named),
        )
        .route("/{index}/_close", post(indices::close))
        .route("/{index}/_open", post(indices::open))
        .route("/_alias", get(aliases::get_all))
        .route("/_aliases", get(aliases::get_all).post(aliases::update))
        .route(
            "/_alias/{name}",
            get(aliases::get_named).head(aliases::head_named),
        )
        .route("/{index}/_alias", get(aliases::get_for_index))
        .route("/{index}/_aliases", get(aliases::get_for_index))
        .route(
            "/{index}/_alias/{name}",
            put(aliases::put)
                .post(aliases::put)
                .delete(aliases::delete)
                .get(aliases::get_for_index_named)
                .head(aliases::head_for_index_named),
        )
        .route(
            "/{index}/_aliases/{name}",
            put(aliases::put).post(aliases::put).delete(aliases::delete),
        )
        .route(
            "/{index}/_refresh",
            get(indices::refresh).post(indices::refresh),
        )
        .route(
            "/_refresh",
            get(indices::refresh_all).post(indices::refresh_all),
        )
        .route("/{index}/_flush", get(indices::flush).post(indices::flush))
        .route("/_flush", get(indices::flush_all).post(indices::flush_all))
        .route(
            "/{index}/_mapping",
            get(indices::get_mapping)
                .put(indices::put_mapping)
                .post(indices::put_mapping),
        )
        .route("/_mapping", get(indices::get_all_mappings))
        .route("/_stats", get(indices::stats))
        .route("/_stats/{metric}", get(indices::stats))
        .route("/{index}/_stats", get(indices::stats))
        .route("/{index}/_stats/{metric}", get(indices::stats))
        .route("/_cat/indices", get(indices::cat_indices))
        .route("/_cat/indices/{index}", get(indices::cat_indices))
        .route("/_cat/aliases", get(aliases::cat))
        .route("/_cat/aliases/{alias}", get(aliases::cat))
        .route("/_cat/shards", get(indices::cat_shards))
        .route("/_cat/shards/{index}", get(indices::cat_shards))
        .route("/_query", post(search::esql))
        .route("/_bulk", post(bulk::bulk).put(bulk::bulk))
        .route(
            "/{index}/_bulk",
            post(bulk::bulk_index).put(bulk::bulk_index),
        )
        .route("/_search", get(search::search_all).post(search::search_all))
        .route("/{index}/_search", get(search::search).post(search::search))
        .route(
            "/_search/template",
            get(search::search_template_all).post(search::search_template_all),
        )
        .route(
            "/{index}/_search/template",
            get(search::search_template).post(search::search_template),
        )
        .route(
            "/_msearch/template",
            get(search::msearch_template).post(search::msearch_template),
        )
        .route(
            "/{index}/_msearch/template",
            get(search::msearch_template).post(search::msearch_template),
        )
        .route("/{index}/_pit", post(scroll::open_pit))
        .route("/_pit", delete(scroll::close_pit))
        .route(
            "/_search/scroll",
            get(scroll::next).post(scroll::next).delete(scroll::clear),
        )
        .route(
            "/_search/scroll/{scroll_id}",
            get(scroll::next).post(scroll::next).delete(scroll::clear),
        )
        .route("/_msearch", get(search::msearch).post(search::msearch))
        .route(
            "/{index}/_msearch",
            get(search::msearch).post(search::msearch),
        )
        .route(
            "/_field_caps",
            get(field_caps::field_caps_all).post(field_caps::field_caps_all),
        )
        .route(
            "/{index}/_field_caps",
            get(field_caps::field_caps).post(field_caps::field_caps),
        )
        .route("/_count", get(search::count_all).post(search::count_all))
        .route("/{index}/_count", get(search::count).post(search::count))
        .route("/{index}/_doc", post(docs::index_auto_id))
        .route(
            "/{index}/_doc/{id}",
            put(docs::index_with_id)
                .post(docs::index_with_id)
                .get(docs::get)
                .delete(docs::delete),
        )
        .route("/_mget", get(docs::mget).post(docs::mget))
        .route("/{index}/_mget", get(docs::mget).post(docs::mget))
        .route("/{index}/_source/{id}", get(docs::get_source))
        .route(
            "/{index}/_create/{id}",
            put(docs::create).post(docs::create),
        )
        .route("/{index}/_update/{id}", post(docs::update))
        .route("/{index}/_update_by_query", post(by_query::update_by_query))
        .route("/{index}/_delete_by_query", post(by_query::delete_by_query))
        // Like Java's `RestController.handleBadRequest`: a bare string error, no `status` field.
        .fallback(
            |method: axum::http::Method, uri: axum::http::Uri| async move {
                let error = format!("no handler found for uri [{uri}] and method [{method}]");
                (
                    axum::http::StatusCode::BAD_REQUEST,
                    Json(json!({ "error": error })),
                )
            },
        )
        .with_state(state)
        // Elasticsearch's default http.max_content_length; axum's own default is 2 MB.
        .layer(axum::extract::DefaultBodyLimit::max(100 << 20))
        .layer(axum::Extension(scroll::Contexts::default()))
        .layer(axum::middleware::from_fn(filter_response))
}

/// Wraps finished routes (route layers included) so 405s get Elasticsearch's body: axum adds
/// `Allow` around the routes, after any layer on them runs.
fn with_es_405(routes: Router) -> Router {
    Router::new()
        .fallback_service(routes)
        .layer(axum::middleware::from_fn(method_not_allowed))
}

/// Rewrites axum's empty 405 into Elasticsearch's `RestController` response: `{error, status}`
/// naming the allowed methods, with the `Allow` header kept.
// ponytail: axum adds HEAD wherever GET is routed, so `allowed` may list HEAD where ES does not
async fn method_not_allowed(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::http::{StatusCode, header::ALLOW};
    let uri = req
        .uri()
        .path_and_query()
        .map_or("", |p| p.as_str())
        .to_owned();
    let method = req.method().to_string();
    let res = next.run(req).await;
    if res.status() != StatusCode::METHOD_NOT_ALLOWED {
        return res;
    }
    let allow = res.headers().get(ALLOW).cloned();
    let listed = allow
        .as_ref()
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .replace(',', ", ");
    let error =
        format!("Incorrect HTTP method for uri [{uri}] and method [{method}], allowed: [{listed}]");
    let mut out = (
        StatusCode::METHOD_NOT_ALLOWED,
        Json(json!({"error": error, "status": 405})),
    )
        .into_response();
    if let Some(h) = allow {
        out.headers_mut().insert(ALLOW, h);
    }
    out
}

#[cfg(test)]
mod indices_tests;

#[cfg(test)]
mod docs_tests;

#[cfg(test)]
mod bulk_tests;

#[cfg(test)]
mod by_query_tests;

#[cfg(test)]
mod search_tests;

#[cfg(test)]
mod msearch_tests;

#[cfg(test)]
mod search_template_tests;

#[cfg(test)]
mod scroll_tests;

#[cfg(test)]
mod aliases_tests;

#[cfg(test)]
mod field_caps_tests;

#[cfg(test)]
mod tests {
    use super::*;

    /// Waits until `n` callers wait for the group's next round.
    fn joined(group: &Group, n: usize) {
        while Arc::strong_count(&group.rounds.lock().unwrap().next) < n + 1 {
            std::thread::yield_now();
        }
    }

    /// Writers that waited for a successful round succeed even when a later round fails before
    /// they wake.
    #[test]
    fn write_failure_handling_rounds_report_their_own_result() {
        let group = Arc::new(Group::default());
        let (started, running) = std::sync::mpsc::channel();
        let (release, blocked) = std::sync::mpsc::channel::<()>();
        let first = {
            let g = group.clone();
            std::thread::spawn(move || {
                g.persist(|| {
                    started.send(()).unwrap();
                    blocked.recv().unwrap();
                    Ok(())
                })
            })
        };
        running.recv().unwrap();
        // Round 2's writers; the one running it lets round 3's writers arrive, then succeeds.
        let later = Arc::new(std::sync::Mutex::new(vec![]));
        let second: Vec<_> = (0..32)
            .map(|_| {
                let (g, later) = (group.clone(), later.clone());
                std::thread::spawn(move || {
                    g.persist(|| {
                        for _ in 0..4 {
                            let g2 = g.clone();
                            let h = std::thread::spawn(move || g2.persist(|| Err("full".into())));
                            later.lock().unwrap().push(h);
                        }
                        joined(&g, 4);
                        Ok(())
                    })
                })
            })
            .collect();
        joined(&group, 32);
        release.send(()).unwrap();
        assert_eq!(first.join().unwrap(), Ok(()));
        for h in second {
            assert_eq!(h.join().unwrap(), Ok(()));
        }
        for h in std::mem::take(&mut *later.lock().unwrap()) {
            assert_eq!(h.join().unwrap(), Err("full".into()));
        }
    }

    #[tokio::test]
    async fn serves_info_and_health_over_http() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(axum::serve(listener, app()).into_future());

        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let req = "GET /_cluster/health/nope HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n";
        tokio::io::AsyncWriteExt::write_all(&mut stream, req.as_bytes())
            .await
            .unwrap();
        let mut resp = String::new();
        tokio::io::AsyncReadExt::read_to_string(&mut stream, &mut resp)
            .await
            .unwrap();
        assert!(resp.starts_with("HTTP/1.1 408"), "{resp}");
        assert!(resp.contains("\"timed_out\":true"), "{resp}");

        for (path, key) in [("/", "tagline"), ("/_cluster/health", "status")] {
            let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
            let req = format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
            tokio::io::AsyncWriteExt::write_all(&mut stream, req.as_bytes())
                .await
                .unwrap();
            let mut resp = String::new();
            tokio::io::AsyncReadExt::read_to_string(&mut stream, &mut resp)
                .await
                .unwrap();
            assert!(resp.starts_with("HTTP/1.1 200"), "{path}: {resp}");
            assert!(resp.contains(&format!("\"{key}\"")), "{path}: {resp}");
        }
    }
}
