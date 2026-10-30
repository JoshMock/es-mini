//! Writes go to the shard Elasticsearch's routing picks, each shard counting its own sequence
//! numbers, and reads find them there, before and after a restart.

use axum::{body::Body, http::Request};
use es_index::routing::shard_id;
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

fn scratch(name: &str) -> (std::path::PathBuf, Cleanup) {
    let dir = std::env::temp_dir().join(format!("esm-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    (dir.clone(), Cleanup(dir))
}

async fn call(app: &axum::Router, method: &str, uri: &str, body: &str) -> Value {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_owned()))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

async fn hits(app: &axum::Router) -> usize {
    let r = call(app, "POST", "/i/_search?size=100", "{}").await;
    let total = r["hits"]["total"]["value"].as_u64().unwrap() as usize;
    assert_eq!(r["hits"]["hits"].as_array().unwrap().len(), total, "{r}");
    total
}

const SHARDS: u32 = 3;

#[tokio::test]
async fn each_shard_numbers_its_own_writes() {
    let (dir, _cleanup) = scratch("shard-writes-seq");
    let app = es_mini::app_persisted(&dir).unwrap();
    let settings = r#"{"settings": {"number_of_shards": 3}}"#;
    call(&app, "PUT", "/i", settings).await;
    let mut next = [0u64; SHARDS as usize];
    let mut seq = |id: &str, routing: Option<&str>| {
        let n = &mut next[shard_id(id, routing, SHARDS) as usize];
        *n += 1;
        *n - 1
    };
    for i in 0..9 {
        let id = i.to_string();
        let r = call(&app, "PUT", &format!("/i/_doc/{id}"), r#"{"v": 1}"#).await;
        assert_eq!(r["_seq_no"], seq(&id, None), "{r}");
        assert_eq!(r["_version"], 1);
    }
    let r = call(&app, "POST", "/i/_update/4", r#"{"doc": {"v": 2}}"#).await;
    assert_eq!(
        (&r["_seq_no"], &r["_version"]),
        (&seq("4", None).into(), &2.into()),
        "{r}"
    );
    let r = call(&app, "DELETE", "/i/_doc/7", "").await;
    assert_eq!(r["_seq_no"], seq("7", None), "{r}");
    let bulk = "{\"index\": {\"_id\": \"20\"}}\n{\"v\": 1}\n{\"delete\": {\"_id\": \"3\"}}\n";
    let r = call(&app, "POST", "/i/_bulk", bulk).await;
    assert_eq!(r["items"][0]["index"]["_seq_no"], seq("20", None), "{r}");
    assert_eq!(r["items"][1]["delete"]["_seq_no"], seq("3", None), "{r}");

    let r = call(&app, "GET", "/i/_doc/4", "").await;
    assert_eq!(r["_source"], json!({"v": 2}), "{r}");
    call(&app, "POST", "/i/_refresh", "").await;
    assert_eq!(hits(&app).await, 8);
    let r = call(&app, "GET", "/i/_count", "").await;
    assert_eq!(r["count"], 8, "{r}");
    call(&app, "POST", "/i/_flush", "").await;
    drop(app);

    let app = es_mini::app_persisted(&dir).unwrap();
    assert_eq!(hits(&app).await, 8);
    let r = call(&app, "PUT", "/i/_doc/30", r#"{"v": 1}"#).await;
    assert_eq!(r["_seq_no"], seq("30", None), "{r}");
}

#[tokio::test]
async fn the_routing_parameter_picks_the_shard() {
    let (dir, _cleanup) = scratch("shard-writes-routing");
    let app = es_mini::app_persisted(&dir).unwrap();
    call(
        &app,
        "PUT",
        "/i",
        r#"{"settings": {"number_of_shards": 3}}"#,
    )
    .await;
    // routed to a shard other than its id's
    let (id, routing) = ("a", "r2");
    assert_ne!(
        shard_id(id, Some(routing), SHARDS),
        shard_id(id, None, SHARDS)
    );
    let doc = format!("/i/_doc/{id}?routing={routing}");
    let r = call(&app, "PUT", &doc, r#"{"v": 1}"#).await;
    assert_eq!(r["result"], "created", "{r}");
    let r = call(&app, "GET", &doc, "").await;
    assert_eq!(r["found"], true, "{r}");
    let r = call(&app, "GET", &format!("/i/_doc/{id}"), "").await;
    assert_eq!(r["found"], false, "{r}");

    let update = format!("/i/_update/{id}?routing={routing}");
    let r = call(&app, "POST", &update, r#"{"doc": {"v": 2}}"#).await;
    assert_eq!(
        (&r["result"], &r["_version"]),
        (&"updated".into(), &2.into()),
        "{r}"
    );
    // the same id without routing is another document, on another shard
    let r = call(&app, "PUT", &format!("/i/_doc/{id}"), r#"{"v": 9}"#).await;
    assert_eq!(
        (&r["result"], &r["_seq_no"]),
        (&"created".into(), &0.into()),
        "{r}"
    );
    let bulk =
        format!("{{\"index\": {{\"_id\": \"b\", \"routing\": \"{routing}\"}}}}\n{{\"v\": 1}}\n");
    let r = call(&app, "POST", "/i/_bulk", &bulk).await;
    assert_eq!(r["items"][0]["index"]["_seq_no"], 2, "{r}");
    let r = call(
        &app,
        "POST",
        &format!("/i/_bulk?routing={routing}"),
        "{\"delete\": {\"_id\": \"b\"}}\n",
    )
    .await;
    assert_eq!(r["items"][0]["delete"]["result"], "deleted", "{r}");
    call(&app, "POST", "/i/_refresh", "").await;
    assert_eq!(hits(&app).await, 2);
    drop(app);

    let app = es_mini::app_persisted(&dir).unwrap();
    let r = call(&app, "GET", &doc, "").await;
    assert_eq!(r["_source"], json!({"v": 2}), "{r}");
    let r = call(&app, "DELETE", &doc, "").await;
    assert_eq!(r["result"], "deleted", "{r}");
    let r = call(&app, "GET", &format!("/i/_doc/{id}"), "").await;
    assert_eq!(r["_source"], json!({"v": 9}), "{r}");
}
