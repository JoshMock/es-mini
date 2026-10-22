//! Single-node stubs for nodes.info, nodes.stats, cluster.stats and cluster.get_settings.

use crate::{AppState, CLUSTER_NAME};
use axum::{Json, extract::State};
use serde_json::{Value, json};

pub(crate) const NODE_ID: &str = "es-mini-node";

fn nodes_header() -> Value {
    json!({"total": 1, "successful": 1, "failed": 0})
}

fn doc_count(state: &AppState) -> (usize, usize) {
    let store = state.read().expect("lock poisoned");
    (store.len(), store.values().map(|i| i.docs().count()).sum())
}

pub async fn info() -> Json<Value> {
    Json(json!({
        "_nodes": nodes_header(),
        "cluster_name": CLUSTER_NAME,
        "nodes": {NODE_ID: {
            "name": CLUSTER_NAME,
            "transport_address": "127.0.0.1:9300",
            "host": "127.0.0.1",
            "ip": "127.0.0.1",
            "version": "9.5.5",
            "transport_version": 0,
            "build_flavor": "default",
            "build_type": "es-mini",
            "build_hash": "unknown",
            "roles": ["data", "ingest", "master"],
            "attributes": {},
            "settings": {"cluster": {"name": CLUSTER_NAME}, "node": {"name": CLUSTER_NAME}},
            "http": {"bound_address": ["127.0.0.1:9200"], "publish_address": "127.0.0.1:9200"}
        }}
    }))
}

pub async fn stats(State(state): State<AppState>) -> Json<Value> {
    let (_, docs) = doc_count(&state);
    Json(json!({
        "_nodes": nodes_header(),
        "cluster_name": CLUSTER_NAME,
        "nodes": {NODE_ID: {
            "name": CLUSTER_NAME,
            "transport_address": "127.0.0.1:9300",
            "host": "127.0.0.1",
            "ip": "127.0.0.1",
            "roles": ["data", "ingest", "master"],
            "indices": {"docs": {"count": docs, "deleted": 0}}
        }}
    }))
}

pub async fn cluster_stats(State(state): State<AppState>) -> Json<Value> {
    let (indices, docs) = doc_count(&state);
    Json(json!({
        "_nodes": nodes_header(),
        "cluster_name": CLUSTER_NAME,
        "cluster_uuid": "_na_",
        "status": "green",
        "indices": {
            "count": indices,
            "shards": {"total": indices, "primaries": indices, "replication": 0.0},
            "docs": {"count": docs, "deleted": 0}
        },
        "nodes": {
            "count": {"total": 1, "data": 1, "master": 1, "ingest": 1},
            "versions": ["9.5.5"]
        }
    }))
}

pub async fn cluster_settings() -> Json<Value> {
    Json(json!({"persistent": {}, "transient": {}}))
}
