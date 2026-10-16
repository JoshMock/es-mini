use crate::{app, indices_tests::call};
use axum::http::{Method, StatusCode};
use serde_json::json;

#[tokio::test]
async fn index_get_delete_with_auto_created_index() {
    let app = app();
    let (s, r) = call(&app, Method::PUT, "/idx/_doc/1", Some(json!({"a": 1}))).await;
    assert_eq!(s, StatusCode::CREATED);
    assert_eq!(
        r,
        json!({
            "_index": "idx", "_id": "1", "_version": 1, "result": "created",
            "_shards": {"total": 2, "successful": 1, "failed": 0},
            "_seq_no": 0, "_primary_term": 1
        })
    );

    let (s, r) = call(&app, Method::POST, "/idx/_doc/1", Some(json!({"a": 2}))).await;
    assert_eq!(
        (s, &r["result"], &r["_version"]),
        (StatusCode::OK, &json!("updated"), &json!(2))
    );

    let (s, r) = call(&app, Method::GET, "/idx/_doc/1", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        r,
        json!({
            "_index": "idx", "_id": "1", "_version": 2, "_seq_no": 1, "_primary_term": 1,
            "found": true, "_source": {"a": 2}
        })
    );

    let (s, r) = call(&app, Method::DELETE, "/idx/_doc/1", None).await;
    assert_eq!(
        (s, &r["result"], &r["_version"]),
        (StatusCode::OK, &json!("deleted"), &json!(3))
    );

    let (s, r) = call(&app, Method::GET, "/idx/_doc/1", None).await;
    assert_eq!(
        (s, r),
        (
            StatusCode::NOT_FOUND,
            json!({"_index": "idx", "_id": "1", "found": false})
        )
    );

    let (s, r) = call(&app, Method::DELETE, "/idx/_doc/1", None).await;
    assert_eq!(
        (s, &r["result"]),
        (StatusCode::NOT_FOUND, &json!("not_found"))
    );
}

#[tokio::test]
async fn auto_id() {
    let app = app();
    let (s, r) = call(&app, Method::POST, "/idx/_doc", Some(json!({"a": 1}))).await;
    assert_eq!((s, &r["result"]), (StatusCode::CREATED, &json!("created")));
    let id = r["_id"].as_str().unwrap();
    assert_eq!(id.len(), 20);
    let (_, r2) = call(&app, Method::POST, "/idx/_doc", Some(json!({"a": 1}))).await;
    assert_ne!(r2["_id"], r["_id"]);
    let (s, _) = call(&app, Method::GET, &format!("/idx/_doc/{id}"), None).await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test]
async fn create_vs_index() {
    let app = app();
    let (s, r) = call(&app, Method::PUT, "/idx/_create/1", Some(json!({}))).await;
    assert_eq!((s, &r["result"]), (StatusCode::CREATED, &json!("created")));
    for uri in ["/idx/_create/1", "/idx/_doc/1?op_type=create"] {
        let (s, r) = call(&app, Method::PUT, uri, Some(json!({}))).await;
        assert_eq!(s, StatusCode::CONFLICT, "{uri}");
        assert_eq!(r["error"]["type"], "version_conflict_engine_exception");
        assert_eq!(
            r["error"]["reason"],
            "[1]: version conflict, document already exists (current version [1])"
        );
    }
}

#[tokio::test]
async fn get_from_missing_index_is_index_not_found() {
    let (s, r) = call(&app(), Method::GET, "/nope/_doc/1", None).await;
    assert_eq!(
        (s, &r["error"]["type"]),
        (StatusCode::NOT_FOUND, &json!("index_not_found_exception"))
    );
}

#[tokio::test]
async fn update() {
    let app = app();
    call(&app, Method::PUT, "/idx/_doc/1", Some(json!({"a": 1}))).await;
    let (s, r) = call(
        &app,
        Method::POST,
        "/idx/_update/1",
        Some(json!({"doc": {"b": 2}})),
    )
    .await;
    assert_eq!(
        (s, &r["result"], &r["_version"]),
        (StatusCode::OK, &json!("updated"), &json!(2))
    );
    let (_, r) = call(&app, Method::GET, "/idx/_doc/1", None).await;
    assert_eq!(r["_source"], json!({"a": 1, "b": 2}));

    let (s, r) = call(
        &app,
        Method::POST,
        "/idx/_update/1",
        Some(json!({"doc": {"b": 2}})),
    )
    .await;
    assert_eq!((s, &r["result"]), (StatusCode::OK, &json!("noop")));

    let (s, r) = call(
        &app,
        Method::POST,
        "/idx/_update/2",
        Some(json!({"doc": {"b": 2}})),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(r["error"]["type"], "document_missing_exception");
    assert_eq!(r["error"]["reason"], "[2]: document missing");

    let body = json!({"doc": {"b": 2}, "doc_as_upsert": true});
    let (s, r) = call(&app, Method::POST, "/idx/_update/3", Some(body)).await;
    assert_eq!((s, &r["result"]), (StatusCode::CREATED, &json!("created")));

    let body = json!({"doc": {"b": 2}, "upsert": {"u": 1}});
    call(&app, Method::POST, "/idx/_update/4", Some(body)).await;
    let (_, r) = call(&app, Method::GET, "/idx/_doc/4", None).await;
    assert_eq!(r["_source"], json!({"u": 1}));
}

#[tokio::test]
async fn non_object_source_is_400() {
    let (s, r) = call(&app(), Method::PUT, "/idx/_doc/1", Some(json!([1]))).await;
    assert_eq!(
        (s, &r["error"]["type"]),
        (StatusCode::BAD_REQUEST, &json!("mapper_parsing_exception"))
    );
}
