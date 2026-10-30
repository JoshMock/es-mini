//! Writes through one server over a data directory, "restarts" on the same directory, reads back.

use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

/// Removes a test's scratch directory when the test ends, even on panic.
struct Cleanup(std::path::PathBuf);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

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
    let _cleanup = Cleanup(dir.clone());
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
    let _cleanup = Cleanup(root.clone());
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
    let _cleanup = Cleanup(dir.clone());
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
    let _cleanup = Cleanup(root.clone());
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
    let _cleanup = Cleanup(root.clone());
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
    let _cleanup = Cleanup(dir.clone());
    let _ = std::fs::remove_dir_all(&dir);
    let app = es_mini::app_persisted(&dir).unwrap();
    let mappings = json!({"properties": {"title": {"type": "keyword"}}});
    call(&app, "PUT", "/books", json!({ "mappings": mappings })).await;
    call(&app, "PUT", "/books/_doc/1", json!({"title": "Dune"})).await;
    drop(app);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    es_mini::serve_persisted(listener, &dir, &Default::default(), async {})
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

#[tokio::test]
async fn async_durability_syncs_on_a_timer() {
    let dir = std::env::temp_dir().join(format!("esm-persist-async-{}", std::process::id()));
    let _cleanup = Cleanup(dir.clone());
    let _ = std::fs::remove_dir_all(&dir);
    let app = es_mini::app_persisted(&dir).unwrap();
    let settings = json!({"index.translog": {"durability": "async", "sync_interval": "100ms"}});
    call(&app, "PUT", "/logs", json!({ "settings": settings })).await;
    call(&app, "PUT", "/logs/_doc/1", json!({})).await;
    // The write is answered before it is logged; the timer logs it within the interval.
    let logged = || {
        es_index::disk::open(&dir).unwrap()["logs"]
            .get("1")
            .is_some()
    };
    assert!(!logged());
    for _ in 0..30 {
        if logged() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(logged());
    drop(app);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_writes_are_all_durable() {
    let dir = std::env::temp_dir().join(format!("esm-persist-group-{}", std::process::id()));
    let _cleanup = Cleanup(dir.clone());
    let _ = std::fs::remove_dir_all(&dir);
    let app = es_mini::app_persisted(&dir).unwrap();
    call(&app, "PUT", "/books", json!({})).await;
    let writes: Vec<_> = (0..64)
        .map(|i| {
            let app = app.clone();
            tokio::spawn(async move {
                call(&app, "PUT", &format!("/books/_doc/{i}"), json!({ "n": i })).await
            })
        })
        .collect();
    for w in writes {
        assert_eq!(w.await.unwrap()["result"], "created");
    }
    // Acknowledged means logged: reopen the directory as it is now, without a flush.
    let back = es_index::disk::open(&dir).unwrap();
    for i in 0..64 {
        let doc = back["books"].get(&i.to_string()).expect("doc lost");
        assert_eq!(doc.source, json!({ "n": i }));
    }
    drop(app);
    let _ = std::fs::remove_dir_all(&dir);
}
