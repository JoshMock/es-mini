//! Writes through one server over a data directory, "restarts" on the same directory, reads back.

use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

async fn call(app: &axum::Router, method: &str, uri: &str, body: Value) -> Value {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

#[tokio::test]
async fn documents_survive_restart() {
    let dir = std::env::temp_dir().join(format!("esm-persist-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let app = es_mini::app_persisted(&dir).unwrap();
    call(
        &app,
        "PUT",
        "/books",
        json!({"mappings": {"properties": {"n": {"type": "long"}}}}),
    )
    .await;
    call(
        &app,
        "PUT",
        "/books/_doc/a%2Fb",
        json!({"title": "Dune", "n": 1}),
    )
    .await;
    call(
        &app,
        "PUT",
        "/books/_doc/2",
        json!({"title": "Emma", "n": 2}),
    )
    .await;
    call(
        &app,
        "PUT",
        "/books/_doc/2",
        json!({"title": "Emma 2", "n": 3}),
    )
    .await;
    call(&app, "DELETE", "/books/_doc/zzz", json!({})).await;
    call(&app, "PUT", "/empty", json!({})).await;
    drop(app);

    assert!(dir.join("_state").is_dir());
    let app = es_mini::app_persisted(&dir).unwrap();
    let doc = call(&app, "GET", "/books/_doc/a%2Fb", json!({})).await;
    assert_eq!(doc["_source"], json!({"title": "Dune", "n": 1}));
    let doc = call(&app, "GET", "/books/_doc/2", json!({})).await;
    assert_eq!(doc["_source"], json!({"title": "Emma 2", "n": 3}));
    assert_eq!(doc["_version"], 2);
    let hits = call(
        &app,
        "POST",
        "/books/_search",
        json!({"query": {"term": {"title.keyword": "Dune"}}}),
    )
    .await;
    assert_eq!(hits["hits"]["total"]["value"], 1);
    let m = call(&app, "GET", "/books/_mapping", json!({})).await;
    assert_eq!(m["books"]["mappings"]["properties"]["n"]["type"], "long");
    assert_eq!(call(&app, "HEAD", "/empty", json!({})).await, Value::Null);
    let _ = std::fs::remove_dir_all(&dir);
}
