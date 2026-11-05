//! Responses report each index's real shards: `_shards` headers, per-shard failures (grouped
//! by index and reason, as Elasticsearch's ExceptionsHelper.groupBy does), profile shard ids,
//! and cluster health.

use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

const NODE: &str = "es-mini-node";

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

/// `a` (3 shards) and `b` (1 shard), each with a document in every shard and field `f` mapped
/// as `a_type` and `b_type`.
async fn setup(a_type: &str, b_type: &str) -> axum::Router {
    let app = es_mini::app();
    for (index, shards, ty) in [("a", 3, a_type), ("b", 1, b_type)] {
        let body = json!({
            "settings": {"number_of_shards": shards},
            "mappings": {"properties": {"f": {"type": ty}}},
        });
        call(&app, "PUT", &format!("/{index}"), body).await;
        for id in 0..6 {
            let uri = format!("/{index}/_doc/{id}?refresh=true");
            call(&app, "PUT", &uri, json!({"f": "127.0.0.1"})).await;
        }
    }
    app
}

fn failure(index: &str, reason: &Value) -> Value {
    json!({"shard": 0, "index": index, "node": NODE, "reason": reason})
}

#[tokio::test]
async fn search_and_count_total_every_shard() {
    let app = setup("keyword", "keyword").await;
    let (_, r) = call(&app, "POST", "/a,b/_search", json!({})).await;
    let all = json!({"total": 4, "successful": 4, "skipped": 0, "failed": 0});
    assert_eq!(r["_shards"], all);
    let (_, r) = call(&app, "POST", "/a,b/_count", json!({})).await;
    assert_eq!(r["count"], 12);
    assert_eq!(r["_shards"], all);
}

#[tokio::test]
async fn failed_shards_are_counted_and_grouped_by_index() {
    let app = setup("keyword", "ip").await;
    let aggs = json!({"aggs": {"t": {"terms": {"field": "f", "value_type": "ip"}}}});
    let (s, r) = call(&app, "POST", "/a,b/_search", aggs.clone()).await;
    assert_eq!(s, 200, "{r}");
    let reason = json!({"type": "illegal_argument_exception", "reason": "Field type [keyword] is incompatible with specified value_type [ip]"});
    assert_eq!(
        r["_shards"],
        json!({"total": 4, "successful": 1, "skipped": 0, "failed": 3, "failures": [failure("a", &reason)]})
    );
    assert_eq!(r["hits"]["total"]["value"], 6);

    let (s, r) = call(&app, "POST", "/a/_search", aggs).await;
    assert_eq!(s, 400, "{r}");
    assert_eq!(r["error"]["root_cause"], json!([reason]));
    assert_eq!(r["error"]["failed_shards"], json!([failure("a", &reason)]));
}

#[tokio::test]
async fn profile_names_each_shard() {
    let app = setup("keyword", "keyword").await;
    let body = json!({"profile": true, "size": 0, "aggs": {"t": {"terms": {"field": "f"}}}});
    let (_, r) = call(&app, "POST", "/a,b/_search", body).await;
    let node = NODE;
    let ids: Vec<_> = (r["profile"]["shards"].as_array().unwrap().iter())
        .map(|s| (s["id"].clone(), s["index"].clone(), s["shard_id"].clone()))
        .collect();
    let id = |i: &str, n: u32| (json!(format!("[{node}][{i}][{n}]")), json!(i), json!(n));
    assert_eq!(ids, [id("a", 0), id("a", 1), id("a", 2), id("b", 0)]);
}

#[tokio::test]
async fn health_counts_every_shard() {
    let app = setup("keyword", "keyword").await;
    let (_, r) = call(&app, "GET", "/_cluster/health?level=shards", Value::Null).await;
    assert_eq!(r["active_primary_shards"], 4);
    assert_eq!(r["active_shards"], 4);
    let a = &r["indices"]["a"];
    assert_eq!(a["number_of_shards"], 3);
    assert_eq!(a["active_primary_shards"], 3);
    assert_eq!(a["active_shards"], 3);
    let shards: Vec<_> = a["shards"].as_object().unwrap().keys().cloned().collect();
    assert_eq!(shards, ["0", "1", "2"]);
    assert_eq!(a["shards"]["2"]["primary_active"], true);
    assert_eq!(r["indices"]["b"]["number_of_shards"], 1);
    let (_, r) = call(&app, "GET", "/_cluster/health/a", Value::Null).await;
    assert_eq!(r["active_primary_shards"], 3);
}

