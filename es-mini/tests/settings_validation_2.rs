//! Index settings validation on create: byte sizes, sort lists, codec names, every setting.

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

async fn create(settings: Value) -> (u16, Value) {
    call(&es_mini::app(), "PUT", "/t", json!({"settings": settings})).await
}

#[tokio::test]
async fn settings_validation_2_byte_size_is_parsed() {
    let (s, b) = create(json!({"index.translog.flush_threshold_size": "lots"})).await;
    assert_eq!(s, 400);
    assert_eq!(
        b["error"]["reason"],
        "failed to parse setting [index.translog.flush_threshold_size] with value [lots] as a size in bytes: unit is missing or unrecognized"
    );
    let (s, _) = create(json!({"index.translog.flush_threshold_size": "512mb"})).await;
    assert_eq!(s, 200);
    let (s, b) = create(json!({"index.translog.flush_threshold_size": "4b"})).await;
    assert_eq!(s, 400);
    assert!(
        b["error"]["reason"]
            .as_str()
            .unwrap()
            .ends_with("must be >= [8b]")
    );
}

#[tokio::test]
async fn settings_validation_2_sort_lists_parse_each_element() {
    let (s, b) =
        create(json!({"index.sort.field": ["a", "b"], "index.sort.order": ["asc", "sideways"]}))
            .await;
    assert_eq!(s, 400);
    assert_eq!(b["error"]["reason"], "Illegal sort order:sideways");
    let (s, b) = create(json!({"index.sort.missing": ["_middle"]})).await;
    assert_eq!(s, 400);
    assert_eq!(
        b["error"]["reason"],
        "Illegal missing value:[_middle], must be one of [_last, _first]"
    );
    // Elasticsearch refuses an index sort on an unmapped field, so `a` is mapped.
    let settings = json!({"index.sort.field": "a", "index.sort.order": "desc"});
    let mappings = json!({"properties": {"a": {"type": "keyword"}}});
    let body = json!({"settings": settings, "mappings": mappings});
    let (s, _) = call(&es_mini::app(), "PUT", "/t", body).await;
    assert_eq!(s, 200);
}

#[tokio::test]
async fn settings_validation_2_sort_lists_default_to_empty() {
    let app = es_mini::app();
    call(&app, "PUT", "/t", json!({})).await;
    let (_, b) = call(&app, "GET", "/t/_settings?include_defaults=true", json!({})).await;
    assert_eq!(b["t"]["defaults"]["index"]["sort"]["field"], json!([]));
}

#[tokio::test]
async fn settings_validation_2_lucene_codec_names_are_accepted() {
    let (s, _) = create(json!({"index.codec": "Lucene104"})).await;
    assert_eq!(s, 200);
    let (s, _) = create(json!({"index.codec": "nope"})).await;
    assert_eq!(s, 400);
}

#[tokio::test]
async fn settings_validation_2_every_setting_is_validated_on_create() {
    let (s, b) = create(json!({"index.max_result_window": "many"})).await;
    assert_eq!(s, 400);
    assert_eq!(
        b["error"]["reason"],
        "Failed to parse value [many] for setting [index.max_result_window]"
    );
    let (s, b) = create(json!({"index": {"hidden": "maybe"}})).await;
    assert_eq!(s, 400, "{b}");
}

#[tokio::test]
async fn settings_validation_2_more_scoped_settings_are_modeled() {
    let app = es_mini::app();
    call(&app, "PUT", "/t", json!({})).await;
    let (s, _) = call(
        &app,
        "PUT",
        "/t/_settings",
        json!({"index.max_terms_count": 10}),
    )
    .await;
    assert_eq!(s, 200);
    let (s, b) = call(
        &app,
        "PUT",
        "/t/_settings",
        json!({"index.max_terms_count": 0}),
    )
    .await;
    assert_eq!(s, 400, "{b}");
}
