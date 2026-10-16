use crate::{app, indices_tests::call};
use axum::http::{Method, StatusCode};
use serde_json::json;

async fn seeded() -> axum::Router {
    let app = app();
    call(
        &app,
        Method::PUT,
        "/idx/_doc/1",
        Some(json!({"t": "red apple", "n": 2})),
    )
    .await;
    call(
        &app,
        Method::PUT,
        "/idx/_doc/2",
        Some(json!({"t": "green apple", "n": 1})),
    )
    .await;
    call(
        &app,
        Method::PUT,
        "/idx/_doc/3",
        Some(json!({"t": "pear", "n": 3})),
    )
    .await;
    app
}

#[tokio::test]
async fn search_envelope() {
    let app = seeded().await;
    let (s, r) = call(
        &app,
        Method::POST,
        "/idx/_search",
        Some(json!({"query": {"match": {"t": "apple"}}})),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(r["timed_out"], false);
    assert!(r["took"].is_u64());
    assert_eq!(
        r["_shards"],
        json!({"total": 1, "successful": 1, "skipped": 0, "failed": 0})
    );
    assert_eq!(r["hits"]["total"], json!({"value": 2, "relation": "eq"}));
    assert!(r["hits"]["max_score"].as_f64().unwrap() > 0.0);
    let hit = &r["hits"]["hits"][0];
    assert_eq!(hit["_index"], "idx");
    assert!(hit["_score"].as_f64().unwrap() > 0.0);
    assert!(hit["_source"]["t"].as_str().unwrap().contains("apple"));
}

#[tokio::test]
async fn get_without_body_and_sorted_hits() {
    let app = seeded().await;
    let (s, r) = call(&app, Method::GET, "/idx/_search", None).await;
    assert_eq!(
        (s, &r["hits"]["total"]["value"]),
        (StatusCode::OK, &json!(3))
    );

    let (_, r) = call(
        &app,
        Method::POST,
        "/idx/_search",
        Some(json!({"sort": [{"n": "desc"}], "size": 2})),
    )
    .await;
    assert_eq!(r["hits"]["max_score"], json!(null));
    let hits = r["hits"]["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 2);
    assert_eq!(
        (&hits[0]["_id"], &hits[0]["_score"], &hits[0]["sort"]),
        (&json!("3"), &json!(null), &json!([3]))
    );
}

#[tokio::test]
async fn count_agrees_with_total() {
    let app = seeded().await;
    let q = json!({"query": {"range": {"n": {"gte": 2}}}});
    let (s, r) = call(&app, Method::POST, "/idx/_count", Some(q.clone())).await;
    assert_eq!((s, &r["count"]), (StatusCode::OK, &json!(2)));
    assert_eq!(r["_shards"]["total"], 1);
    let (_, r) = call(&app, Method::POST, "/idx/_search", Some(q)).await;
    assert_eq!(r["hits"]["total"]["value"], 2);
    let (_, r) = call(&app, Method::GET, "/idx/_count", None).await;
    assert_eq!(r["count"], 3);
}

#[tokio::test]
async fn errors() {
    let app = seeded().await;
    let (s, r) = call(&app, Method::POST, "/nope/_search", Some(json!({}))).await;
    assert_eq!(
        (s, &r["error"]["type"]),
        (StatusCode::NOT_FOUND, &json!("index_not_found_exception"))
    );
    let (s, r) = call(
        &app,
        Method::POST,
        "/idx/_search",
        Some(json!({"query": {"bogus": {}}})),
    )
    .await;
    assert_eq!(
        (s, &r["error"]["type"]),
        (StatusCode::BAD_REQUEST, &json!("parsing_exception"))
    );
    assert_eq!(r["error"]["reason"], "unknown query [bogus]");
    let (s, _) = call(
        &app,
        Method::POST,
        "/idx/_count",
        Some(json!({"query": {"bogus": {}}})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn esql_query() {
    let app = seeded().await;
    let body = json!({"query": "FROM idx | WHERE n >= 2 | SORT n | KEEP t, n"});
    let (s, r) = call(&app, Method::POST, "/_query", Some(body)).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        r["columns"],
        json!([{"name": "t", "type": "text"}, {"name": "n", "type": "long"}])
    );
    assert_eq!(r["values"], json!([["red apple", 2], ["pear", 3]]));
    assert_eq!(r["is_partial"], false);
    assert!(r["took"].is_u64());

    let (s, r) = call(
        &app,
        Method::POST,
        "/_query",
        Some(json!({"query": "FROM idx | BOGUS"})),
    )
    .await;
    assert_eq!(
        (s, &r["error"]["type"]),
        (StatusCode::BAD_REQUEST, &json!("parsing_exception"))
    );
    let (s, r) = call(
        &app,
        Method::POST,
        "/_query",
        Some(json!({"query": "FROM idx | KEEP zz"})),
    )
    .await;
    assert_eq!(
        (s, &r["error"]["type"]),
        (StatusCode::BAD_REQUEST, &json!("verification_exception"))
    );
    let (s, _) = call(&app, Method::POST, "/_query", Some(json!({}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}
