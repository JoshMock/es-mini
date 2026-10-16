mod bulk;
mod docs;
mod error;
mod indices;
mod search;

use axum::{
    Json, Router,
    routing::{get, post, put},
};
use es_index::Indices;
use serde_json::{Value, json};
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

async fn health() -> Json<Value> {
    Json(json!({
        "cluster_name": CLUSTER_NAME,
        "status": "green",
        "timed_out": false,
        "number_of_nodes": 1,
        "number_of_data_nodes": 1,
        "active_primary_shards": 0,
        "active_shards": 0,
        "relocating_shards": 0,
        "initializing_shards": 0,
        "unassigned_shards": 0,
        "unassigned_primary_shards": 0,
        "delayed_unassigned_shards": 0,
        "number_of_pending_tasks": 0,
        "number_of_in_flight_fetch": 0,
        "task_max_waiting_in_queue_millis": 0,
        "active_shards_percent_as_number": 100.0
    }))
}

fn app() -> Router {
    Router::new()
        .route("/", get(info))
        .route("/_cluster/health", get(health))
        .route(
            "/{index}",
            put(indices::create)
                .get(indices::get)
                .head(indices::head)
                .delete(indices::delete),
        )
        .route(
            "/{index}/_settings",
            put(indices::put_settings).get(indices::get_settings),
        )
        .route("/{index}/_mapping", get(indices::get_mapping))
        .route("/_bulk", post(bulk::bulk).put(bulk::bulk))
        .route(
            "/{index}/_bulk",
            post(bulk::bulk_index).put(bulk::bulk_index),
        )
        .route("/{index}/_search", get(search::search).post(search::search))
        .route("/{index}/_count", get(search::count).post(search::count))
        .route("/{index}/_doc", post(docs::index_auto_id))
        .route(
            "/{index}/_doc/{id}",
            put(docs::index_with_id)
                .post(docs::index_with_id)
                .get(docs::get)
                .delete(docs::delete),
        )
        .route(
            "/{index}/_create/{id}",
            put(docs::create).post(docs::create),
        )
        .route("/{index}/_update/{id}", post(docs::update))
        .with_state(AppState::default())
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let port = std::env::var("ES_MINI_PORT").unwrap_or_else(|_| "9200".into());
    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{port}"))
        .await
        .expect("bind 127.0.0.1");
    tracing::info!("listening on {}", listener.local_addr().unwrap());
    axum::serve(listener, app()).await.unwrap();
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
mod tests {
    use super::*;

    #[tokio::test]
    async fn serves_info_and_health_over_http() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(axum::serve(listener, app()).into_future());

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
