//! Responses report each index's real shards: `_shards` headers, per-shard failures (grouped
//! by index and reason, as Elasticsearch's ExceptionsHelper.groupBy does), profile shard ids,
//! and cluster health.

use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

const NODE: &str = "es-mini-node";

async fn call(app: &axum::Router, method: &str, uri: &str, body: Value) -> (u16, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status().as_u16();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// `a` (3 shards) and `b` (1 shard), each with a document in every shard and field `f` mapped
/// as `a_type` and `b_type`.
async fn setup(a_type: &str, b_type: &str) -> axum::Router {
    let app = es_mini::app();
    for (index, shards, ty) in [("a", 3, a_type), ("b", 1, b_type)] {
        let body = json!({
            "settings": {"number_of_shards": shards},
            "mappings": {"properties": {"f": {"type": ty}}},
        });
        call(&app, "PUT", &format!("/{index}"), body).await;
        for id in 0..6 {
            let uri = format!("/{index}/_doc/{id}?refresh=true");
            call(&app, "PUT", &uri, json!({"f": "127.0.0.1"})).await;
        }
    }
    app
}

fn failure(index: &str, reason: &Value) -> Value {
    json!({"shard": 0, "index": index, "node": NODE, "reason": reason})
}

#[tokio::test]
async fn search_and_count_total_every_shard() {
    let app = setup("keyword", "keyword").await;
    let (_, r) = call(&app, "POST", "/a,b/_search", json!({})).await;
    let all = json!({"total": 4, "successful": 4, "skipped": 0, "failed": 0});
    assert_eq!(r["_shards"], all);
    let (_, r) = call(&app, "POST", "/a,b/_count", json!({})).await;
    assert_eq!(r["count"], 12);
    assert_eq!(r["_shards"], all);
}

#[tokio::test]
async fn failed_shards_are_counted_and_grouped_by_index() {
    let app = setup("keyword", "ip").await;
    let aggs = json!({"aggs": {"t": {"terms": {"field": "f", "value_type": "ip"}}}});
    let (s, r) = call(&app, "POST", "/a,b/_search", aggs.clone()).await;
    assert_eq!(s, 200, "{r}");
    let reason = json!({"type": "illegal_argument_exception", "reason": "Field type [keyword] is incompatible with specified value_type [ip]"});
    assert_eq!(
        r["_shards"],
        json!({"total": 4, "successful": 1, "skipped": 0, "failed": 3, "failures": [failure("a", &reason)]})
    );
    assert_eq!(r["hits"]["total"]["value"], 6);

    let (s, r) = call(&app, "POST", "/a/_search", aggs).await;
    assert_eq!(s, 400, "{r}");
    assert_eq!(r["error"]["root_cause"], json!([reason]));
    assert_eq!(r["error"]["failed_shards"], json!([failure("a", &reason)]));
}

#[tokio::test]
async fn profile_names_each_shard() {
    let app = setup("keyword", "keyword").await;
    let body = json!({"profile": true, "size": 0, "aggs": {"t": {"terms": {"field": "f"}}}});
    let (_, r) = call(&app, "POST", "/a,b/_search", body).await;
    let node = NODE;
    let ids: Vec<_> = (r["profile"]["shards"].as_array().unwrap().iter())
        .map(|s| (s["id"].clone(), s["index"].clone(), s["shard_id"].clone()))
        .collect();
    let id = |i: &str, n: u32| (json!(format!("[{node}][{i}][{n}]")), json!(i), json!(n));
    assert_eq!(ids, [id("a", 0), id("a", 1), id("a", 2), id("b", 0)]);
}

#[tokio::test]
async fn health_counts_every_shard() {
    let app = setup("keyword", "keyword").await;
    let (_, r) = call(&app, "GET", "/_cluster/health?level=shards", Value::Null).await;
    assert_eq!(r["active_primary_shards"], 4);
    assert_eq!(r["active_shards"], 4);
    let a = &r["indices"]["a"];
    assert_eq!(a["number_of_shards"], 3);
    assert_eq!(a["active_primary_shards"], 3);
    assert_eq!(a["active_shards"], 3);
    let shards: Vec<_> = a["shards"].as_object().unwrap().keys().cloned().collect();
    assert_eq!(shards, ["0", "1", "2"]);
    assert_eq!(a["shards"]["2"]["primary_active"], true);
    assert_eq!(r["indices"]["b"]["number_of_shards"], 1);
    let (_, r) = call(&app, "GET", "/_cluster/health/a", Value::Null).await;
    assert_eq!(r["active_primary_shards"], 3);
}

#[tokio::test]
async fn broadcasts_count_every_shard() {
    let app = setup("keyword", "keyword").await;
    let all = json!({"total": 4, "successful": 4, "failed": 0});
    for (method, uri) in [
        ("POST", "/a,b/_refresh"),
        ("POST", "/a,b/_flush"),
        ("GET", "/a,b/_stats"),
    ] {
        let (_, r) = call(&app, method, uri, Value::Null).await;
        assert_eq!(r["_shards"], all, "{uri}");
    }
}

#[tokio::test]
async fn stats_list_every_shard_at_level_shards() {
    let app = setup("keyword", "keyword").await;
    let (_, r) = call(&app, "GET", "/a/_stats/docs?level=shards", Value::Null).await;
    let shards = r["indices"]["a"]["shards"].as_object().unwrap();
    assert_eq!(shards.keys().collect::<Vec<_>>(), ["0", "1", "2"]);
    let mut docs = 0;
    for copies in shards.values() {
        let [copy] = copies.as_array().unwrap().as_slice() else {
            panic!("{copies}")
        };
        let routing =
            json!({"state": "STARTED", "primary": true, "node": NODE, "relocating_node": null});
        assert_eq!(copy["routing"], routing);
        docs += copy["docs"]["count"].as_u64().unwrap();
    }
    assert_eq!(docs, 6);
    let (_, r) = call(&app, "GET", "/a/_stats", Value::Null).await;
    assert!(r["indices"]["a"].get("shards").is_none(), "{r}");
    let (_, r) = call(&app, "GET", "/a/_stats?level=cluster", Value::Null).await;
    assert!(r.get("indices").is_none(), "{r}");
}

#[tokio::test]
async fn level_must_be_known() {
    let app = setup("keyword", "keyword").await;
    let (s, r) = call(&app, "GET", "/a/_stats?level=bogus", Value::Null).await;
    assert_eq!(s, 400);
    let reason =
        "level parameter must be one of [cluster] or [indices] or [shards] but was [bogus]";
    assert_eq!(r["error"]["reason"], reason);
}
