use crate::app;
use axum::{
    Router,
    body::Body,
    http::{Method, Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

async fn call(app: &Router, method: Method, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let body = body.map_or(Body::empty(), |b| Body::from(b.to_string()));
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(body)
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, json)
}

#[tokio::test]
async fn create_get_head_delete() {
    let app = app();
    let body = json!({"settings": {"number_of_shards": 2}, "mappings": {"properties": {"t": {"type": "text"}}}});
    let (s, r) = call(&app, Method::PUT, "/idx", Some(body)).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        r,
        json!({"acknowledged": true, "shards_acknowledged": true, "index": "idx"})
    );

    let (s, r) = call(&app, Method::GET, "/idx", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        r["idx"]["mappings"],
        json!({"properties": {"t": {"type": "text"}}})
    );
    assert_eq!(r["idx"]["settings"]["index"]["number_of_shards"], "2");
    assert_eq!(r["idx"]["aliases"], json!({}));

    assert_eq!(
        call(&app, Method::HEAD, "/idx", None).await.0,
        StatusCode::OK
    );

    let (s, r) = call(&app, Method::DELETE, "/idx", None).await;
    assert_eq!((s, r), (StatusCode::OK, json!({"acknowledged": true})));
    assert_eq!(
        call(&app, Method::HEAD, "/idx", None).await,
        (StatusCode::NOT_FOUND, Value::Null)
    );
}

#[tokio::test]
async fn create_without_body() {
    let (s, _) = call(&app(), Method::PUT, "/idx", None).await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test]
async fn create_existing_is_400() {
    let app = app();
    call(&app, Method::PUT, "/idx", None).await;
    let (s, r) = call(&app, Method::PUT, "/idx", None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(r["status"], 400);
    assert_eq!(r["error"]["type"], "resource_already_exists_exception");
    assert_eq!(
        r["error"]["root_cause"][0]["type"],
        "resource_already_exists_exception"
    );
    assert_eq!(r["error"]["index"], "idx");
}

#[tokio::test]
async fn missing_index_is_404_everywhere() {
    let app = app();
    for (m, uri) in [
        (Method::GET, "/nope"),
        (Method::DELETE, "/nope"),
        (Method::GET, "/nope/_settings"),
        (Method::PUT, "/nope/_settings"),
        (Method::GET, "/nope/_mapping"),
    ] {
        let (s, r) = call(&app, m, uri, Some(json!({}))).await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{uri}");
        assert_eq!(r["error"]["type"], "index_not_found_exception", "{uri}");
        assert_eq!(r["error"]["reason"], "no such index [nope]", "{uri}");
        assert_eq!(r["status"], 404, "{uri}");
    }
}

#[tokio::test]
async fn bad_mapping_is_400() {
    let body = json!({"mappings": {"properties": {"x": {"type": "nope"}}}});
    let (s, r) = call(&app(), Method::PUT, "/idx", Some(body)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(r["error"]["type"], "mapper_parsing_exception");
}

#[tokio::test]
async fn malformed_json_is_400() {
    let req = Request::builder()
        .method(Method::PUT)
        .uri("/idx")
        .body(Body::from("{"))
        .unwrap();
    let resp = app().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn settings_and_mapping_endpoints() {
    let app = app();
    call(&app, Method::PUT, "/idx", None).await;
    let (s, r) = call(
        &app,
        Method::PUT,
        "/idx/_settings",
        Some(json!({"index": {"number_of_replicas": 0}})),
    )
    .await;
    assert_eq!((s, r), (StatusCode::OK, json!({"acknowledged": true})));

    let (_, r) = call(&app, Method::GET, "/idx/_settings", None).await;
    assert_eq!(r["idx"]["settings"]["index"]["number_of_replicas"], "0");

    let (s, r) = call(&app, Method::GET, "/idx/_mapping", None).await;
    assert_eq!((s, r), (StatusCode::OK, json!({"idx": {"mappings": {}}})));
}
