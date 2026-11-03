//! Request bodies up to Elasticsearch's default `http.max_content_length` (100 MB) are accepted;
//! larger ones get its 413.

use axum::{body::Body, http::Request};
use tower::ServiceExt;

async fn bulk(bytes: usize) -> u16 {
    let line = "{\"index\":{\"_index\":\"t\"}}\n{\"f\":\"".to_owned() + &"x".repeat(1000) + "\"}\n";
    let body = line.repeat(bytes / line.len() + 1);
    let req = Request::builder()
        .method("POST")
        .uri("/_bulk")
        .header("content-type", "application/x-ndjson")
        .body(Body::from(body))
        .unwrap();
    es_mini::app().oneshot(req).await.unwrap().status().as_u16()
}

#[tokio::test]
async fn bulk_bodies_over_axums_2_mb_default_are_accepted() {
    assert_eq!(bulk(3 << 20).await, 200);
}

#[tokio::test]
async fn bodies_over_100_mb_are_too_large() {
    assert_eq!(bulk(100 << 20).await, 413);
}
