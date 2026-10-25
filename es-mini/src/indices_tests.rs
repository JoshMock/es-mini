use crate::app;
use axum::{
    Router,
    body::Body,
    http::{Method, Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

pub(crate) async fn call(
    app: &Router,
    method: Method,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let body = body.map_or(Body::empty(), |b| Body::from(b.to_string()));
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(body)
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, json)
}

#[tokio::test]
async fn create_get_head_delete() {
    let app = app();
    let body = json!({"settings": {"number_of_shards": 2}, "mappings": {"properties": {"t": {"type": "text"}}}});
    let (s, r) = call(&app, Method::PUT, "/idx", Some(body)).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        r,
        json!({"acknowledged": true, "shards_acknowledged": true, "index": "idx"})
    );

    let (s, r) = call(&app, Method::GET, "/idx", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        r["idx"]["mappings"],
        json!({"properties": {"t": {"type": "text"}}})
    );
    assert_eq!(r["idx"]["settings"]["index"]["number_of_shards"], "2");
    assert_eq!(r["idx"]["aliases"], json!({}));

    assert_eq!(
        call(&app, Method::HEAD, "/idx", None).await.0,
        StatusCode::OK
    );

    let (s, r) = call(&app, Method::DELETE, "/idx", None).await;
    assert_eq!((s, r), (StatusCode::OK, json!({"acknowledged": true})));
    assert_eq!(
        call(&app, Method::HEAD, "/idx", None).await,
        (StatusCode::NOT_FOUND, Value::Null)
    );
}

#[tokio::test]
async fn create_without_body() {
    let (s, _) = call(&app(), Method::PUT, "/idx", None).await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test]