#[tokio::test]
async fn broadcasts_count_every_shard() {
    let app = setup("keyword", "keyword").await;
    // each shard's default replica is unassigned
    let all = json!({"total": 8, "successful": 4, "failed": 0});
    for (method, uri) in [
        ("POST", "/a,b/_refresh"),
        ("POST", "/a,b/_flush"),
        ("GET", "/a,b/_stats"),
    ] {
        let (_, r) = call(&app, method, uri, Value::Null).await;
        assert_eq!(r["_shards"], all, "{uri}");
    }
}

#[tokio::test]
async fn stats_list_every_shard_at_level_shards() {
    let app = setup("keyword", "keyword").await;
    let (_, r) = call(&app, "GET", "/a/_stats/docs?level=shards", Value::Null).await;
    let shards = r["indices"]["a"]["shards"].as_object().unwrap();
    assert_eq!(shards.keys().collect::<Vec<_>>(), ["0", "1", "2"]);
    let mut docs = 0;
    for copies in shards.values() {
        let [copy] = copies.as_array().unwrap().as_slice() else {
            panic!("{copies}")
        };
        let routing =
            json!({"state": "STARTED", "primary": true, "node": NODE, "relocating_node": null});
        assert_eq!(copy["routing"], routing);
        docs += copy["docs"]["count"].as_u64().unwrap();
    }
    assert_eq!(docs, 6);
    let (_, r) = call(&app, "GET", "/a/_stats", Value::Null).await;
    assert!(r["indices"]["a"].get("shards").is_none(), "{r}");
    let (_, r) = call(&app, "GET", "/a/_stats?level=cluster", Value::Null).await;
    assert!(r.get("indices").is_none(), "{r}");
}

#[tokio::test]
async fn level_must_be_known() {
    let app = setup("keyword", "keyword").await;
    let (s, r) = call(&app, "GET", "/a/_stats?level=bogus", Value::Null).await;
    assert_eq!(s, 400);
    let reason =
        "level parameter must be one of [cluster] or [indices] or [shards] but was [bogus]";
    assert_eq!(r["error"]["reason"], reason);
}

#[tokio::test]
async fn cat_shards_lists_every_shard() {
    let app = setup("keyword", "keyword").await;
    // without replicas: replica_counts_cat_shards_lists_unassigned_replicas covers them
    let settings = json!({"index": {"number_of_replicas": 0}});
    call(&app, "PUT", "/a,b/_settings", settings).await;
    let (s, r) = call(&app, "GET", "/_cat/shards/a,b?format=json", Value::Null).await;
    assert_eq!(s, 200, "{r}");
    let rows = r.as_array().unwrap();
    let ids: Vec<_> = rows
        .iter()
        .map(|r| (r["index"].clone(), r["shard"].clone()))
        .collect();
    let id = |i: &str, n: &str| (json!(i), json!(n));
    assert_eq!(
        ids,
        [id("a", "0"), id("a", "1"), id("a", "2"), id("b", "0")]
    );
    let docs: u64 = (rows.iter())
        .map(|r| r["docs"].as_str().unwrap().parse::<u64>().unwrap())
        .sum();
    assert_eq!(docs, 12);
    let keys: Vec<_> = rows[0].as_object().unwrap().keys().cloned().collect();
    let columns = [
        "index", "shard", "prirep", "state", "docs", "store", "dataset", "ip", "node",
    ];
    assert_eq!(keys, columns);
    assert_eq!(
        (&rows[0]["prirep"], &rows[0]["state"]),
        (&json!("p"), &json!("STARTED"))
    );

    let req = Request::get("/_cat/shards/b?v")
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let text = res.into_body().collect().await.unwrap().to_bytes();
    let expected = "index shard prirep state   docs store dataset ip        node\n\
                    b     0     p      STARTED    6    0b      0b 127.0.0.1 es-mini\n";
    assert_eq!(std::str::from_utf8(&text).unwrap(), expected);

    let (s, r) = call(&app, "GET", "/_cat/shards/nope", Value::Null).await;
    assert_eq!(
        (s, &r["error"]["type"]),
        (404, &json!("index_not_found_exception"))
    );
}

#[tokio::test]
async fn shard_reads_stats_count_the_request_cache_of_each_shard() {
    let app = setup("keyword", "keyword").await;
    let body = json!({"size": 0});
    call(&app, "POST", "/b/_search", body.clone()).await;
    call(&app, "POST", "/b/_search", body).await;
    let (_, r) = call(
        &app,
        "GET",
        "/b/_stats/request_cache?level=shards",
        Value::Null,
    )
    .await;
    let index = &r["indices"]["b"]["total"]["request_cache"];
    assert_eq!(
        (index["hit_count"].as_u64(), index["miss_count"].as_u64()),
        (Some(1), Some(1)),
        "{r}"
    );
    assert_eq!(
        r["indices"]["b"]["shards"]["0"][0]["request_cache"], *index,
        "{r}"
    );
}

