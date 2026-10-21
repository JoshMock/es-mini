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

#[tokio::test]
async fn alias_handlers_reject_unknown_params() {
    for (method, uri) in [
        ("GET", "/_alias?foo=1"),
        ("GET", "/_aliases?foo=1"),
        ("GET", "/_alias/a?foo=1"),
        ("GET", "/x/_alias?foo=1"),
        ("GET", "/x/_alias/a?foo=1"),
        ("HEAD", "/_alias/a?foo=1"),
        ("HEAD", "/x/_alias/a?foo=1"),
        ("PUT", "/x/_alias/a?foo=1"),
        ("DELETE", "/x/_alias/a?foo=1"),
        ("POST", "/_aliases?foo=1"),
    ] {
        let (status, body) = call(method, uri).await;
        assert_eq!(status, 400, "{method} {uri}");
        if method != "HEAD" {
            assert_eq!(body["error"]["type"], "illegal_argument_exception");
        }
    }
}

#[tokio::test]
async fn alias_handlers_accept_known_params() {
    for (method, uri) in [
        ("GET", "/_alias?local=true&ignore_unavailable=true"),
        ("PUT", "/x/_alias/a?timeout=1s&master_timeout=1s"),
    ] {
        let (status, _) = call(method, uri).await;
        assert_ne!(status, 400, "{method} {uri}");
    }
}

#[tokio::test]
async fn filter_path_applies_to_every_json_response() {
    let (status, body) = call("GET", "/_cluster/health?filter_path=status").await;
    assert_eq!((status, body), (200, json!({"status": "green"})));
    let (status, body) = call("GET", "/missing/_doc/1?filter_path=status").await;
    assert_eq!((status, body), (404, json!({"status": 404})));
}

#[tokio::test]
async fn total_hits_defaults_to_tracking_up_to_10000() {
    let app = es_mini::app();
    let mut bulk = String::new();
    for i in 0..10_001 {
        bulk.push_str(&format!("{{\"index\":{{\"_id\":\"{i}\"}}}}\n{{\"a\":1}}\n"));
    }
    let send = |method: &'static str, uri: &'static str, body: String| {
        let app = app.clone();
        async move {
            let req = Request::builder()
                .method(method)
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap();
            let res = app.oneshot(req).await.unwrap();
            let bytes = res.into_body().collect().await.unwrap().to_bytes();
            serde_json::from_slice::<Value>(&bytes).unwrap()
        }
    };
    send("POST", "/big/_bulk?refresh=true", bulk).await;
    let r = send("POST", "/big/_search?size=0", "{}".into()).await;
    assert_eq!(
        r["hits"]["total"],
        json!({"value": 10000, "relation": "gte"})
    );
    let r = send(
        "POST",
        "/big/_search?size=0&track_total_hits=true",
        "{}".into(),
    )
    .await;
    assert_eq!(
        r["hits"]["total"],
        json!({"value": 10001, "relation": "eq"})
    );
    let r = send(
        "POST",
        "/big/_search?size=0&rest_total_hits_as_int=true",
        "{}".into(),
    )
    .await;
    assert_eq!(r["hits"]["total"], json!(10001));
    let r = send(
        "POST",
        "/big/_search?size=0",
        r#"{"track_total_hits":true}"#.into(),
    )
    .await;
    assert_eq!(
        r["hits"]["total"],
        json!({"value": 10001, "relation": "eq"})
    );
    let r = send(
        "POST",
        "/big/_search?size=0",
        r#"{"track_total_hits":5}"#.into(),
    )
    .await;
    assert_eq!(r["hits"]["total"], json!({"value": 5, "relation": "gte"}));
    let r = send(
        "POST",
        "/big/_search?size=0&track_total_hits=true",
        r#"{"track_total_hits":false}"#.into(),
    )
    .await;
    assert_eq!(r["hits"]["total"]["value"], json!(10001));
}

#[tokio::test]
async fn error_metadata_and_empty_body() {
    let app = es_mini::app();
    let send = |m: &str, u: &str, b: &'static str| {
        let req = Request::builder()
            .method(m)
            .uri(u)
            .header("content-type", "application/json")
            .body(Body::from(b))
            .unwrap();
        let app = app.clone();
        async move {
            let res = app.oneshot(req).await.unwrap();
            let s = res.status().as_u16();
            let b = res.into_body().collect().await.unwrap().to_bytes();
            (s, serde_json::from_slice::<Value>(&b).unwrap())
        }
    };
    let (s, b) = send("PUT", "/meta1/_doc/1", "").await;
    assert_eq!(
        (s, b["error"]["reason"].as_str()),
        (400, Some("request body is required"))
    );
    send("PUT", "/meta1/_doc/1", "{}").await;
    let (s, b) = send("PUT", "/meta1/_create/1", "{}").await;
    assert_eq!(s, 409);
    assert_eq!(b["error"]["index"], "meta1");
    assert_eq!(b["error"]["index_uuid"], "_na_");
    let (s, b) = send("POST", "/meta1/_update/9", r#"{"doc":{"a":1}}"#).await;
    assert_eq!(s, 404);
    assert_eq!(b["error"]["index"], "meta1");
}
