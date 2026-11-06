//! The cluster's node list, `_cat/nodes`, and single-node stubs for nodes.info, nodes.stats,
//! cluster.stats and cluster.get_settings.

use crate::{AppState, CLUSTER_NAME, error::AppError};
use axum::{
    Json,
    extract::{FromRequestParts, MatchedPath, Path, Query, Request, State},
    http::{HeaderMap, Method, StatusCode, Uri, header::CONTENT_TYPE},
    middleware::Next,
    response::{IntoResponse, Response},
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
    /// This node's place in `addrs`.
    me: usize,
}

impl Nodes {
    pub fn from_settings(settings: &HashMap<String, String>) -> Result<Self, String> {
        let Some(list) = settings.get("es_mini.nodes") else {
            return Ok(Self::default());
        };
        let addrs: Vec<String> = list.split(',').map(|a| a.trim().to_owned()).collect();
        let me = settings.get("es_mini.node").map_or("", String::as_str);
        let me = (addrs.iter().position(|a| a == me))
            .ok_or_else(|| format!("es_mini.nodes [{list}] does not list this node [{me}]"))?;
        Ok(Self { addrs, me })
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

    /// Node `node`'s id; one node alone keeps [`NODE_ID`].
    pub fn id(&self, node: usize) -> String {
        match self.addrs.is_empty() {
            true => NODE_ID.into(),
            false => format!("{NODE_ID}-{node}"),
        }
    }

    /// The node that holds shard `shard` of every index: shards go round the node list.
    pub fn owner(&self, shard: usize) -> usize {
        shard % self.count()
    }

    /// Node `node`'s IP address.
    pub fn ip(&self, node: usize) -> &str {
        (self.addrs.get(node))
            .and_then(|a| a.rsplit_once(':'))
            .map_or("127.0.0.1", |(host, _)| host)
    }
}

/// Marks a request one node sends another, so the receiver applies it alone.
const PEER: &str = "x-es-mini-peer";

/// Sends `method uri` with `body` and the client's `content-type` and `authorization` from
/// `headers` to the node at `addr`, marked as from a [`PEER`]; returns the answer, keeping only
/// its status, `content-type` and body.
async fn send(
    addr: &str,
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
    body: &[u8],
) -> std::io::Result<Response> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut head = format!("{method} {uri} HTTP/1.0\r\n{PEER}: 1\r\n");
    for name in ["content-type", "authorization"] {
        if let Some(v) = headers.get(name).and_then(|v| v.to_str().ok()) {
            head += &format!("{name}: {v}\r\n");
        }
    }
    head += &format!("content-length: {}\r\n\r\n", body.len());
    let mut s = tokio::net::TcpStream::connect(addr).await?;
    s.write_all(head.as_bytes()).await?;
    s.write_all(body).await?;
    // HTTP/1.0: the server closes the connection after answering.
    let mut res = Vec::new();
    s.read_to_end(&mut res).await?;
    let malformed = || std::io::Error::other("malformed HTTP response");
    let split = (res.windows(4).position(|w| w == b"\r\n\r\n")).ok_or_else(malformed)?;
    let head = String::from_utf8_lossy(&res[..split]);
    let mut lines = head.split("\r\n");
    let status = (lines.next().and_then(|l| l.split(' ').nth(1)))
        .and_then(|code| StatusCode::from_bytes(code.as_bytes()).ok())
        .ok_or_else(malformed)?;
    let mut answer = (status, res[split + 4..].to_vec()).into_response();
    for (name, value) in lines.filter_map(|l| l.split_once(':')) {
        if name.eq_ignore_ascii_case("content-type")
            && let Ok(value) = value.trim().parse()
        {
            answer.headers_mut().insert(CONTENT_TYPE, value);
        }
    }
    Ok(answer)
}

/// Sends a single-document request to the node that holds the document's shard, as
/// Elasticsearch routes it to the primary, and answers with that node's response. Requests whose
/// index or routing does not resolve here run here, to fail as on one node.
// ponytail: POST /{index}/_doc (auto id) is not forwarded, so its doc stays on this node
pub async fn forward(State(state): State<AppState>, req: Request, next: Next) -> Response {
    const ROUTES: [&str; 4] = [
        "/{index}/_doc/{id}",
        "/{index}/_create/{id}",
        "/{index}/_update/{id}",
        "/{index}/_source/{id}",
    ];
    let route = (req.extensions().get::<MatchedPath>()).map(MatchedPath::as_str);
    if state.nodes().addrs.len() < 2
        || !route.is_some_and(|r| ROUTES.contains(&r))
        || req.headers().contains_key(PEER)
    {
        return next.run(req).await;
    }
    let (mut parts, body) = req.into_parts();
    let owner = match Path::<(String, String)>::from_request_parts(&mut parts, &state).await {
        Ok(Path((index, id))) => doc_owner(&state, &parts.method, &parts.uri, &index, &id),
        Err(_) => None,
    };
    let nodes = state.nodes();
    let Some(owner) = owner.filter(|&o| o != nodes.me) else {
        return next.run(Request::from_parts(parts, body)).await;
    };
    let Ok(body) = axum::body::to_bytes(body, usize::MAX).await else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let sent = send(
        &nodes.addrs[owner],
        &parts.method,
        &parts.uri,
        &parts.headers,
        &body,
    );
    sent.await.unwrap_or_else(|e| {
        let node = nodes.name(owner);
        AppError::Io(format!("node [{node}] did not answer: {e}")).into_response()
    })
}

/// The node holding document `id` that a request `method uri` sent to `index` reaches, if the
/// index and the routing resolve.
fn doc_owner(state: &AppState, method: &Method, uri: &Uri, index: &str, id: &str) -> Option<usize> {
    let shared = state.read();
    let target = match method.is_safe() {
        true => crate::docs::single_index(&*shared, index).ok(),
        false => es_index::aliases::resolve_write(&*shared, index).ok(),
    };
    let i = shared.read(&target?)?;
    let params = Query::<HashMap<String, String>>::try_from_uri(uri).ok()?;
    let routing = params.get("routing").map(String::as_str);
    let routing = crate::docs::routing(&i, index, routing).ok()??;
    Some(state.nodes().owner(i.shard_for(id, routing)))
}

/// Repeats a successful index creation or deletion on every other node, as Elasticsearch's
/// master publishes the cluster state, before answering.
// ponytail: each node records its own uuid and creation date for the index
pub async fn broadcast(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map(MatchedPath::as_str);
    let metadata =
        route == Some("/{index}") && [Method::PUT, Method::DELETE].contains(req.method());
    if state.nodes().addrs.len() < 2 || !metadata || req.headers().contains_key(PEER) {
        return next.run(req).await;
    }
    let (parts, body) = req.into_parts();
    let (method, uri, headers) = (
        parts.method.clone(),
        parts.uri.clone(),
        parts.headers.clone(),
    );
    let Ok(body) = axum::body::to_bytes(body, usize::MAX).await else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let res = next
        .run(Request::from_parts(parts, body.clone().into()))
        .await;
    if !res.status().is_success() {
        return res;
    }
    let nodes = state.nodes();
    for (n, addr) in nodes
        .addrs
        .iter()
        .enumerate()
        .filter(|(n, _)| *n != nodes.me)
    {
        match send(addr, &method, &uri, &headers, &body).await {
            Ok(res) if res.status().is_success() => {}
            failed => {
                let why = failed.map_or_else(|e| e.to_string(), |r| r.status().to_string());
                return AppError::Io(format!(
                    "applied on this node, but node [{}] answered: {why}",
                    nodes.name(n)
                ))
                .into_response();
            }
        }
    }
    res
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
