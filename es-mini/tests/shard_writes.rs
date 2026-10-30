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

#[tokio::test]
async fn documents_keep_their_routing() {
    let (dir, _cleanup) = scratch("shard-writes-keep-routing");
    let app = es_mini::app_persisted(&dir).unwrap();
    call(
        &app,
        "PUT",
        "/i",
        r#"{"settings": {"number_of_shards": 3}}"#,
    )
    .await;
    call(&app, "PUT", "/i/_doc/a?routing=r1", r#"{"v": 1}"#).await;
    call(&app, "PUT", "/i/_doc/b", r#"{"v": 2}"#).await;
    let bulk = "{\"index\": {\"_id\": \"c\", \"routing\": \"r2\"}}\n{\"v\": 3}\n";
    call(&app, "POST", "/i/_bulk?refresh=true", bulk).await;

    // _routing follows _primary_term in a get and _score in a hit, as Elasticsearch orders them
    let check = |app: axum::Router| async move {
        let r = call(&app, "GET", "/i/_doc/a?routing=r1", "").await;
        let keys: Vec<_> = r.as_object().unwrap().keys().cloned().collect();
        let order = [
            "_index",
            "_id",
            "_version",
            "_seq_no",
            "_primary_term",
            "_routing",
            "found",
            "_source",
        ];
        assert_eq!(keys, order, "{r}");
        assert_eq!(r["_routing"], "r1", "{r}");
        let r = call(&app, "GET", "/i/_doc/b", "").await;
        assert!(r.get("_routing").is_none(), "{r}");
        let r = call(&app, "POST", "/i/_search", r#"{"sort": ["_doc"]}"#).await;
        let mut routings: Vec<_> = (r["hits"]["hits"].as_array().unwrap().iter())
            .map(|h| {
                (
                    h["_id"].as_str().unwrap().to_owned(),
                    h.get("_routing").cloned(),
                )
            })
            .collect();
        routings.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            routings,
            [
                ("a".into(), Some("r1".into())),
                ("b".into(), None),
                ("c".into(), Some("r2".into()))
            ],
            "{r}"
        );
        let hit = r["hits"]["hits"]
            .as_array()
            .unwrap()
            .iter()
            .find(|h| h["_id"] == "a");
        let keys: Vec<_> = hit.unwrap().as_object().unwrap().keys().cloned().collect();
        assert_eq!(
            keys[..5],
            ["_index", "_id", "_score", "_routing", "_source"],
            "{r}"
        );
    };
    check(app.clone()).await;
    drop(app);
    // replayed from the translog
    let app = es_mini::app_persisted(&dir).unwrap();
    check(app.clone()).await;
    call(&app, "POST", "/i/_flush", "").await;
    drop(app);
    // read from the segments
    let app = es_mini::app_persisted(&dir).unwrap();
    check(app.clone()).await;
    let r = call(
        &app,
        "POST",
        "/i/_update/a?routing=r1",
        r#"{"doc": {"v": 5}}"#,
    )
    .await;
    assert_eq!(r["result"], "updated", "{r}");
    let r = call(&app, "GET", "/i/_doc/a?routing=r1", "").await;
    assert_eq!(r["_routing"], "r1", "{r}");
}

/// The shard directories of the only index under `dir` with the committed segment `segment`.
fn committed_shards(dir: &std::path::Path, segment: &str) -> Vec<String> {
    let index = std::fs::read_dir(dir.join("indices"))
        .unwrap()
        .next()
        .unwrap();
    let mut shards: Vec<_> = std::fs::read_dir(index.unwrap().path())
        .unwrap()
        .map(|s| s.unwrap().path())
        .filter(|s| s.join(format!("index/{segment}.si")).exists())
        .map(|s| s.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    shards.sort();
    shards
}

#[tokio::test]
async fn flush_commits_every_shard() {
    let (dir, _cleanup) = scratch("shard-writes-flush");
    let app = es_mini::app_persisted(&dir).unwrap();
    call(
        &app,
        "PUT",
        "/i",
        r#"{"settings": {"number_of_shards": 3}}"#,
    )
    .await;
    for i in 0..9 {
        call(&app, "PUT", &format!("/i/_doc/{i}"), "{}").await;
    }
    assert!(committed_shards(&dir, "_0").is_empty());
    let r = call(&app, "POST", "/i/_flush", "").await;
    assert_eq!(r["_shards"]["failed"], 0, "{r}");
    assert_eq!(committed_shards(&dir, "_0"), ["0", "1", "2"]);

    call(&app, "PUT", "/i/_doc/9?routing=x", "{}").await;
    let r = call(&app, "GET", "/_flush", "").await;
    assert_eq!(r["_shards"]["failed"], 0, "{r}");
    let shard = shard_id("9", Some("x"), SHARDS).to_string();
    assert_eq!(committed_shards(&dir, "_1"), [shard]);
}

#[tokio::test]
async fn by_query_writes_keep_documents_on_their_shard() {
    let app = es_mini::app();
    call(
        &app,
        "PUT",
        "/i",
        r#"{"settings": {"number_of_shards": 3}}"#,
    )
    .await;
    // routed to a shard other than its id's
    call(
        &app,
        "PUT",
        "/i/_doc/a?routing=r2&refresh=true",
        r#"{"v": 1}"#,
    )
    .await;
    let r = call(&app, "POST", "/i/_update_by_query?refresh=true", "{}").await;
    assert_eq!(r["updated"], 1, "{r}");
    assert_eq!(hits(&app).await, 1);
    let r = call(&app, "GET", "/i/_doc/a?routing=r2", "").await;
    assert_eq!(
        (&r["_version"], &r["_routing"]),
        (&2.into(), &"r2".into()),
        "{r}"
    );
    let r = call(&app, "POST", "/i/_delete_by_query?refresh=true", "{}").await;
    assert_eq!(r["deleted"], 1, "{r}");
    assert_eq!(hits(&app).await, 0);
}

#[tokio::test]
async fn mget_reads_each_documents_routing() {
    let app = es_mini::app();
    call(
        &app,
        "PUT",
        "/i",
        r#"{"settings": {"number_of_shards": 3}}"#,
    )
    .await;
    call(&app, "PUT", "/i/_doc/a?routing=r2", r#"{"v": 1}"#).await;
    let body = r#"{"docs": [{"_id": "a", "routing": "r2"}, {"_id": "a"}]}"#;
    let r = call(&app, "POST", "/i/_mget", body).await;
    let found: Vec<_> = (r["docs"].as_array().unwrap().iter())
        .map(|d| d["found"].clone())
        .collect();
    assert_eq!(found, [true, false], "{r}");
    // the URL's routing applies to items without their own
    let r = call(&app, "POST", "/i/_mget?routing=r2", r#"{"ids": ["a"]}"#).await;
    assert_eq!(r["docs"][0]["_routing"], "r2", "{r}");
}
