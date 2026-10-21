//! Unrecognized query parameters are rejected like Java's `BaseRestHandler`.

use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

async fn call(method: &str, uri: &str) -> (u16, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let res = es_mini::app().oneshot(req).await.unwrap();
    let status = res.status().as_u16();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn document_handlers_reject_unknown_params() {
    for (method, uri) in [
        ("PUT", "/x/_doc/1?foo=1"),
        ("POST", "/x/_doc?foo=1"),
        ("GET", "/x/_doc/1?foo=1"),
        ("POST", "/x/_update/1?foo=1"),
    ] {
        let (status, body) = call(method, uri).await;
        assert_eq!(status, 400, "{method} {uri}");
        assert_eq!(body["error"]["type"], "illegal_argument_exception");
        let path = uri.split('?').next().unwrap();
        assert_eq!(
            body["error"]["reason"],
            json!(format!(
                "request [{path}] contains unrecognized parameter: [foo]"
            ))
        );
    }
}

#[tokio::test]
async fn document_handlers_accept_known_params() {
    let (status, _) = call(
        "PUT",
        "/x/_doc/1?refresh=true&pretty&routing=r&op_type=index",
    )
    .await;
    assert_eq!(status, 201);
    let (status, _) = call(
        "GET",
        "/x/_doc/1?realtime=false&_source=false&filter_path=found",
    )
    .await;
    assert_ne!(status, 400);
}
