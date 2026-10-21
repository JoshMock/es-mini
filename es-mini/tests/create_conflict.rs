use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

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

#[tokio::test]
async fn failed_create_still_maps_new_fields() {
    // Elasticsearch 9.5.5 applies the dynamic mapping update before the version conflict.
    let app = es_mini::app();
    call(&app, "PUT", "/c/_doc/1", json!({"a": 1})).await;
    let (s, _) = call(&app, "PUT", "/c/_create/1", json!({"a": 1, "b": "x"})).await;
    assert_eq!(s, 409);
    let (_, m) = call(&app, "GET", "/c/_mapping", json!({})).await;
    assert_eq!(m["c"]["mappings"]["properties"]["b"]["type"], "text");
}
