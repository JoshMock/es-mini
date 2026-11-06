//! The cluster's node list, `_cat/nodes`, and single-node stubs for nodes.info, nodes.stats,
//! cluster.stats and cluster.get_settings.

use crate::{AppState, CLUSTER_NAME, error::AppError};
use axum::{
    Json,
    extract::{Query, State},
    http::Uri,
};
use serde_json::{Value, json};
use std::collections::HashMap;

pub(crate) const NODE_ID: &str = "es-mini-node";

/// The cluster's nodes, fixed at startup by the `es_mini.nodes` setting (comma-separated
/// `host:port`, listing this node's own `es_mini.node` too); without it, this node alone.
/// Node 0 acts as the elected master; there is no election.
#[derive(Default)]
pub(crate) struct Nodes {
    addrs: Vec<String>,
}

impl Nodes {
    pub fn from_settings(settings: &HashMap<String, String>) -> Result<Self, String> {
        let Some(list) = settings.get("es_mini.nodes") else {
            return Ok(Self::default());
        };
        let addrs: Vec<String> = list.split(',').map(|a| a.trim().to_owned()).collect();
        let me = settings.get("es_mini.node").map_or("", String::as_str);
        if !addrs.iter().any(|a| a == me) {
            return Err(format!(
                "es_mini.nodes [{list}] does not list this node [{me}]"
            ));
        }
        Ok(Self { addrs })
    }

    pub fn count(&self) -> usize {
        self.addrs.len().max(1)
    }

    /// Node `node`'s name; one node alone keeps the cluster's name.
    pub fn name(&self, node: usize) -> String {
        match self.addrs.is_empty() {
            true => CLUSTER_NAME.into(),
            false => format!("{CLUSTER_NAME}-{node}"),
        }
    }

    /// Node `node`'s IP address.
    fn ip(&self, node: usize) -> &str {
        (self.addrs.get(node))
            .and_then(|a| a.rsplit_once(':'))
            .map_or("127.0.0.1", |(host, _)| host)
    }
}

/// `GET /_cat/nodes`: one row per node, with zero resource use.
pub async fn cat(
    State(state): State<AppState>,
    uri: Uri,
    Query(params): Query<HashMap<String, String>>,
) -> Result<axum::response::Response, AppError> {
    crate::params::check(&uri, &params, &["format", "v", "h", "s", "help"])?;
    // RestNodesAction's default columns
    const COLUMNS: [(&str, &str, &str); 10] = [
        ("ip", "i", "ip address"),
        ("heap.percent", "hp,heapPercent", "used heap ratio"),
        ("ram.percent", "rp,ramPercent", "used machine memory ratio"),
        ("cpu", "", "recent cpu usage"),
        ("load_1m", "l", "1m load avg"),
        ("load_5m", "l", "5m load avg"),
        ("load_15m", "l", "15m load avg"),
        (
            "node.role",
            "r,role,nodeRole",
            "m:master eligible node, d:data node, i:ingest node, -:coordinating node only",
        ),
        ("master", "m", "*:current master"),
        ("name", "n", "node name"),
    ];
    let nodes = state.nodes();
    let rows = (0..nodes.count())
        .map(|n| {
            let master = if n == 0 { "*" } else { "-" };
            [
                nodes.ip(n),
                "0",
                "0",
                "0",
                "0.00",
                "0.00",
                "0.00",
                "dim",
                master,
            ]
            .map(str::to_owned)
            .into_iter()
            .chain([nodes.name(n)])
            .collect()
        })
        .collect();
    Ok(crate::indices::cat_view(
        &COLUMNS,
        rows,
        |c| (1..=6).contains(&c),
        |_| true,
        &params,
    ))
}

fn nodes_header() -> Value {
    json!({"total": 1, "successful": 1, "failed": 0})
}

fn doc_count(state: &AppState) -> (usize, usize) {
    let shared = state.read();
    let store = shared.all();
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
