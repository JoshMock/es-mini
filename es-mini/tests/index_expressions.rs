//! Search/count and HEAD resolve index expressions like Elasticsearch.

use axum::{Router, body::Body, http::Request};
use tower::ServiceExt;

async fn status(app: &Router, method: &str, uri: &str) -> u16 {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from("{}"))
        .unwrap();
    app.clone().oneshot(req).await.unwrap().status().as_u16()
}

#[tokio::test]
async fn search_count_and_head_resolve_expressions() {
    let app = es_mini::app();
    assert_eq!(status(&app, "PUT", "/logs-1/_doc/1").await, 201);
    assert_eq!(status(&app, "POST", "/logs-1/_alias/logs").await, 200);

    for api in ["_search", "_count"] {
        let u = |q: &str| format!("/logs-1,missing/{api}{q}");
        assert_eq!(status(&app, "GET", &u("")).await, 404, "{api}");
        assert_eq!(
            status(&app, "GET", &u("?ignore_unavailable=true")).await,
            200
        );
        let none = format!("/nomatch*/{api}");
        assert_eq!(status(&app, "GET", &none).await, 200);
        let strict = format!("{none}?allow_no_indices=false");
        assert_eq!(status(&app, "GET", &strict).await, 404, "{api}");
    }

    for (name, want) in [
        ("logs-1", 200),
        ("logs", 200),
        ("logs*", 200),
        ("logs-1,x", 404),
        ("nope", 404),
    ] {
        assert_eq!(
            status(&app, "HEAD", &format!("/{name}")).await,
            want,
            "{name}"
        );
    }
}

async fn get(app: &Router, uri: &str) -> serde_json::Value {
    let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn search_lists_resolve_as_one_expression() {
    let app = es_mini::app();
    for i in ["logs-1", "logs-2"] {
        assert_eq!(
            status(&app, "PUT", &format!("/{i}/_doc/1?refresh=true")).await,
            201
        );
    }
    let count = async |uri: &str| get(&app, uri).await["count"].clone();
    assert_eq!(count("/logs-*,-logs-1/_count").await, 1);
    assert_eq!(count("/logs-1,logs-*/_count").await, 2);
    let expr = "/logs-*,-logs-1,missing";
    assert_eq!(
        count(&format!("{expr}/_count?ignore_unavailable=true")).await,
        1
    );
    let index = get(&app, &format!("{expr}?ignore_unavailable=true")).await;
    assert_eq!(
        index.as_object().unwrap().keys().collect::<Vec<_>>(),
        ["logs-2"]
    );
}
