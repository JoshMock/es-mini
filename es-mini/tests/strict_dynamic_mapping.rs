//! `dynamic` at the root and on object fields, as Elasticsearch's DocumentParser applies it:
//! `strict` refuses documents with new fields, `false` keeps them in `_source` only. The
//! expected answers are Elasticsearch 9.5.5's for the same requests.

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

/// Elasticsearch's response to a write refused by `dynamic: strict`.
fn strict(at: &str, field: &str, parent: &str) -> Value {
    let reason = format!(
        "[{at}] mapping set to strict, dynamic introduction of [{field}] within [{parent}] is not allowed"
    );
    let e = json!({"type": "strict_dynamic_mapping_exception", "reason": reason});
    json!({"error": {"root_cause": [e], "type": e["type"], "reason": e["reason"]}, "status": 400})
}

const MAPPINGS: &str = r#"{"mappings":{"dynamic":"strict","properties":{"a":{"type":"keyword"},
  "o":{"properties":{"x":{"type":"long"},"p":{"properties":{"q":{"type":"long"}}}}},
  "t":{"dynamic":true,"properties":{"y":{"type":"long"}}},
  "f":{"dynamic":"false","properties":{"k":{"type":"long"}}},
  "e":{"type":"object","dynamic":"strict"}}}}"#;

fn created_mapping() -> Value {
    json!({"s": {"mappings": {"dynamic": "strict", "properties": {
        "a": {"type": "keyword"},
        "e": {"type": "object", "dynamic": "strict"},
        "f": {"dynamic": "false", "properties": {"k": {"type": "long"}}},
        "o": {"properties": {"p": {"properties": {"q": {"type": "long"}}}, "x": {"type": "long"}}},
        "t": {"dynamic": "true", "properties": {"y": {"type": "long"}}}}}}})
}

fn final_mapping() -> Value {
    let mut m = created_mapping();
    m["s"]["mappings"]["properties"]["t"]["properties"] = json!({
        "n": {"properties": {"m": {"type": "long"}}},
        "n2": {"properties": {"m": {"type": "long"}}},
        "y": {"type": "long"}, "z": {"type": "long"}});
    m
}

