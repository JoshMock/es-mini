//! Replays fixtures/generate_shard_search.sh's setup (3-shard indices) into an in-memory
//! es-mini and compares every search, scroll page and point-in-time page with the responses
//! Elasticsearch 9.5.5 recorded in fixtures/shard_search.json.

// The shared harness also serves fixture_parity and disk_parity; this test uses only part of it.
#[allow(dead_code)]
mod common;

use common::parity::{Req, diff, fixtures, report, send};
use serde_json::{Value, json};
use std::{collections::BTreeSet, fs};

/// Cases es-mini does not match yet. Like the other xfail lists it may only shrink, and goal
/// shard-search-merge is done only when it is empty.
const XFAIL: &[&str] = &[
    "search_after",
    "search_after_ties",
    "dfs",
    "terms_error",
    "scroll_doc",
    "pit_sorted",
    "pit_score",
];

fn req(method: &str, path: &str, body: &Value) -> Req {
    let body = match body {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        b => Some(b.to_string()),
    };
    Req {
        method: method.into(),
        path: path.into(),
        body,
    }
}

/// The responses to a case's requests, as generate_shard_search.sh made them.
async fn run(app: &axum::Router, case: &Value) -> Vec<Value> {
    let (path, body) = (case["path"].as_str().unwrap(), &case["body"]);
    let mut pages = vec![];
    match case["kind"].as_str().unwrap() {
        "search" => pages.push(send(app, &req("POST", path, body)).await),
        "scroll" => {
            let mut r = send(app, &req("POST", path, body)).await;
            loop {
                let id = r.as_object_mut().and_then(|o| o.remove("_scroll_id"));
                let done = r["hits"]["hits"].as_array().is_none_or(Vec::is_empty);
                pages.push(r);
                let Some(id) = id.filter(|_| !done) else {
                    break;
                };
                let next = json!({"scroll": "1m", "scroll_id": id});
                r = send(app, &req("POST", "/_search/scroll", &next)).await;
            }
        }
        "pit" => {
            let mut id = send(app, &req("POST", path, &Value::Null)).await["id"].clone();
            let mut after = Value::Null;
            loop {
                let mut b = body.clone();
                b["pit"] = json!({"id": id, "keep_alive": "1m"});
                if !after.is_null() {
                    b["search_after"] = after;
                }
                let mut r = send(app, &req("POST", "/_search", &b)).await;
                id = r.as_object_mut().and_then(|o| o.remove("pit_id")).into();
                let last = r["hits"]["hits"].as_array().and_then(|h| h.last());
                after = last.map_or(Value::Null, |h| h["sort"].clone());
                pages.push(r);
                if after.is_null() {
                    break;
                }
            }
        }
        k => panic!("unknown case kind {k}"),
    }
    for p in &mut pages {
        if let Value::Object(o) = p {
            o.remove("took");
        }
    }
    pages
}

#[tokio::test]
async fn in_memory_matches_recorded_shard_searches() {
    let recorded: Value =
        serde_json::from_slice(&fs::read(fixtures().join("shard_search.json")).unwrap()).unwrap();
    let app = es_mini::app();
    for s in recorded["setup"].as_array().unwrap() {
        let r = send(
            &app,
            &req(
                s["method"].as_str().unwrap(),
                s["path"].as_str().unwrap(),
                &s["body"],
            ),
        )
        .await;
        assert!(
            r["acknowledged"] == true || r["errors"] == false,
            "{s}: {r}"
        );
    }
    let xfail: BTreeSet<String> = XFAIL.iter().map(|s| s.to_string()).collect();
    let mut problems = vec![];
    for case in recorded["cases"].as_array().unwrap() {
        let actual = Value::Array(run(&app, case).await);
        let mut diffs = vec![];
        diff("", &case["responses"], &actual, &mut diffs);
        report(
            case["name"].as_str().unwrap(),
            &diffs,
            &xfail,
            &mut problems,
        );
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}
