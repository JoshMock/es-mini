use crate::{app, indices_tests::call};
use axum::http::{Method, StatusCode};
use serde_json::json;

async fn seeded() -> axum::Router {
    let app = app();
    for (id, t) in [("1", "red apple"), ("2", "green apple"), ("3", "pear")] {
        call(
            &app,
            Method::PUT,
            &format!("/idx/_doc/{id}"),
            Some(json!({ "t": t })),
        )
        .await;
    }
    app
}

fn apples() -> Option<serde_json::Value> {
    Some(json!({"query": {"match": {"t": "apple"}}}))
}

#[tokio::test]
async fn by_query_update_reindexes_matches() {
    let app = seeded().await;
    let (s, r) = call(&app, Method::POST, "/idx/_update_by_query", apples()).await;
    assert_eq!(s, StatusCode::OK, "{r}");
    for (k, v) in [
        ("total", json!(2)),
        ("updated", json!(2)),
        ("deleted", json!(0)),
        ("batches", json!(1)),
        ("version_conflicts", json!(0)),
        ("noops", json!(0)),
        ("failures", json!([])),
        ("timed_out", json!(false)),
    ] {
        assert_eq!(r[k], v, "{k}: {r}");
    }
    assert!(r["took"].is_u64(), "{r}");
    let (_, d) = call(&app, Method::GET, "/idx/_doc/1", None).await;
    assert_eq!(
        (d["_version"].clone(), d["_source"].clone()),
        (json!(2), json!({"t": "red apple"}))
    );
    let (_, d) = call(&app, Method::GET, "/idx/_doc/3", None).await;
    assert_eq!(d["_version"], 1);
}

#[tokio::test]
async fn by_query_delete_removes_matches() {
    let app = seeded().await;
    let (s, r) = call(&app, Method::POST, "/idx/_delete_by_query", apples()).await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(
        (r["total"].clone(), r["deleted"].clone()),
        (json!(2), json!(2)),
        "{r}"
    );
    assert!(r.get("updated").is_none(), "{r}");
    assert_eq!(r["batches"], 1);
    let (_, c) = call(&app, Method::GET, "/idx/_count", None).await;
    assert_eq!(c["count"], 1);
    let (s, _) = call(&app, Method::GET, "/idx/_doc/3", None).await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test]
async fn by_query_without_matches_runs_no_batches() {
    let app = seeded().await;
    let body = Some(json!({"query": {"match": {"t": "plum"}}}));
    let (_, r) = call(&app, Method::POST, "/idx/_delete_by_query", body).await;
    assert_eq!(
        (
            r["total"].clone(),
            r["deleted"].clone(),
            r["batches"].clone()
        ),
        (json!(0), json!(0), json!(0)),
        "{r}"
    );
}

#[tokio::test]
async fn by_query_max_docs_limits_body_and_param() {
    let app = seeded().await;
    let body = Some(json!({"query": {"match_all": {}}, "max_docs": 1}));
    let (_, r) = call(&app, Method::POST, "/idx/_delete_by_query", body).await;
    assert_eq!(
        (r["total"].clone(), r["deleted"].clone()),
        (json!(1), json!(1)),
        "{r}"
    );
    let (_, r) = call(&app, Method::POST, "/idx/_update_by_query?max_docs=1", None).await;
    assert_eq!(
        (r["total"].clone(), r["updated"].clone()),
        (json!(1), json!(1)),
        "{r}"
    );
}

#[tokio::test]
async fn by_query_q_param_selects_docs() {
    let app = seeded().await;
    let (_, r) = call(&app, Method::POST, "/idx/_delete_by_query?q=t:pear", None).await;
    assert_eq!(r["deleted"], 1, "{r}");
}

#[tokio::test]
async fn by_query_conflicts_accepts_proceed_and_abort_only() {
    let app = seeded().await;
    let (s, _) = call(
        &app,
        Method::POST,
        "/idx/_update_by_query?conflicts=proceed",
        apples(),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let body = Some(json!({"conflicts": "abort"}));
    let (s, _) = call(&app, Method::POST, "/idx/_update_by_query", body).await;
    assert_eq!(s, StatusCode::OK);
    let (s, r) = call(
        &app,
        Method::POST,
        "/idx/_delete_by_query?conflicts=maybe",
        apples(),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{r}");
    assert!(
        r["error"]["reason"].as_str().unwrap().contains("conflicts"),
        "{r}"
    );
}

#[tokio::test]
async fn by_query_rejects_scripts() {
    let app = seeded().await;
    let body = Some(json!({"script": {"source": "ctx._source.n = 1"}}));
    let (s, r) = call(&app, Method::POST, "/idx/_update_by_query", body).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{r}");
    assert!(
        r["error"]["reason"].as_str().unwrap().contains("script"),
        "{r}"
    );
    let (_, d) = call(&app, Method::GET, "/idx/_doc/1", None).await;
    assert_eq!(d["_version"], 1);
}

#[tokio::test]
async fn by_query_missing_index_is_404() {
    let app = app();
    let (s, r) = call(&app, Method::POST, "/nope/_delete_by_query", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{r}");
}
