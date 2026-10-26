//! PUT _settings: which settings Elasticsearch 9.5.5 lets change on open and closed indices
//! (MetadataUpdateSettingsService and AbstractScopedSettings.updateSettings).

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

async fn index(app: &axum::Router) -> String {
    call(app, "PUT", "/t", json!({})).await;
    let (_, s) = call(app, "GET", "/t/_settings", json!({})).await;
    s["t"]["settings"]["index"]["uuid"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn reason(body: &Value) -> &str {
    body["error"]["reason"].as_str().unwrap()
}

#[tokio::test]
async fn final_setting_is_rejected_on_open_index() {
    let app = es_mini::app();
    let uuid = index(&app).await;
    let (s, b) = call(&app, "PUT", "/t/_settings", json!({"number_of_shards": 2})).await;
    assert_eq!(s, 400);
    assert_eq!(
        reason(&b),
        format!(
            "Can't update non dynamic setting(s) [[index.number_of_shards]] for open indices [[t/{uuid}]]. The setting(s) [[index.number_of_shards]] cannot be modified on an index once it is created. You will need to create a new index with the desired setting(s) and reindex your data. See https://www.elastic.co/docs/api/doc/elasticsearch/operation/operation-reindex"
        )
    );
}

#[tokio::test]
async fn static_setting_suggests_reopen_on_open_index() {
    let app = es_mini::app();
    let uuid = index(&app).await;
    let (s, b) = call(
        &app,
        "PUT",
        "/t/_settings",
        json!({"index.codec": "best_compression"}),
    )
    .await;
    assert_eq!(s, 400);
    assert_eq!(
        reason(&b),
        format!(
            "Can't update non dynamic setting(s) [[index.codec]] for open indices [[t/{uuid}]]. You can either resubmit the update with `?reopen=true`, or create a new index with the desired setting(s) and reindex your data. See https://www.elastic.co/docs/api/doc/elasticsearch/operation/operation-reindex"
        )
    );
}

#[tokio::test]
async fn static_setting_applies_to_closed_index() {
    let app = es_mini::app();
    index(&app).await;
    call(&app, "POST", "/t/_close", json!({})).await;
    let (s, _) = call(
        &app,
        "PUT",
        "/t/_settings",
        json!({"index.codec": "best_compression"}),
    )
    .await;
    assert_eq!(s, 200);
    let (_, b) = call(&app, "GET", "/t/_settings?expand_wildcards=all", json!({})).await;
    assert_eq!(b["t"]["settings"]["index"]["codec"], "best_compression");
}

#[tokio::test]
async fn final_setting_is_rejected_on_closed_index() {
    let app = es_mini::app();
    index(&app).await;
    call(&app, "POST", "/t/_close", json!({})).await;
    let (s, b) = call(&app, "PUT", "/t/_settings", json!({"number_of_shards": 2})).await;
    assert_eq!(s, 400);
    assert_eq!(
        reason(&b),
        "final t setting [index.number_of_shards], not updateable"
    );
}

#[tokio::test]
async fn reopen_with_unchanged_final_setting_is_a_noop() {
    let app = es_mini::app();
    index(&app).await;
    let (s, _) = call(
        &app,
        "PUT",
        "/t/_settings?reopen=true",
        json!({"number_of_shards": 1}),
    )
    .await;
    assert_eq!(s, 200);
}
