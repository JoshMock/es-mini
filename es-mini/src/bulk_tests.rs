use crate::{app, indices_tests::call};
use axum::{
    Router,
    body::Body,
    http::{Method, Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

async fn bulk(app: &Router, uri: &str, body: &str) -> (StatusCode, Value) {
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
async fn mixed_actions_with_one_failure() {
    let app = app();
    let body = r#"{"index":{"_index":"idx","_id":"1"}}
{"a":1}
{"create":{"_index":"idx","_id":"1"}}
{"a":2}
{"update":{"_index":"idx","_id":"1"}}
{"doc":{"b":2}}
{"create":{"_index":"idx"}}
{"c":3}
{"delete":{"_index":"idx","_id":"1"}}
"#;
    let (s, r) = bulk(&app, "/_bulk", body).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(r["errors"], true);
    assert!(r["took"].is_u64());
    let items = r["items"].as_array().unwrap();
    assert_eq!(items.len(), 5);

    assert_eq!(items[0]["index"]["status"], 201);
    assert_eq!(items[0]["index"]["result"], "created");
    assert_eq!(items[0]["index"]["_index"], "idx");

    assert_eq!(items[1]["create"]["status"], 409);
    assert_eq!(items[1]["create"]["_id"], "1");
    assert_eq!(
        items[1]["create"]["error"]["type"],
        "version_conflict_engine_exception"
    );

    assert_eq!(items[2]["update"]["status"], 200);
    assert_eq!(items[2]["update"]["_version"], 2);

    assert_eq!(items[3]["create"]["status"], 201);
    assert_eq!(items[3]["create"]["_id"].as_str().unwrap().len(), 20);

    assert_eq!(items[4]["delete"]["result"], "deleted");

    let (s, _) = call(&app, Method::GET, "/idx/_doc/1", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn default_index_from_path_and_no_trailing_newline() {
    let app = app();
    let (s, r) = bulk(&app, "/idx/_bulk", "{\"index\":{\"_id\":\"1\"}}\n{\"a\":1}").await;
    assert_eq!((s, &r["errors"]), (StatusCode::OK, &json!(false)));
    let (_, r) = call(&app, Method::GET, "/idx/_doc/1", None).await;
    assert_eq!(r["_source"], json!({"a": 1}));
}

#[tokio::test]
async fn missing_index_is_request_error() {
    let (s, r) = bulk(&app(), "/_bulk", "{\"index\":{}}\n{}\n").await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(r["error"]["type"], "action_request_validation_exception");
}

#[tokio::test]
async fn malformed_lines_are_request_errors() {
    for body in [
        "nope\n",
        "{\"explode\":{}}\n",
        "{\"index\":{\"_index\":\"i\"}}\n",
        "{\"delete\":{\"_index\":\"i\"}}\n",
    ] {
        let (s, r) = bulk(&app(), "/_bulk", body).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(r["status"], 400, "{body}");
    }
}
