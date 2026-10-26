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

/// PUTs `body` to an open index and returns the error reason, checking nothing changed.
async fn rejected(body: Value) -> String {
    let app = es_mini::app();
    index(&app).await;
    let (_, before) = call(&app, "GET", "/t/_settings", json!({})).await;
    let (s, b) = call(&app, "PUT", "/t/_settings", body).await;
    assert_eq!(s, 400, "{b}");
    assert_eq!(b["error"]["type"], "illegal_argument_exception");
    let (_, after) = call(&app, "GET", "/t/_settings", json!({})).await;
    assert_eq!(before, after);
    reason(&b).to_owned()
}

#[tokio::test]
async fn setting_values_are_validated() {
    for (body, want) in [
        (
            json!({"number_of_replicas": -1}),
            "Failed to parse value [-1] for setting [index.number_of_replicas] must be >= 0",
        ),
        (
            json!({"number_of_replicas": "two"}),
            "Failed to parse value [two] for setting [index.number_of_replicas]",
        ),
        (
            json!({"max_result_window": 3_000_000_000_u64}),
            "Failed to parse value [3000000000] for setting [index.max_result_window] must be <= 2147483647",
        ),
        (
            json!({"index": {"hidden": "yes"}}),
            "Failed to parse value [yes] as only [true] or [false] are allowed.",
        ),
        (
            json!({"refresh_interval": "10"}),
            "failed to parse setting [index.refresh_interval] with value [10] as a time value: unit is missing or unrecognized",
        ),
        (
            json!({"refresh_interval": "1.5s"}),
            "failed to parse [1.5s], fractional time values are not supported",
        ),
        (
            json!({"index.translog.sync_interval": "50ms"}),
            "failed to parse value [50ms] for setting [index.translog.sync_interval], must be >= [100ms]",
        ),
        (
            json!({"index.gc_deletes": "-1s"}),
            "failed to parse value [-1s] for setting [index.gc_deletes], must be >= [-1]",
        ),
        (
            json!({"index.translog.durability": "sometimes"}),
            "No enum constant org.elasticsearch.index.translog.Translog.Durability.SOMETIMES",
        ),
        (
            json!({"index.mapping.array_objects.limit": 0}),
            "Failed to parse value [0] for setting [index.mapping.array_objects.limit] must be >= 1",
        ),
    ] {
        assert_eq!(rejected(body).await, want);
    }
}

#[tokio::test]
async fn private_setting_is_rejected() {
    assert_eq!(
        rejected(json!({"index.version.created": "1"})).await,
        "can not update private setting [index.version.created]; this setting is managed by Elasticsearch"
    );
}

#[tokio::test]
async fn nested_static_setting_is_rejected_on_open_index() {
    assert!(
        rejected(json!({"index.query_string.lenient": true}))
            .await
            .starts_with("Can't update non dynamic setting(s) [[index.query_string.lenient]]")
    );
}

#[tokio::test]
async fn unknown_nested_setting_is_rejected() {
    assert_eq!(
        rejected(json!({"index": {"mapping": {"unknown_nested_thing": 1}}})).await,
        "unknown setting [index.mapping.unknown_nested_thing] please check that any required plugins are installed, or check the breaking changes documentation for removed settings"
    );
}

#[tokio::test]
async fn nested_settings_are_validated() {
    assert_eq!(
        rejected(json!({"index.blocks.write": "yes"})).await,
        "Failed to parse value [yes] as only [true] or [false] are allowed."
    );
    assert_eq!(
        rejected(json!({"index.mapping.depth.limit": 0})).await,
        "Failed to parse value [0] for setting [index.mapping.depth.limit] must be >= 1"
    );
}

#[tokio::test]
async fn group_settings_take_their_scope() {
    // index.analysis.* is a static group setting.
    assert!(
        rejected(json!({"index.analysis.analyzer.a.type": "standard"}))
            .await
            .starts_with("Can't update non dynamic setting(s) [[index.analysis.analyzer.a.type]]")
    );
    // index.sort.* settings are final.
    assert!(
        rejected(json!({"index.sort.field": "f"}))
            .await
            .contains("The setting(s) [[index.sort.field]] cannot be modified")
    );
    // index.routing.allocation.include.* is a dynamic affix setting.
    let app = es_mini::app();
    index(&app).await;
    let (s, b) = call(
        &app,
        "PUT",
        "/t/_settings",
        json!({"index.routing.allocation.include._name": "node-1", "index.mapping.total_fields.limit": 5}),
    )
    .await;
    assert_eq!(s, 200, "{b}");
}

#[tokio::test]
async fn valid_values_are_accepted() {
    let app = es_mini::app();
    index(&app).await;
    let (s, b) = call(
        &app,
        "PUT",
        "/t/_settings",
        json!({"index": {"translog.durability": "ASYNC", "refresh_interval": "-1", "hidden": false, "number_of_replicas": "+2"}}),
    )
    .await;
    assert_eq!(s, 200, "{b}");
}

#[tokio::test]
async fn include_defaults_lists_every_modelled_default_not_set() {
    let app = es_mini::app();
    call(
        &app,
        "PUT",
        "/t",
        json!({"settings": {"refresh_interval": "5s"}}),
    )
    .await;
    let (_, b) = call(
        &app,
        "GET",
        "/t/_settings?include_defaults=true&flat_settings=true",
        json!({}),
    )
    .await;
    let defaults = &b["t"]["defaults"];
    assert_eq!(defaults["index.codec"], "default");
    assert_eq!(defaults["index.translog.durability"], "REQUEST");
    assert_eq!(defaults["index.max_result_window"], "10000");
    assert_eq!(defaults["index.mapping.dynamic_strings.auto_text"], "true");
    // Set settings are not defaults.
    assert!(defaults.get("index.refresh_interval").is_none());
    assert!(defaults.get("index.number_of_shards").is_none());
    // Name filters apply to defaults too.
    let (_, b) = call(
        &app,
        "GET",
        "/t/_settings/index.translog.*?include_defaults=true",
        json!({}),
    )
    .await;
    assert_eq!(
        b["t"]["defaults"],
        json!({"index": {"translog": {"durability": "REQUEST", "sync_interval": "5s", "flush_threshold_size": "10gb"}}})
    );
}

/// Elasticsearch accepts a setting that is both a leaf and a branch, and renders the branch's
/// keys in dot notation beside the leaf (Settings.getAsStructuredMap).
#[tokio::test]
async fn leaf_that_is_also_a_branch_renders_in_dot_notation() {
    let app = es_mini::app();
    call(
        &app,
        "PUT",
        "/t",
        json!({"settings": {"index.routing.allocation.include._name": "a", "index.routing.allocation.include._name.x": "b"}}),
    )
    .await;
    let (_, b) = call(&app, "GET", "/t/_settings", json!({})).await;
    assert_eq!(
        b["t"]["settings"]["index"]["routing"]["allocation"]["include"],
        json!({"_name": "a", "_name.x": "b"})
    );
}