/// The writes `dynamic: strict` refuses, with where Elasticsearch locates them.
async fn check_refused(app: &axum::Router, how: &str) {
    let refused = [
        (r#"{"a":"x","b":1}"#, strict("1:14", "b", "_doc")),
        (
            "{\"a\": \"x\",\n  \"b\": \"str\"}",
            strict("2:8", "b", "_doc"),
        ),
        (r#"{"a":"x","o":{"z":1}}"#, strict("1:19", "z", "o")),
        (r#"{"o":{"p":{"z":{"w":1}}}}"#, strict("1:16", "z", "o.p")),
        (r#"{"e":{"z":1}}"#, strict("1:11", "z", "e")),
        (r#"{"b":null}"#, strict("1:6", "b", "_doc")),
        (r#"{"b":[1]}"#, strict("1:6", "b", "_doc")),
        (r#"{"b":[]}"#, strict("1:6", "b", "_doc")),
        (r#"{"b":{"c":1}}"#, strict("1:6", "b", "_doc")),
        (r#"{"b":{}}"#, strict("1:6", "b", "_doc")),
        (r#"{"o.z":1}"#, strict("1:8", "z", "o")),
        (r#"{"b.c":1}"#, strict("1:2", "b", "_doc")),
    ];
    for (doc, want) in refused {
        assert_eq!(
            call(app, "PUT", "/s/_doc/2", doc).await,
            want,
            "{how}: {doc}"
        );
    }
    let r = call(app, "PUT", "/s/_create/3", r#"{"b":1}"#).await;
    assert_eq!(r, strict("1:6", "b", "_doc"), "{how}");
}

#[tokio::test]
async fn strict_refuses_new_fields_at_the_root_and_in_objects() {
    let dir = std::env::temp_dir().join(format!("esm-strict-dynamic-{}", std::process::id()));
    let _cleanup = Cleanup(dir.clone());
    let _ = std::fs::remove_dir_all(&dir);
    let app = es_mini::app_persisted(&dir).unwrap();
    let r = call(&app, "PUT", "/s", MAPPINGS).await;
    assert_eq!(r["acknowledged"], true, "{r}");
    assert_eq!(
        call(&app, "GET", "/s/_mapping", "").await,
        created_mapping()
    );
    check_refused(&app, "in memory").await;

    // `dynamic: true` on `t` lets its sub-objects (dotted or not) map; `false` on `f` drops.
    for (id, doc) in [
        ("1", r#"{"a":"x","t":{"z":1,"n":{"m":1}}}"#),
        ("1", r#"{"a":"x","f":{"z":1,"k":2,"n":{"m":1}}}"#),
        ("1", r#"{"e":{}}"#),
        ("2", r#"{"t.n2.m":1}"#),
    ] {
        let r = call(&app, "PUT", &format!("/s/_doc/{id}"), doc).await;
        assert!(r["result"].is_string(), "{doc}: {r}");
    }

    // Updates locate the field in the merged document, upserts in the upsert.
    let updates = [
        ("1", r#"{"doc":{"b":1}}"#, strict("1:13", "b", "_doc")),
        (
            "1",
            r#"{"doc":{"a":"y","o":{"q":5}}}"#,
            strict("1:26", "q", "o"),
        ),
        (
            "9",
            r#"{"doc":{"a":"x"},"upsert":{"b":1}}"#,
            strict("1:6", "b", "_doc"),
        ),
        (
            "9",
            r#"{"doc":{"b":1},"doc_as_upsert":true}"#,
            strict("1:6", "b", "_doc"),
        ),
    ];
    for (id, body, want) in updates {
        let r = call(&app, "POST", &format!("/s/_update/{id}"), body).await;
        assert_eq!(r, want, "{body}");
    }

    let bulk = "{\"index\":{\"_id\":\"5\"}}\n{\"b\":1}\n{\"create\":{\"_id\":\"6\"}}\n\
        {\"a\":\"x\",\"zz\":true}\n{\"update\":{\"_id\":\"1\"}}\n{\"doc\":{\"c\":1}}\n\
        {\"index\":{\"_id\":\"7\"}}\n{\"a\":\"ok\"}\n";
    let mut r = call(&app, "POST", "/s/_bulk", bulk).await;
    r.as_object_mut().unwrap().remove("took");
    let item = |op: &str, id: &str, at: &str, field: &str| {
        let e = &strict(at, field, "_doc")["error"];
        let error = json!({"type": e["type"], "reason": e["reason"]});
        json!({op: {"_index": "s", "_id": id, "status": 400, "error": error}})
    };
    let shards = json!({"total": 2, "successful": 1, "failed": 0});
    let want = json!({"errors": true, "items": [
        item("index", "5", "1:6", "b"),
        item("create", "6", "1:15", "zz"),
        item("update", "1", "1:13", "c"),
        {"index": {"_index": "s", "_id": "7", "_version": 1, "result": "created",
            "_shards": shards, "_seq_no": 4, "_primary_term": 1, "status": 201}}]});
    assert_eq!(r, want);
    assert_eq!(call(&app, "GET", "/s/_mapping", "").await, final_mapping());
    let r = call(&app, "GET", "/s/_doc/1", "").await;
    assert_eq!(r["_source"], json!({"e": {}}));

    drop(app);
    let app = es_mini::app_persisted(&dir).unwrap();
    assert_eq!(call(&app, "GET", "/s/_mapping", "").await, final_mapping());
    check_refused(&app, "after reopening").await;
}

#[tokio::test]
async fn dynamic_false_keeps_new_fields_in_source_only() {
    let dir = std::env::temp_dir().join(format!("esm-false-dynamic-{}", std::process::id()));
    let _cleanup = Cleanup(dir.clone());
    let _ = std::fs::remove_dir_all(&dir);
    let app = es_mini::app_persisted(&dir).unwrap();
    call(&app, "PUT", "/f2", r#"{"mappings":{"dynamic":false}}"#).await;
    let r = call(&app, "PUT", "/f2/_doc/1", r#"{"o":{"p":1},"x":"y"}"#).await;
    assert_eq!(r["result"], "created", "{r}");
    let want = json!({"f2": {"mappings": {"dynamic": "false"}}});
    assert_eq!(call(&app, "GET", "/f2/_mapping", "").await, want);

    // PUT _mapping switches the root to `false`, then back to `strict`.
    call(
        &app,
        "PUT",
        "/s",
        r#"{"mappings":{"properties":{"a":{"type":"keyword"}}}}"#,
    )
    .await;
    call(&app, "PUT", "/s/_doc/1?refresh=true", r#"{"a":"x"}"#).await;
    let bad = r#"{"error":{"root_cause":[{"type":"mapper_parsing_exception","reason":"Failed to parse mapping: Could not convert [dynamic.dynamic] to boolean"}],"type":"mapper_parsing_exception","reason":"Failed to parse mapping: Could not convert [dynamic.dynamic] to boolean","caused_by":{"type":"illegal_argument_exception","reason":"Could not convert [dynamic.dynamic] to boolean","caused_by":{"type":"illegal_argument_exception","reason":"Failed to parse value [False] as only [true] or [false] are allowed."}}},"status":400}"#;
    let r = call(&app, "PUT", "/s/_mapping", r#"{"dynamic":"False"}"#).await;
    assert_eq!(r, serde_json::from_str::<Value>(bad).unwrap());
    let r = call(&app, "PUT", "/bad", r#"{"mappings":{"dynamic":"False"}}"#).await;
    assert_eq!(r, serde_json::from_str::<Value>(bad).unwrap());
    let r = call(&app, "PUT", "/s/_mapping", r#"{"dynamic":"STRICT"}"#).await;
    assert_eq!(r, json!({"acknowledged": true}));
    let r = call(&app, "PUT", "/s/_mapping", r#"{"dynamic":false}"#).await;
    assert_eq!(r, json!({"acknowledged": true}));
    let doc = r#"{"b":1,"nn":{"x":"y"}}"#;
    let r = call(&app, "PUT", "/s/_doc/4?refresh=true", doc).await;
    assert_eq!(r["result"], "created", "{r}");
    let want = json!({"s": {"mappings": {"dynamic": "false",
        "properties": {"a": {"type": "keyword"}}}}});
    assert_eq!(call(&app, "GET", "/s/_mapping", "").await, want);
    let check = async |app: &axum::Router, how: &str| {
        let r = call(app, "GET", "/s/_doc/4", "").await;
        assert_eq!(r["_source"], json!({"b": 1, "nn": {"x": "y"}}), "{how}");
    };
    check(&app, "in memory").await;
    drop(app);
    let app = es_mini::app_persisted(&dir).unwrap();
    check(&app, "after reopening").await;
    assert_eq!(call(&app, "GET", "/s/_mapping", "").await, want);

    // Reindexing in place under `strict` refuses the document holding unmapped fields.
    call(&app, "PUT", "/s/_mapping", r#"{"dynamic":"strict"}"#).await;
    let mut r = call(&app, "POST", "/s/_update_by_query?refresh=true", "{}").await;
    r.as_object_mut().unwrap().remove("took");
    let cause = &strict("1:6", "b", "_doc")["error"];
    let cause = json!({"type": cause["type"], "reason": cause["reason"]});
    let want = json!({"timed_out": false, "total": 2, "updated": 1, "deleted": 0,
        "batches": 1, "version_conflicts": 0, "noops": 0, "retries": {"bulk": 0, "search": 0},
        "throttled_millis": 0, "requests_per_second": -1.0, "throttled_until_millis": 0,
        "failures": [{"index": "s", "id": "4", "cause": cause, "status": 400}]});
    assert_eq!(r, want);
}

#[tokio::test]
async fn object_dynamic_is_set_by_put_mapping_and_inherited() {
    let app = es_mini::app();
    let body =
        r#"{"mappings":{"dynamic":"strict","properties":{"o":{"type":"object","dynamic":true}}}}"#;
    call(&app, "PUT", "/tt", body).await;
    let r = call(&app, "GET", "/tt/_mapping", "").await;
    let want = json!({"tt": {"mappings": {"dynamic": "strict",
        "properties": {"o": {"type": "object", "dynamic": "true"}}}}});
    assert_eq!(r, want);
    let r = call(&app, "PUT", "/tt/_doc/1", r#"{"o":{"a":1}}"#).await;
    assert_eq!(r["result"], "created", "{r}");
    let r = call(&app, "GET", "/tt/_mapping", "").await;
    let want = json!({"tt": {"mappings": {"dynamic": "strict",
        "properties": {"o": {"dynamic": "true", "properties": {"a": {"type": "long"}}}}}}});
    assert_eq!(r, want);

    let body =
        r#"{"properties":{"newobj":{"dynamic":"strict","properties":{"x":{"type":"long"}}}}}"#;
    let r = call(&app, "PUT", "/tt/_mapping", body).await;
    assert_eq!(r, json!({"acknowledged": true}));
    let r = call(&app, "PUT", "/tt/_doc/8", r#"{"newobj":{"zz":1}}"#).await;
    assert_eq!(r, strict("1:17", "zz", "newobj"));

    let body = r#"{"mappings":{"properties":{"ns":{"type":"nested","dynamic":"strict","properties":{"x":{"type":"long"}}}}}}"#;
    call(&app, "PUT", "/n", body).await;
    let want = json!({"n": {"mappings": {"properties": {"ns": {"type": "nested",
        "dynamic": "strict", "properties": {"x": {"type": "long"}}}}}}});
    assert_eq!(call(&app, "GET", "/n/_mapping", "").await, want);
    let r = call(&app, "PUT", "/n/_doc/1", r#"{"ns":[{"x":1,"y":2}]}"#).await;
    assert_eq!(r, strict("1:19", "y", "ns"));
}

#[tokio::test]
async fn searches_find_nothing_in_fields_dynamic_false_left_unmapped() {
    let app = es_mini::app();
    let body = r#"{"mappings":{"dynamic":false,"properties":{"a":{"type":"keyword"},
      "o":{"properties":{"x":{"type":"long"}}}}}}"#;
    call(&app, "PUT", "/f", body).await;
    let doc = r#"{"a":"x","b":1,"c":"hello world","o":{"x":1,"p":"z"}}"#;
    call(&app, "PUT", "/f/_doc/1?refresh=true", doc).await;
    // Elasticsearch 9.5.5's hit counts for the same queries.
    let queries = [
        (r#"{"match":{"b":1}}"#, 0),
        (r#"{"term":{"c":"hello"}}"#, 0),
        (r#"{"exists":{"field":"b"}}"#, 0),
        (r#"{"exists":{"field":"o"}}"#, 1),
        (r#"{"match":{"o.p":"z"}}"#, 0),
        (r#"{"range":{"b":{"gte":0}}}"#, 0),
        (r#"{"prefix":{"c":"he"}}"#, 0),
        (r#"{"wildcard":{"c.keyword":"he*"}}"#, 0),
        (r#"{"match_phrase":{"c":"hello world"}}"#, 0),
        (r#"{"match_phrase_prefix":{"c":"hello wo"}}"#, 0),
        (r#"{"multi_match":{"query":"hello","fields":["c","a"]}}"#, 0),
        (r#"{"match":{"o.x":1}}"#, 1),
        (r#"{"bool":{"must_not":[{"term":{"b":1}}]}}"#, 1),
    ];
    for (q, want) in queries {
        let r = call(&app, "POST", "/f/_count", &format!(r#"{{"query":{q}}}"#)).await;
        assert_eq!(r["count"], want, "{q}: {r}");
    }
}
