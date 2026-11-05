//! Errors Elasticsearch reports for malformed parts of a request. Every expected value here is
//! what Elasticsearch 9.5.5 answered to the same requests, but for the `[line:col]` prefix of
//! x_content_parse_exception reasons, which es-mini does not give.

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

/// Index `t` with one document.
async fn setup() -> axum::Router {
    let app = es_mini::app();
    let mappings =
        json!({"mappings": {"properties": {"n": {"type": "long"}, "k": {"type": "keyword"}}}});
    call(&app, "PUT", "/t", mappings).await;
    call(
        &app,
        "PUT",
        "/t/_doc/1?refresh=true",
        json!({"n": 1, "k": "a"}),
    )
    .await;
    app
}

#[tokio::test]
async fn error_positions_malformed_order_path_elements() {
    let app = setup().await;
    let max = json!({"a": {"max": {"field": "n"}}});
    let filter = json!({"f": {"filter": {"match_all": {}}, "aggs": max}});
    for (path, subs, element) in [
        ("a[", &max, "a["),
        ("[x]", &max, "[x]"),
        ("a[]", &max, "a[]"),
        ("a[x]y", &max, "a[x]y"),
        ("a[x", &max, "a[x"),
        ("a.", &max, "a."),
        (".a", &max, ".a"),
        ("f>[x]", &filter, "[x]"),
        ("f[x>a", &filter, "f[x"),
    ] {
        let body = json!({"size": 0, "aggs": {"x": {"terms": {"field": "k", "order": {path: "asc"}}, "aggs": subs}}});
        let outer = json!({"type": "x_content_parse_exception", "reason": "[terms] failed to parse field [order]"});
        let mut error = outer.clone();
        error["root_cause"] = json!([outer]);
        error["caused_by"] = json!({"type": "illegal_argument_exception",
            "reason": format!("Invalid path element [{element}] in path [{path}]")});
        let expected = json!({"error": error, "status": 400});
        assert_eq!(
            call(&app, "POST", "/t/_search", body).await,
            (400, expected),
            "{path}"
        );
    }
}
