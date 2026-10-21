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

#[tokio::test]
async fn search_count_bulk_reject_unknown_params() {
    for (method, uri) in [
        ("POST", "/x/_search?foo=1"),
        ("GET", "/_search?foo=1"),
        ("POST", "/x/_count?foo=1"),
        ("POST", "/_count?foo=1"),
        ("POST", "/_bulk?foo=1"),
        ("POST", "/x/_bulk?foo=1"),
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
async fn search_count_bulk_accept_known_params() {
    for (method, uri) in [
        (
            "POST",
            "/_search?size=1&q=a&rest_total_hits_as_int=true&pretty",
        ),
        ("POST", "/_count?q=a&terminate_after=1&filter_path=count"),
        ("POST", "/_bulk?refresh=true&require_alias=false"),
    ] {
        let (_, body) = call(method, uri).await;
        let reason = body["error"]["reason"].as_str().unwrap_or_default();
        assert!(!reason.contains("unrecognized"), "{method} {uri}: {reason}");
    }
}

#[tokio::test]
async fn index_admin_handlers_reject_unknown_params() {
    for (method, uri) in [
        ("PUT", "/x?foo=1"),
        ("GET", "/x?foo=1"),
        ("DELETE", "/x?foo=1"),
        ("POST", "/x/_refresh?foo=1"),
        ("GET", "/x/_settings?foo=1"),
        ("GET", "/_settings?foo=1"),
        ("GET", "/x/_settings/index.*?foo=1"),
        ("GET", "/_settings/index.*?foo=1"),
        ("PUT", "/x/_settings?foo=1"),
        ("PUT", "/_settings?foo=1"),
        ("POST", "/x/_close?foo=1"),
        ("POST", "/x/_open?foo=1"),
        ("GET", "/x/_mapping?foo=1"),
        ("GET", "/_mapping?foo=1"),
        ("GET", "/_cluster/health?foo=1"),
        ("POST", "/_query?foo=1"),
        ("PUT", "/x/_create/1?foo=1"),
        ("DELETE", "/x/_doc/1?foo=1"),
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
async fn index_admin_handlers_accept_known_params() {
    for (method, uri) in [
        ("PUT", "/y?master_timeout=1s&timeout=1s"),
        ("GET", "/y?flat_settings=true&ignore_unavailable=true&human"),
        (
            "GET",
            "/y/_settings?include_defaults=true&expand_wildcards=open",
        ),
        ("PUT", "/y/_settings?reopen=true&preserve_existing=true"),
        ("POST", "/y/_close?allow_no_indices=true"),
        ("POST", "/y/_open?wait_for_active_shards=1"),
        ("GET", "/y/_mapping?local=true"),
        ("GET", "/_cluster/health?level=indices&timeout=1s"),
        ("DELETE", "/x/_doc/1?refresh=true&routing=r"),
        ("DELETE", "/y?timeout=1s"),
    ] {
        let (_, body) = call(method, uri).await;
        let reason = body["error"]["reason"].as_str().unwrap_or_default();
        assert!(!reason.contains("unrecognized"), "{method} {uri}: {reason}");
    }
}