#[tokio::test]
async fn shard_reads_searches_cache_each_shard_of_every_index() {
    let app = setup("keyword", "keyword").await;
    let search = || call(&app, "POST", "/a,b/_search", json!({"size": 0}));
    search().await;
    search().await;
    // a write to b leaves the entries of a's shards, and the next search sees it
    call(&app, "PUT", "/b/_doc/new?refresh=true", json!({"f": "x"})).await;
    let (_, r) = search().await;
    assert_eq!(r["hits"]["total"]["value"], 13, "{r}");
    let uri = "/a,b/_stats/request_cache?level=shards";
    let (_, r) = call(&app, "GET", uri, Value::Null).await;
    let counts = |c: &Value| (c["hit_count"].as_u64(), c["miss_count"].as_u64());
    let a = &r["indices"]["a"];
    for n in ["0", "1", "2"] {
        let c = &a["shards"][n][0]["request_cache"];
        assert_eq!(counts(c), (Some(2), Some(1)), "shard {n}: {r}");
    }
    let b = &r["indices"]["b"]["total"]["request_cache"];
    assert_eq!(counts(b), (Some(1), Some(2)), "{r}");
}

/// `a`: 2 shards with the default 1 replica; `b`: 2 shards and no replicas; `c`: 1 shard whose
/// `auto_expand_replicas` leaves it no replica on one node.
async fn replicated() -> axum::Router {
    let app = es_mini::app();
    for (index, settings) in [
        ("a", json!({"number_of_shards": 2})),
        ("b", json!({"number_of_shards": 2, "number_of_replicas": 0})),
        ("c", json!({"auto_expand_replicas": "0-1"})),
    ] {
        call(
            &app,
            "PUT",
            &format!("/{index}"),
            json!({"settings": settings}),
        )
        .await;
    }
    app
}

// Elasticsearch 9.5.5 on one node: `POST /a/_refresh` gives {"total":4,"successful":2}, and the
// unassigned replicas also count in `_flush` and `_stats` but not in searches.
#[tokio::test]
async fn replica_counts_broadcasts_total_unassigned_replicas() {
    let app = replicated().await;
    for (method, path) in [("POST", "_refresh"), ("POST", "_flush"), ("GET", "_stats")] {
        for (target, total, successful) in [("a", 4, 2), ("b", 2, 2), ("c", 1, 1), ("a,b", 6, 4)] {
            let uri = format!("/{target}/{path}");
            let (_, r) = call(&app, method, &uri, Value::Null).await;
            let expected = json!({"total": total, "successful": successful, "failed": 0});
            assert_eq!(r["_shards"], expected, "{uri}");
        }
    }
    let (_, r) = call(&app, "POST", "/a/_search", json!({})).await;
    assert_eq!(r["_shards"]["total"], 2, "{r}");
    let (_, r) = call(&app, "PUT", "/c/_doc/1", json!({})).await;
    assert_eq!(r["_shards"]["total"], 1, "{r}");
}

// Recorded from Elasticsearch 9.5.5 on one node with `a` (2 shards, 1 replica) and `b` (2
// shards, no replicas): 4 of 6 copies active.
#[tokio::test]
async fn replica_counts_health_is_yellow_with_unassigned_replicas() {
    let app = replicated().await;
    call(&app, "DELETE", "/c", Value::Null).await;
    let (_, r) = call(&app, "GET", "/_cluster/health?level=shards", Value::Null).await;
    let counts = |h: &Value| {
        json!([
            h["status"],
            h["active_shards"],
            h["unassigned_shards"],
            h["active_shards_percent_as_number"]
        ])
    };
    assert_eq!(
        counts(&r),
        json!(["yellow", 4, 2, 66.66666666666666]),
        "{r}"
    );
    assert_eq!(r["active_primary_shards"], 4);
    let a = &r["indices"]["a"];
    let index = json!({
        "status": "yellow", "number_of_shards": 2, "number_of_replicas": 1,
        "active_primary_shards": 2, "active_shards": 2, "relocating_shards": 0,
        "initializing_shards": 0, "unassigned_shards": 2, "unassigned_primary_shards": 0,
    });
    let shard = json!({
        "status": "yellow", "primary_active": true, "active_shards": 1, "relocating_shards": 0,
        "initializing_shards": 0, "unassigned_shards": 1, "unassigned_primary_shards": 0,
    });
    let mut expected = index.clone();
    expected["shards"] = json!({"0": shard, "1": shard});
    assert_eq!(*a, expected);
    let b = &r["indices"]["b"];
    assert_eq!(
        (&b["status"], &b["number_of_replicas"]),
        (&json!("green"), &json!(0))
    );
    assert_eq!(b["shards"]["0"]["status"], "green");

    let (_, r) = call(&app, "GET", "/_cluster/health/a", Value::Null).await;
    assert_eq!(counts(&r), json!(["yellow", 2, 2, 50.0]), "{r}");
    let (_, r) = call(&app, "GET", "/_cluster/health/b", Value::Null).await;
    assert_eq!(counts(&r), json!(["green", 2, 0, 100.0]), "{r}");
    let settings = json!({"index": {"number_of_replicas": 0}});
    call(&app, "PUT", "/a/_settings", settings).await;
    let (_, r) = call(&app, "GET", "/_cluster/health", Value::Null).await;
    assert_eq!(counts(&r), json!(["green", 4, 0, 100.0]), "{r}");
}

