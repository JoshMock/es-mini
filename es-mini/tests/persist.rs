//! Writes through one server over a data directory, "restarts" on the same directory, reads back.

use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

async fn call(app: &axum::Router, method: &str, uri: &str, body: Value) -> Value {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

#[tokio::test]
async fn documents_survive_restart() {
    let dir = std::env::temp_dir().join(format!("esm-persist-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let app = es_mini::app_persisted(&dir).unwrap();
    call(
        &app,
        "PUT",
        "/books",
        json!({"mappings": {"properties": {"n": {"type": "long"}}}}),
    )
    .await;
    call(
        &app,
        "PUT",
        "/books/_doc/a%2Fb",
        json!({"title": "Dune", "n": 1}),
    )
    .await;
    call(
        &app,
        "PUT",
        "/books/_doc/2",
        json!({"title": "Emma", "n": 2}),
    )
    .await;
    call(
        &app,
        "PUT",
        "/books/_doc/2",
        json!({"title": "Emma 2", "n": 3}),
    )
    .await;
    call(&app, "DELETE", "/books/_doc/zzz", json!({})).await;
    call(&app, "PUT", "/empty", json!({})).await;
    drop(app);

    assert!(dir.join("_state").is_dir());
    let app = es_mini::app_persisted(&dir).unwrap();
    let doc = call(&app, "GET", "/books/_doc/a%2Fb", json!({})).await;
    assert_eq!(doc["_source"], json!({"title": "Dune", "n": 1}));
    let doc = call(&app, "GET", "/books/_doc/2", json!({})).await;
    assert_eq!(doc["_source"], json!({"title": "Emma 2", "n": 3}));
    assert_eq!(doc["_version"], 2);
    let hits = call(
        &app,
        "POST",
        "/books/_search",
        json!({"query": {"term": {"title.keyword": "Dune"}}}),
    )
    .await;
    assert_eq!(hits["hits"]["total"]["value"], 1);
    let m = call(&app, "GET", "/books/_mapping", json!({})).await;
    assert_eq!(m["books"]["mappings"]["properties"]["n"]["type"], "long");
    assert_eq!(call(&app, "HEAD", "/empty", json!({})).await, Value::Null);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_write_that_cannot_be_saved_fails() {
    use std::os::unix::fs::PermissionsExt;
    let root = std::env::temp_dir().join(format!("esm-persist-ro-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let app = es_mini::app_persisted(&root.join("data")).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o555)).unwrap();
    let req = Request::builder()
        .method("PUT")
        .uri("/idx")
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(res.status(), 500);
    let body: Value =
        serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body["status"], 500);
    assert!(
        body["error"]["reason"]
            .as_str()
            .unwrap()
            .contains("not saved")
    );
}

#[tokio::test]
async fn writes_whose_path_ends_like_a_read_endpoint_survive_restart() {
    let dir = std::env::temp_dir().join(format!("esm-persist-suffix-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let app = es_mini::app_persisted(&dir).unwrap();
    call(&app, "PUT", "/my_query", json!({})).await;
    call(&app, "PUT", "/my_query/_doc/my_count", json!({"n": 1})).await;
    call(&app, "PUT", "/my_query/_doc/x_search", json!({"n": 2})).await;
    drop(app);
    let app = es_mini::app_persisted(&dir).unwrap();
    call(&app, "DELETE", "/my_query/_doc/x_search", json!({})).await;
    drop(app);

    let app = es_mini::app_persisted(&dir).unwrap();
    let doc = call(&app, "GET", "/my_query/_doc/my_count", json!({})).await;
    assert_eq!(doc["_source"], json!({"n": 1}));
    let doc = call(&app, "GET", "/my_query/_doc/x_search", json!({})).await;
    assert_eq!(doc["found"], false);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn posted_reads_do_not_save() {
    use std::os::unix::fs::PermissionsExt;
    let root = std::env::temp_dir().join(format!("esm-persist-reads-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let app = es_mini::app_persisted(&root.join("data")).unwrap();
    call(&app, "PUT", "/idx", json!({})).await;
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o555)).unwrap();
    let mut statuses = vec![];
    for uri in ["/idx/_search", "/_search", "/idx/_count", "/_count"] {
        let req = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from("{}"))
            .unwrap();
        statuses.push(app.clone().oneshot(req).await.unwrap().status());
    }
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
    let _ = std::fs::remove_dir_all(&root);
    assert!(statuses.iter().all(|s| s.is_success()), "{statuses:?}");
}

#[tokio::test]
async fn posted_template_and_msearch_reads_do_not_save() {
    use std::os::unix::fs::PermissionsExt;
    let root = std::env::temp_dir().join(format!("esm-persist-tmpl-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    // No index yet, so any save would have to write the data directory's first state.
    let app = es_mini::app_persisted(&root.join("data")).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o555)).unwrap();
    let template = r#"{"source": {"query": {"match_all": {}}}}"#;
    let mut statuses = vec![];
    for (uri, body) in [
        ("/_search/template", template.to_owned()),
        ("/_msearch", "{}\n{}\n".to_owned()),
        ("/_msearch/template", format!("{{}}\n{template}\n")),
    ] {
        let req = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap();
        statuses.push(app.clone().oneshot(req).await.unwrap().status());
    }
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
    let _ = std::fs::remove_dir_all(&root);
    assert!(statuses.iter().all(|s| s.is_success()), "{statuses:?}");
}

#[tokio::test]
async fn graceful_shutdown_flushes() {
    let dir = std::env::temp_dir().join(format!("esm-persist-shutdown-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let app = es_mini::app_persisted(&dir).unwrap();
    let mappings = json!({"properties": {"title": {"type": "keyword"}}});
    call(&app, "PUT", "/books", json!({ "mappings": mappings })).await;
    call(&app, "PUT", "/books/_doc/1", json!({"title": "Dune"})).await;
    drop(app);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    es_mini::serve_persisted(listener, &dir, async {})
        .await
        .unwrap();

    // Without the translog, only committed segments hold the document.
    for ix in std::fs::read_dir(dir.join("indices")).unwrap() {
        std::fs::remove_dir_all(ix.unwrap().path().join("0/translog")).unwrap();
    }
    let app = es_mini::app_persisted(&dir).unwrap();
    let doc = call(&app, "GET", "/books/_doc/1", json!({})).await;
    assert_eq!(doc["_source"], json!({"title": "Dune"}));
    let _ = std::fs::remove_dir_all(&dir);
}