async fn create_existing_is_400() {
    let app = app();
    call(&app, Method::PUT, "/idx", None).await;
    let (s, r) = call(&app, Method::PUT, "/idx", None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(r["status"], 400);
    assert_eq!(r["error"]["type"], "resource_already_exists_exception");
    assert_eq!(
        r["error"]["root_cause"][0]["type"],
        "resource_already_exists_exception"
    );
    assert_eq!(r["error"]["index"], "idx");
}

#[tokio::test]
async fn missing_index_is_404_everywhere() {
    let app = app();
    for (m, uri) in [
        (Method::GET, "/nope"),
        (Method::DELETE, "/nope"),
        (Method::GET, "/nope/_settings"),
        (Method::PUT, "/nope/_settings"),
        (Method::GET, "/nope/_mapping"),
    ] {
        let (s, r) = call(&app, m, uri, Some(json!({}))).await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{uri}");
        assert_eq!(r["error"]["type"], "index_not_found_exception", "{uri}");
        assert_eq!(r["error"]["reason"], "no such index [nope]", "{uri}");
        assert_eq!(r["status"], 404, "{uri}");
    }
}

#[tokio::test]
async fn bad_mapping_is_400() {
    let body = json!({"mappings": {"properties": {"x": {"type": "nope"}}}});
    let (s, r) = call(&app(), Method::PUT, "/idx", Some(body)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(r["error"]["type"], "mapper_parsing_exception");
}

#[tokio::test]
async fn date_nanos_fields_and_date_multi_fields() {
    let app = app();
    let props = json!({
        "d": {"type": "date", "fields": {"nanos": {"type": "date_nanos"}}},
        "n": {"type": "date_nanos"}
    });
    let body = json!({"mappings": {"properties": props}});
    let (s, r) = call(&app, Method::PUT, "/idx", Some(body)).await;
    assert_eq!(s, StatusCode::OK, "{r}");
    let (_, r) = call(&app, Method::GET, "/idx/_mapping", None).await;
    assert_eq!(r["idx"]["mappings"]["properties"], props);
    let doc = json!({"d": "2016-01-02", "n": "2016-01-03T00:00:00.123456789Z"});
    call(&app, Method::PUT, "/idx/_doc/1?refresh=true", Some(doc)).await;
    // aggregations read date_nanos doc values as millis
    let aggs = json!({"a": {"max": {"field": "d.nanos"}}, "b": {"max": {"field": "n"}}});
    let body = json!({"size": 0, "aggs": aggs});
    let (s, r) = call(&app, Method::POST, "/idx/_search", Some(body)).await;
    assert_eq!(s, StatusCode::OK, "{r}");
    let a = &r["aggregations"];
    assert_eq!(a["a"]["value_as_string"], "2016-01-02T00:00:00.000Z");
    assert_eq!(a["b"]["value"], 1_451_779_200_123.0);
}

#[tokio::test]
async fn range_fields() {
    let app = app();
    let props = json!({
        "d": {"type": "date_range", "format": "yyyy-MM-dd"},
        "l": {"type": "long_range"}
    });
    let body = json!({"mappings": {"properties": props}});
    let (s, r) = call(&app, Method::PUT, "/idx", Some(body)).await;
    assert_eq!(s, StatusCode::OK, "{r}");
    let (_, r) = call(&app, Method::GET, "/idx/_mapping", None).await;
    assert_eq!(r["idx"]["mappings"]["properties"], props);
    let doc = json!({"d": {"gte": "2016-01-02"}, "l": {"gte": 1, "lt": 3}});
    let (s, r) = call(&app, Method::PUT, "/idx/_doc/1?refresh=true", Some(doc)).await;
    assert_eq!(s, StatusCode::CREATED, "{r}");
    let body = json!({"query": {"term": {"l": 2}}});
    let (s, r) = call(&app, Method::POST, "/idx/_search", Some(body)).await;
    assert_eq!(s, StatusCode::OK, "{r}");
}

#[tokio::test]
async fn empty_field_name_is_illegal_argument() {
    let body = json!({"mappings": {"properties": {"": {"type": "keyword"}}}});
    let (s, r) = call(&app(), Method::PUT, "/idx", Some(body)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(r["error"]["type"], "illegal_argument_exception");
}

#[tokio::test]
async fn disabling_soft_deletes_is_illegal_argument() {
    let body = json!({"settings": {"soft_deletes.enabled": false}});
    let (s, r) = call(&app(), Method::PUT, "/idx", Some(body)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(r["error"]["type"], "illegal_argument_exception");
}

#[tokio::test]
async fn malformed_json_is_400() {
    let req = Request::builder()
        .method(Method::PUT)
        .uri("/idx")
        .body(Body::from("{"))
        .unwrap();
    let resp = app().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn settings_and_mapping_endpoints() {
    let app = app();
    call(&app, Method::PUT, "/idx", None).await;
    let (s, r) = call(
        &app,
        Method::PUT,
        "/idx/_settings",
        Some(json!({"index": {"number_of_replicas": 0}})),
    )
    .await;
    assert_eq!((s, r), (StatusCode::OK, json!({"acknowledged": true})));

    let (_, r) = call(&app, Method::GET, "/idx/_settings", None).await;
    assert_eq!(r["idx"]["settings"]["index"]["number_of_replicas"], "0");

    let (_, r) = call(&app, Method::GET, "/_settings", None).await;
    assert_eq!(r["idx"]["settings"]["index"]["number_of_replicas"], "0");

    let (s, r) = call(&app, Method::GET, "/idx/_mapping", None).await;
    assert_eq!((s, r), (StatusCode::OK, json!({"idx": {"mappings": {}}})));

    let (s, r) = call(&app, Method::GET, "/*/_mapping", None).await;
    assert_eq!((s, r), (StatusCode::OK, json!({"idx": {"mappings": {}}})));
}

#[tokio::test]
async fn put_settings_rejects_unknown_setting() {
    let app = app();
    call(&app, Method::PUT, "/idx", None).await;
    let (s, _) = call(
        &app,
        Method::PUT,
        "/idx/_settings",
        Some(json!({"garbage": "garbage"})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn put_settings_preserve_existing_keeps_set_keys() {
    let app = app();
    call(&app, Method::PUT, "/idx", None).await;
    call(
        &app,
        Method::PUT,
        "/idx/_settings",
        Some(json!({"number_of_replicas": 0})),
    )
    .await;
    call(
        &app,
        Method::PUT,
        "/idx/_settings?preserve_existing=true",
        Some(json!({"number_of_replicas": 1, "index.translog.durability": "async"})),
    )
    .await;
    let (_, r) = call(&app, Method::GET, "/idx/_settings", None).await;
    assert_eq!(r["idx"]["settings"]["index"]["number_of_replicas"], "0");
    assert_eq!(
        r["idx"]["settings"]["index"]["translog"]["durability"],
        "async"
    );
}

#[tokio::test]
async fn close_acknowledges_existing_index() {
    let app = app();
    call(&app, Method::PUT, "/idx", None).await;
    let (s, r) = call(&app, Method::POST, "/idx/_close", None).await;
    assert_eq!(
        (s, r["acknowledged"].clone()),
        (StatusCode::OK, json!(true))
    );
    let (s, _) = call(&app, Method::POST, "/nope/_close", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn open_acknowledges_existing_index() {
    let app = app();
    call(&app, Method::PUT, "/idx", None).await;
    let (s, r) = call(&app, Method::POST, "/idx/_open", None).await;
    assert_eq!(
        (s, r["acknowledged"].clone()),
        (StatusCode::OK, json!(true))
    );
    let (s, _) = call(&app, Method::POST, "/nope/_open", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn closed_indices_reject_data_operations_but_keep_metadata() {
    let app = app();
    call(&app, Method::PUT, "/idx/_doc/1", Some(json!({"a": 1}))).await;
    call(&app, Method::PUT, "/other/_doc/1", Some(json!({"a": 2}))).await;
    let (s, r) = call(&app, Method::POST, "/idx/_close", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(r["indices"], json!({"idx": {"closed": true}}));

    for (m, uri, body) in [
        (Method::GET, "/idx/_doc/1", None),
        (Method::PUT, "/idx/_doc/2", Some(json!({}))),
        (
            Method::POST,
            "/idx/_update/1",
            Some(json!({"doc": {"b": 1}})),
        ),
        (Method::DELETE, "/idx/_doc/1", None),
        (Method::POST, "/idx/_search", None),
        (Method::POST, "/idx/_count", None),
    ] {
        let (s, r) = call(&app, m, uri, body).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{uri}");
        assert_eq!(r["error"]["type"], "index_closed_exception", "{uri}");
        assert_eq!(
            (&r["error"]["reason"], &r["error"]["index"]),
            (&json!("closed"), &json!("idx")),
            "{uri}"
        );
    }
    let (s, r) = call(
        &app,
        Method::POST,
        "/_query",
        Some(json!({"query": "FROM idx"})),
    )
    .await;
    assert_eq!(
        (s, &r["error"]["type"]),
        (StatusCode::BAD_REQUEST, &json!("index_closed_exception"))
    );

    // Wildcards skip closed indices.
    let (_, r) = call(&app, Method::POST, "/*/_count", None).await;
    assert_eq!(r["count"], 1);

    // Metadata and settings still work while closed.
    assert_eq!(
        call(&app, Method::GET, "/idx", None).await.0,
        StatusCode::OK
    );
    assert_eq!(
        call(&app, Method::GET, "/idx/_mapping", None).await.0,
        StatusCode::OK
    );
    let (s, _) = call(
        &app,
        Method::PUT,
        "/idx/_settings",
        Some(json!({"index": {"number_of_replicas": 0}})),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    call(&app, Method::POST, "/idx/_open", None).await;
    let (s, r) = call(&app, Method::GET, "/idx/_doc/1", None).await;
    assert_eq!((s, &r["_source"]), (StatusCode::OK, &json!({"a": 1})));
}

#[tokio::test]
async fn get_human_adds_readable_settings() {
    let app = app();
    call(&app, Method::PUT, "/idx", None).await;
    let (_, r) = call(&app, Method::GET, "/idx?human=true", None).await;
    assert!(r["idx"]["settings"]["index"]["creation_date_string"].is_string());
    assert_eq!(
        r["idx"]["settings"]["index"]["version"]["created_string"],
        "9.5.5"
    );
    // Without ?human, only the raw values ES always returns (e.g. version.created) appear.
    let (_, r) = call(&app, Method::GET, "/idx", None).await;
    assert!(r["idx"]["settings"]["index"]["version"]["created_string"].is_null());
    assert!(r["idx"]["settings"]["index"]["creation_date_string"].is_null());
}

#[tokio::test]
async fn get_index_features_selects_sections() {
    let app = app();
    let body = json!({"mappings": {"properties": {"t": {"type": "text"}}}});
    call(&app, Method::PUT, "/idx", Some(body)).await;
    let (_, r) = call(&app, Method::GET, "/idx?features=aliases,settings", None).await;
    assert_eq!(r["idx"]["mappings"], json!({}));
    assert!(r["idx"].get("aliases").is_some() && r["idx"].get("settings").is_some());
}

#[tokio::test]
async fn new_indices_record_identity_and_creation_time() {
    let app = app();
    call(&app, Method::PUT, "/made", None).await;
    call(&app, Method::PUT, "/auto/_doc/1", Some(json!({}))).await;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    for name in ["made", "auto"] {
        let (_, r) = call(&app, Method::GET, &format!("/{name}/_settings"), None).await;
        let s = &r[name]["settings"]["index"];
        assert_eq!(s["uuid"].as_str().unwrap().len(), 22, "{name}");
        assert_eq!(s["version"]["created"], "9111000", "{name}");
        assert_eq!(s["provided_name"], name);
        let created: u128 = s["creation_date"].as_str().unwrap().parse().unwrap();
        assert!(now - created < 60_000, "{name}: {created}");
    }
    let (_, a) = call(&app, Method::GET, "/made/_settings", None).await;
    let (_, b) = call(&app, Method::GET, "/auto/_settings", None).await;
    assert_ne!(
        a["made"]["settings"]["index"]["uuid"],
        b["auto"]["settings"]["index"]["uuid"]
    );
}

#[tokio::test]
async fn invalid_index_names_are_rejected() {
    let app = app();
    for name in ["Upper", "-lead", "a%20b", "a%23b", "..", "a%2Cb"] {
        let (s, r) = call(&app, Method::PUT, &format!("/{name}"), None).await;
        assert_eq!(
            (s, &r["error"]["type"]),
            (
                StatusCode::BAD_REQUEST,
                &json!("invalid_index_name_exception")
            ),
            "{name}"
        );
    }
    // Recorded from Elasticsearch 9.5.5: the filename-character check (space included) runs first.
    let chars =
        r#"must not contain the following characters [' ','"','*',',','/','<','>','?','\','|']"#;
    for (name, reason) in [
        ("a%20b", chars),
        ("_a%20b", chars),
        ("Up%20per", chars),
        ("a%22b", chars),
        ("Upper", "must be lowercase"),
    ] {
        let (_, r) = call(&app, Method::PUT, &format!("/{name}"), None).await;
        let reason = format!(
            "Invalid index name [{}], {reason}",
            r["error"]["index"].as_str().unwrap()
        );
        assert_eq!(r["error"]["reason"], json!(reason), "{name}");
    }
    let (s, r) = call(&app, Method::PUT, "/Upper/_doc/1", Some(json!({}))).await;
    assert_eq!(
        (s, &r["error"]["type"]),
        (
            StatusCode::BAD_REQUEST,
            &json!("invalid_index_name_exception")
        )
    );
}

#[tokio::test]
async fn wrong_method_is_es_405() {
    let req = axum::http::Request::builder()
        .method(Method::PATCH)
        .uri("/idx/_mapping")
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = tower::ServiceExt::oneshot(app(), req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert!(resp.headers().get("allow").is_some());
    let bytes = http_body_util::BodyExt::collect(resp.into_body())
        .await
        .unwrap()
        .to_bytes();
    let r: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(r["status"], 405);
    let e = r["error"].as_str().unwrap();
    assert!(
        e.starts_with(
            "Incorrect HTTP method for uri [/idx/_mapping] and method [PATCH], allowed: [GET"
        ),
        "{e}"
    );
}