// As Elasticsearch 9.5.5 lists them on one node: each replica UNASSIGNED after its primary.
#[tokio::test]
async fn replica_counts_cat_shards_lists_unassigned_replicas() {
    let app = replicated().await;
    let (_, r) = call(&app, "GET", "/_cat/shards/a?format=json", Value::Null).await;
    let rows: Vec<_> = (r.as_array().unwrap().iter())
        .map(|r| (r["shard"].as_str().unwrap(), r["prirep"].as_str().unwrap()))
        .collect();
    assert_eq!(rows, [("0", "p"), ("0", "r"), ("1", "p"), ("1", "r")]);
    let unassigned = json!({
        "index": "a", "shard": "0", "prirep": "r", "state": "UNASSIGNED",
        "docs": null, "store": null, "dataset": null, "ip": null, "node": null,
    });
    assert_eq!(r[1], unassigned);
    let (_, r) = call(&app, "GET", "/_cat/shards/b,c?format=json", Value::Null).await;
    assert_eq!(r.as_array().unwrap().len(), 3, "{r}");

    let req = Request::get("/_cat/shards/c,a")
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let text = res.into_body().collect().await.unwrap().to_bytes();
    let expected = "a 0 p STARTED    0 0b 0b 127.0.0.1 es-mini\n\
                    a 0 r UNASSIGNED                   \n\
                    a 1 p STARTED    0 0b 0b 127.0.0.1 es-mini\n\
                    a 1 r UNASSIGNED                   \n\
                    c 0 p STARTED    0 0b 0b 127.0.0.1 es-mini\n";
    assert_eq!(std::str::from_utf8(&text).unwrap(), expected);
}

// Elasticsearch times out (408, `timed_out: true`) waiting for a status replicas keep it from.
#[tokio::test]
async fn replica_counts_health_waits_time_out_short_of_green() {
    let app = replicated().await;
    let (s, r) = call(
        &app,
        "GET",
        "/_cluster/health?wait_for_status=green&timeout=1ms",
        Value::Null,
    )
    .await;
    assert_eq!(
        (s, &r["status"], &r["timed_out"]),
        (408, &json!("yellow"), &json!(true)),
        "{r}"
    );
    for uri in [
        "/_cluster/health?wait_for_status=yellow",
        "/_cluster/health/b?wait_for_status=green",
    ] {
        let (s, r) = call(&app, "GET", uri, Value::Null).await;
        assert_eq!((s, &r["timed_out"]), (200, &json!(false)), "{uri}: {r}");
    }
}

// As Elasticsearch 9.5.5 lists them on one node: indices with unassigned replicas are yellow.
#[tokio::test]
async fn replica_counts_cat_indices_reports_health_and_replicas() {
    let app = replicated().await;
    call(&app, "PUT", "/a/_doc/1?refresh=true", json!({"x": 1})).await;
    let (_, r) = call(&app, "GET", "/_cat/indices/a,b,c?format=json", Value::Null).await;
    let rows: Vec<_> = (r.as_array().unwrap().iter())
        .map(|r| {
            let mut r = r.clone();
            assert_eq!(r["uuid"].as_str().unwrap().len(), 22, "{r}");
            r.as_object_mut().unwrap().remove("uuid");
            r
        })
        .collect();
    let row = |health, index, pri, rep, docs| {
        json!({
            "health": health, "status": "open", "index": index, "pri": pri, "rep": rep,
            "docs.count": docs, "docs.deleted": "0", "store.size": "0b",
            "pri.store.size": "0b", "dataset.size": "0b",
        })
    };
    assert_eq!(
        rows,
        [
            row("yellow", "a", "2", "1", "1"),
            row("green", "b", "2", "0", "0"),
            row("green", "c", "1", "0", "0"),
        ]
    );

    let req = Request::get("/_cat/indices/a,b?v")
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let text = res.into_body().collect().await.unwrap().to_bytes();
    let lines: Vec<&str> = std::str::from_utf8(&text).unwrap().lines().collect();
    assert_eq!(
        lines[0],
        "health status index uuid                   pri rep docs.count docs.deleted store.size pri.store.size dataset.size"
    );
    assert!(lines[1].starts_with("yellow open   a     "), "{lines:?}");
    assert!(
        lines[1]
            .ends_with("   2   1          1            0         0b             0b           0b"),
        "{lines:?}"
    );
    assert!(lines[2].starts_with("green  open   b     "), "{lines:?}");
    assert_eq!(lines.len(), 3);
}

