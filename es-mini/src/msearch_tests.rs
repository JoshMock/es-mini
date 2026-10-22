use crate::{app, indices_tests::call};
use axum::{
    body::Body,
    http::{Method, Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

async fn msearch(app: &axum::Router, uri: &str, body: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header("content-type", "application/x-ndjson")
        .body(Body::from(body.to_owned()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn runs_each_search_and_isolates_failures() {
    let app = app();
    call(&app, Method::PUT, "/a/_doc/1", Some(json!({"t": "red"}))).await;
    call(&app, Method::PUT, "/b/_doc/1", Some(json!({"t": "blue"}))).await;
    let body = concat!(
        "{}\n{\"query\":{\"match\":{\"t\":\"red\"}}}\n",
        "{\"index\":\"b\"}\n{}\n",
        "{\"index\":\"missing\"}\n{}\n",
        "{\"index\":[\"a\",\"b\"]}\n{\"size\":0}\n",
    );
    let (s, r) = msearch(&app, "/a/_msearch", body).await;
    assert_eq!(s, StatusCode::OK);
    assert!(r["took"].is_u64());
    let rs = r["responses"].as_array().unwrap();
    assert_eq!(rs.len(), 4);
    assert_eq!(rs[0]["status"], 200);
    assert_eq!(rs[0]["hits"]["hits"][0]["_index"], "a");
    assert_eq!(rs[1]["hits"]["hits"][0]["_index"], "b");
    assert_eq!(rs[2]["status"], 404);
    assert_eq!(rs[2]["error"]["type"], "index_not_found_exception");
    assert_eq!(rs[3]["hits"]["total"]["value"], 2);
    assert_eq!(rs[3]["hits"]["hits"], json!([]));
}

#[tokio::test]
async fn global_endpoint_and_empty_body() {
    let app = app();
    call(&app, Method::PUT, "/a/_doc/1", Some(json!({"t": "red"}))).await;
    let (s, r) = msearch(&app, "/_msearch", "{\"index\":\"a\"}\n{}\n").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(r["responses"][0]["hits"]["total"]["value"], 1);
    let (s, r) = msearch(&app, "/_msearch", "").await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(r["error"]["type"], "action_request_validation_exception");
}
