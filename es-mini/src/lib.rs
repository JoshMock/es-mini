//! es-mini: an Elasticsearch-compatible HTTP server.

mod aliases;
mod bulk;
mod docs;
mod error;
mod field_caps;
mod filter_path;
mod indices;
mod params;
mod search;

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{StatusCode, Uri},
    response::IntoResponse,
    routing::{get, post, put},
};
use error::AppError;
use es_index::Indices;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

// ponytail: one global lock over all indices; per-index locks if write contention shows up
type AppState = Arc<RwLock<Indices>>;

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
    let store = state.read().expect("lock poisoned");
    let names: Vec<String> = match &index {
        None => store.keys().cloned().collect(),
        Some(Path(expr)) => match es_index::aliases::resolve_read(&store, expr) {
            Ok(targets) => targets.into_iter().map(|t| t.index).collect(),
            Err(_) => {
                let mut body = health_body(0);
                body["status"] = json!("red");
                body["timed_out"] = json!(true);
                return Ok((StatusCode::REQUEST_TIMEOUT, Json(body)));
            }
        },
    };
    let level = params.get("level").map(String::as_str);
    let mut body = health_body(names.len());
    if matches!(level, Some("indices" | "shards")) {
        let shards = level == Some("shards");
        let indices: serde_json::Map<String, Value> = names
            .iter()
            .map(|name| (name.clone(), index_health(shards)))
            .collect();
        body["indices"] = Value::Object(indices);
    }
    Ok((StatusCode::OK, Json(body)))
}

fn health_body(shards: usize) -> Value {
    json!({
        "cluster_name": CLUSTER_NAME,
        "status": "green",
        "timed_out": false,
        "number_of_nodes": 1,
        "number_of_data_nodes": 1,
        "active_primary_shards": shards,
        "active_shards": shards,
        "relocating_shards": 0,
        "initializing_shards": 0,
        "unassigned_shards": 0,
        "unassigned_primary_shards": 0,
        "delayed_unassigned_shards": 0,
        "number_of_pending_tasks": 0,
        "number_of_in_flight_fetch": 0,
        "task_max_waiting_in_queue_millis": 0,
        "active_shards_percent_as_number": 100.0
    })
}

// ponytail: every index is one green primary shard, no replicas
fn index_health(shards: bool) -> Value {
    let mut h = json!({
        "status": "green",
        "number_of_shards": 1,
        "number_of_replicas": 0,
        "active_primary_shards": 1,
        "active_shards": 1,
        "relocating_shards": 0,
        "initializing_shards": 0,
        "unassigned_shards": 0,
        "unassigned_primary_shards": 0
    });
    if shards {
        h["shards"] = json!({"0": {
            "status": "green",
            "primary_active": true,
            "active_shards": 1,
            "relocating_shards": 0,
            "initializing_shards": 0,
            "unassigned_shards": 0,
            "unassigned_primary_shards": 0
        }});
    }
    h
}

/// The full HTTP API over a fresh, empty in-memory store.
pub fn app() -> Router {
    app_with(Indices::new())
}

/// The full HTTP API over `indices`, e.g. ones opened with [`es_index::disk::open`].
pub fn app_with(indices: Indices) -> Router {
    with_es_405(router(Arc::new(RwLock::new(indices))))
}

/// The full HTTP API over the data directory `dir` (created if missing): its indices are loaded
/// now (replaying their translogs), and every successful write request is made durable before
/// it is answered (see [`es_index::disk::persist`]).
pub fn app_persisted(dir: &std::path::Path) -> Result<Router, String> {
    es_index::disk::recover(dir).map_err(|e| e.0)?;
    let indices = if dir.join("_state").is_dir() {
        es_index::disk::open(dir).map_err(|e| e.0)?
    } else {
        Indices::new()
    };
    let state = Arc::new(RwLock::new(indices));
    let (dir, saved) = (dir.to_owned(), state.clone());
    Ok(with_es_405(router(state).layer(axum::middleware::from_fn(
        move |req: axum::extract::Request, next: axum::middleware::Next| {
            let (dir, state) = (dir.clone(), saved.clone());
            async move {
                let route = req.extensions().get::<axum::extract::MatchedPath>();
                let reads = req.method().is_safe()
                    || route.is_some_and(|r| {
                        ["/_search", "/_count", "/_query", "/_field_caps"]
                            .iter()
                            .any(|s| r.as_str().ends_with(s))
                    });
                let res = next.run(req).await;
                if reads || !res.status().is_success() {
                    return res;
                }
                let saved =
                    es_index::disk::persist(&dir, &mut state.write().expect("lock poisoned"));
                match saved {
                    Ok(()) => res,
                    // ponytail: the change stays applied in memory; a later successful save persists it
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
    ))))
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
    let out = serde_json::to_vec(&filter_path::apply(&v, &f)).unwrap_or_default();
    parts.headers.remove("content-length");
    (parts, out).into_response()
}

fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(info))
        .route("/_cluster/health", get(health))
        .route("/_cluster/health/{index}", get(health))
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
        .route("/{index}/_mapping", get(indices::get_mapping))
        .route("/_mapping", get(indices::get_all_mappings))
        .route("/_query", post(search::esql))
        .route("/_bulk", post(bulk::bulk).put(bulk::bulk))
        .route(
            "/{index}/_bulk",
            post(bulk::bulk_index).put(bulk::bulk_index),
        )
        .route("/_search", get(search::search_all).post(search::search_all))
        .route("/{index}/_search", get(search::search).post(search::search))
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
mod search_tests;

#[cfg(test)]
mod msearch_tests;

#[cfg(test)]
mod aliases_tests;

#[cfg(test)]
mod field_caps_tests;

#[cfg(test)]
mod tests {
    use super::*;

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