async fn cat_text(app: &axum::Router, uri: &str) -> String {
    let req = Request::get(uri).body(Body::empty()).unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let text = res.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(text.to_vec()).unwrap()
}

#[tokio::test]
async fn stats_and_cat_shards_take_h_s_bytes_and_help() {
    let app = setup("keyword", "keyword").await;
    let settings = json!({"index": {"number_of_replicas": 0}});
    call(&app, "PUT", "/a,b/_settings", settings).await;
    let text = cat_text(&app, "/_cat/shards/a?h=sh,i&s=shard:desc&v").await;
    assert_eq!(text, "shard index\n2     a\n1     a\n0     a\n");
    let text = cat_text(&app, "/_cat/shards/b?h=store,docs&bytes=kb").await;
    assert_eq!(text, "0 6\n");
    let text = cat_text(&app, "/_cat/shards?help").await;
    assert!(
        text.starts_with("index                                         | i,idx"),
        "{text}"
    );
    assert!(text.contains("number of docs in shard"), "{text}");
    let (s, _) = call(&app, "GET", "/_cat/shards?bytes=zz", Value::Null).await;
    assert_eq!(s, 400);
}

#[tokio::test]
async fn stats_and_cat_stats_count_indexing_operations() {
    let app = setup("keyword", "keyword").await;
    call(&app, "DELETE", "/b/_doc/0", Value::Null).await;
    let (_, r) = call(&app, "GET", "/b/_stats/indexing", Value::Null).await;
    let ix = &r["_all"]["primaries"]["indexing"];
    assert_eq!(
        (&ix["index_total"], &ix["delete_total"]),
        (&json!(6), &json!(1))
    );
}

#[tokio::test]
async fn stats_and_cat_shards_report_every_optional_column() {
    let app = setup("keyword", "keyword").await;
    let (_, r) = call(&app, "GET", "/_cat/shards/b?format=json&h=*", Value::Null).await;
    let row = &r[0];
    assert_eq!(row.as_object().unwrap().len(), 79, "{row}");
    assert_eq!(row["id"], "es-mini-node");
    assert_eq!(row["seq_no.max"], "5");
    assert_eq!(row["indexing.index_total"], "6");
    assert_eq!(row["merges.total_time"], "0s");
    assert_eq!(row["bulk.total_size_in_bytes"], "0");
    let text = cat_text(&app, "/_cat/shards/b?h=sqm,completion.size,unassigned.*&v").await;
    assert!(
        text.starts_with("seq_no.max completion.size unassigned.reason"),
        "{text}"
    );
    // default columns stay the nine, without the node id
    let (_, r) = call(&app, "GET", "/_cat/shards/b?format=json", Value::Null).await;
    assert_eq!(r[0].as_object().unwrap().len(), 9);
    let (_, r) = call(
        &app,
        "GET",
        "/_cat/shards/b?format=json&h=state,unassigned.reason",
        Value::Null,
    )
    .await;
    assert_eq!(r[0]["state"], "STARTED");
}

#[tokio::test]
async fn cat_shards_lists_hidden_indices() {
    let app = es_mini::app();
    let settings = json!({"settings": {"index.hidden": true, "number_of_replicas": 0}});
    call(&app, "PUT", "/h", settings).await;
    call(
        &app,
        "PUT",
        "/v",
        json!({"settings": {"number_of_replicas": 0}}),
    )
    .await;
    // Elasticsearch 9.5.5 lists hidden indices' shards without expand_wildcards.
    assert_eq!(cat_text(&app, "/_cat/shards?h=index").await, "h\nv\n");
    assert_eq!(cat_text(&app, "/_cat/shards/*?h=index").await, "h\nv\n");
}
