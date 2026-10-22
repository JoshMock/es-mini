//! Single-node stubs: nodes.info, nodes.stats, cluster.stats, cluster.get_settings.

use axum::{Router, body::Body, http::Request};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

async fn call(app: &Router, method: &str, uri: &str, body: &str) -> (u16, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_owned()))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status().as_u16();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn node_info_lists_the_single_node() {
    let app = es_mini::app();
    for uri in ["/_nodes", "/_nodes/_local", "/_nodes/http"] {
        let (status, body) = call(&app, "GET", uri, "").await;
        assert_eq!(status, 200, "{uri}");
        assert_eq!(
            body["_nodes"],
            json!({"total": 1, "successful": 1, "failed": 0})
        );
        assert_eq!(body["cluster_name"], "es-mini");
        let nodes = body["nodes"].as_object().unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes.values().next().unwrap()["version"], "9.5.5");
    }
}

#[tokio::test]
async fn node_info_stats_and_cluster_stats_count_real_data() {
    let app = es_mini::app();
    call(&app, "PUT", "/a/_doc/1?refresh=true", "{}").await;
    call(&app, "PUT", "/b/_doc/1?refresh=true", "{}").await;
    call(&app, "PUT", "/b/_doc/2?refresh=true", "{}").await;
    let (status, body) = call(&app, "GET", "/_cluster/stats", "").await;
    assert_eq!(status, 200);
    assert_eq!(body["indices"]["count"], 2);
    assert_eq!(body["indices"]["docs"]["count"], 3);
    assert_eq!(body["nodes"]["count"]["total"], 1);
    let (status, body) = call(&app, "GET", "/_nodes/stats", "").await;
    assert_eq!(status, 200);
    let node = body["nodes"]
        .as_object()
        .unwrap()
        .values()
        .next()
        .unwrap()
        .clone();
    assert_eq!(node["indices"]["docs"]["count"], 3);
}

#[tokio::test]
async fn node_info_cluster_settings_are_empty() {
    let (status, body) = call(&es_mini::app(), "GET", "/_cluster/settings", "").await;
    assert_eq!(status, 200);
    assert_eq!(body, json!({"persistent": {}, "transient": {}}));
}
