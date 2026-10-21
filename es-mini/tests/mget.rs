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
async fn head_and_mget() {
    let app = es_mini::app();
    call(&app, "PUT", "/m/_doc/1", json!({"a": 1, "b": 2})).await;
    assert_eq!(call(&app, "HEAD", "/m/_doc/1", json!({})).await.0, 200);
    assert_eq!(call(&app, "HEAD", "/m/_doc/9", json!({})).await.0, 404);
    let (s, r) = call(
        &app,
        "POST",
        "/m/_mget?_source=a",
        json!({"ids": ["1", "9"]}),
    )
    .await;
    assert_eq!(s, 200);
    assert_eq!(r["docs"][0]["_source"], json!({"a": 1}));
    assert_eq!(r["docs"][0]["found"], true);
    assert_eq!(r["docs"][1]["found"], false);
    let (_, r) = call(
        &app,
        "POST",
        "/_mget",
        json!({"docs": [{"_index": "m", "_id": "1"}]}),
    )
    .await;
    assert_eq!(r["docs"][0]["_source"], json!({"a": 1, "b": 2}));
}
