//! PUT _mapping changing a keyword's `ignore_above` applies to documents indexed afterwards
//! only. The expected answers are Elasticsearch 9.5.5's for the same requests.

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

async fn limit(app: &axum::Router, n: u32) {
    let body = json!({"properties": {"k": {"type": "keyword", "ignore_above": n}}});
    let r = call(app, "PUT", "/t/_mapping", body).await;
    assert_eq!(r, json!({"acknowledged": true}));
}

async fn put(app: &axum::Router, id: &str, k: Value) {
    let r = call(
        app,
        "PUT",
        &format!("/t/_doc/{id}?refresh=true"),
        json!({"k": k}),
    )
    .await;
    assert_eq!(r["result"], "created", "{r}");
}

/// The ids of the hits of `body`, sorted.
async fn ids(app: &axum::Router, body: Value) -> Vec<String> {
    let r = call(app, "POST", "/t/_search", body).await;
    let mut ids: Vec<String> = (r["hits"]["hits"]
        .as_array()
        .unwrap_or_else(|| panic!("{r}"))
        .iter())
    .map(|h| h["_id"].as_str().unwrap().to_owned())
    .collect();
    ids.sort();
    ids
}

/// Each hit's id and `sort` or `fields`, in order.
async fn hits(app: &axum::Router, body: Value, what: &str) -> Vec<Value> {
    let r = call(app, "POST", "/t/_search", body).await;
    (r["hits"]["hits"]
        .as_array()
        .unwrap_or_else(|| panic!("{r}"))
        .iter())
    .map(|h| json!([h["_id"], h[what]]))
    .collect()
}

async fn check(app: &axum::Router, how: &str) {
    let term = |v: &str| json!({"query": {"term": {"k": v}}});
    assert_eq!(ids(app, term("abcdefg")).await, ["d"], "{how}");
    assert_eq!(ids(app, term("abc")).await, ["a", "c"], "{how}");
    let exists = json!({"query": {"exists": {"field": "k"}}});
    assert_eq!(ids(app, exists).await, ["a", "c", "d", "e", "g"], "{how}");
    let range = json!({"query": {"range": {"k": {"gte": "abcd"}}}});
    assert_eq!(ids(app, range).await, ["d", "e"], "{how}");
    let ignored = json!({"query": {"term": {"_ignored": "k"}}});
    assert_eq!(ids(app, ignored).await, ["b", "c", "e", "f", "g"], "{how}");

    let asc = json!({"sort": [{"k": "asc"}]});
    let want = json!([
        ["g", ["ab"]],
        ["a", ["abc"]],
        ["c", ["abc"]],
        ["e", ["abcd"]],
        ["d", ["abcdefg"]],
        ["b", [null]],
        ["f", [null]]
    ]);
    assert_eq!(Value::from(hits(app, asc, "sort").await), want, "{how}");
    let desc = json!({"sort": [{"k": "desc"}]});
    let want = json!([
        ["d", ["abcdefg"]],
        ["e", ["abcd"]],
        ["a", ["abc"]],
        ["c", ["abc"]],
        ["g", ["ab"]],
        ["b", [null]],
        ["f", [null]]
    ]);
    assert_eq!(Value::from(hits(app, desc, "sort").await), want, "{how}");

    let aggs = json!({"size": 0, "aggs": {
        "k": {"terms": {"field": "k"}}, "i": {"terms": {"field": "_ignored"}}}});
    let r = call(app, "POST", "/t/_search", aggs).await;
    let buckets = |a: &str| r["aggregations"][a]["buckets"].clone();
    let want = json!([{"key": "abc", "doc_count": 2}, {"key": "ab", "doc_count": 1},
        {"key": "abcd", "doc_count": 1}, {"key": "abcdefg", "doc_count": 1}]);
    assert_eq!(buckets("k"), want, "{how}");
    assert_eq!(buckets("i"), json!([{"key": "k", "doc_count": 5}]), "{how}");

    // `fields` reads `_source` with the current mapping, `docvalue_fields` what was indexed.
    let fetched = async |fields: &str| {
        let top = json!({"size": 10, "_source": false, (fields): ["k"]});
        let body = json!({"size": 0, "aggs": {"h": {"top_hits": top}}});
        let r = call(app, "POST", "/t/_search", body).await;
        let mut hits: Vec<Value> = (r["aggregations"]["h"]["hits"]["hits"].as_array())
            .unwrap_or_else(|| panic!("{r}"))
            .iter()
            .map(|h| json!([h["_id"], h["fields"]]))
            .collect();
        hits.sort_by_key(|h| h[0].to_string());
        Value::from(hits)
    };
    let want = json!([["a", null], ["b", null], ["c", null], ["d", null], ["e", null],
        ["f", null], ["g", {"k": ["ab"]}]]);
    assert_eq!(fetched("fields").await, want, "{how}");
    let want = json!([["a", {"k": ["abc"]}], ["b", null], ["c", {"k": ["abc"]}],
        ["d", {"k": ["abcdefg"]}], ["e", {"k": ["abcd"]}], ["f", null], ["g", {"k": ["ab"]}]]);
    assert_eq!(fetched("docvalue_fields").await, want, "{how}");
}

#[tokio::test]
async fn ignore_above_changes_apply_to_later_documents() {
    let dir = std::env::temp_dir().join(format!("esm-ignore-above-{}", std::process::id()));
    let _cleanup = Cleanup(dir.clone());
    let _ = std::fs::remove_dir_all(&dir);
    let app = es_mini::app_persisted(&dir).unwrap();
    let mappings = json!({"properties": {"k": {"type": "keyword", "ignore_above": 5}}});
    call(&app, "PUT", "/t", json!({"mappings": mappings})).await;
    put(&app, "a", json!("abc")).await;
    put(&app, "b", json!("abcdefg")).await;
    put(&app, "c", json!(["abc", "abcdefghij"])).await;
    limit(&app, 8).await;
    put(&app, "d", json!("abcdefg")).await;
    put(&app, "e", json!(["abcdefghij", "abcd"])).await;
    limit(&app, 2).await;
    put(&app, "f", json!("abc")).await;
    put(&app, "g", json!(["ab", "abcdefg"])).await;
    check(&app, "in memory").await;

    // f and g are in the translog only, so the restart replays them.
    drop(app);
    let app = es_mini::app_persisted(&dir).unwrap();
    check(&app, "after replaying the translog").await;
    call(&app, "POST", "/t/_flush", json!({})).await;
    check(&app, "after a flush").await;
    drop(app);
    let app = es_mini::app_persisted(&dir).unwrap();
    check(&app, "after reopening the flushed index").await;
}
