use crate::{app, indices_tests::call};
use axum::http::{Method, StatusCode};
use serde_json::{Value, json};

async fn seeded() -> axum::Router {
    let app = app();
    for n in 1..=5 {
        call(
            &app,
            Method::PUT,
            &format!("/idx/_doc/{n}"),
            Some(json!({"n": n})),
        )
        .await;
    }
    app
}

fn ids(r: &Value) -> Vec<&str> {
    r["hits"]["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["_id"].as_str().unwrap())
        .collect()
}

async fn open(app: &axum::Router, keep_alive: &str) -> (Value, String) {
    let (s, r) = call(
        app,
        Method::POST,
        &format!("/idx/_search?scroll={keep_alive}"),
        Some(json!({"size": 2, "sort": ["n"]})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    let id = r["_scroll_id"].as_str().unwrap().to_owned();
    (r, id)
}

#[tokio::test]
async fn scroll_pages_through_a_snapshot() {
    let app = seeded().await;
    let (r, id) = open(&app, "1m").await;
    assert_eq!(ids(&r), ["1", "2"]);
    assert_eq!(r["hits"]["total"], json!({"value": 5, "relation": "eq"}));

    // Writes after the scroll opened are not seen by it.
    call(&app, Method::DELETE, "/idx/_doc/3", None).await;
    call(&app, Method::PUT, "/idx/_doc/0", Some(json!({"n": 0}))).await;

    let body = json!({"scroll": "1m", "scroll_id": id});
    let (s, r) = call(&app, Method::POST, "/_search/scroll", Some(body.clone())).await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(ids(&r), ["3", "4"]);
    assert_eq!(r["hits"]["total"]["value"], 5);
    assert_eq!(r["_scroll_id"], json!(id));
    assert_eq!(r["_shards"]["total"], 1);

    let (_, r) = call(&app, Method::GET, &format!("/_search/scroll/{id}"), None).await;
    assert_eq!(ids(&r), ["5"]);
    let (s, r) = call(
        &app,
        Method::POST,
        &format!("/_search/scroll?scroll_id={id}"),
        None,
    )
    .await;
    assert_eq!((s, ids(&r).len()), (StatusCode::OK, 0));
}

#[tokio::test]
async fn scroll_clear_frees_contexts() {
    let app = seeded().await;
    let (_, id) = open(&app, "1m").await;
    let body = json!({"scroll_id": id});
    let (s, r) = call(&app, Method::DELETE, "/_search/scroll", Some(body.clone())).await;
    assert_eq!(
        (s, r),
        (StatusCode::OK, json!({"succeeded": true, "num_freed": 1}))
    );

    let (s, r) = call(&app, Method::POST, "/_search/scroll", Some(body.clone())).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{r}");
    assert_eq!(r["error"]["type"], "search_phase_execution_exception");
    assert_eq!(
        r["error"]["root_cause"][0]["type"],
        "search_context_missing_exception"
    );

    let (s, r) = call(&app, Method::DELETE, "/_search/scroll", Some(body)).await;
    assert_eq!(
        (s, r),
        (
            StatusCode::NOT_FOUND,
            json!({"succeeded": true, "num_freed": 0})
        )
    );

    open(&app, "1m").await;
    open(&app, "1m").await;
    let (s, r) = call(&app, Method::DELETE, "/_search/scroll/_all", None).await;
    assert_eq!((s, &r["num_freed"]), (StatusCode::OK, &json!(2)));
}

#[tokio::test]
async fn scroll_expires_after_its_keep_alive() {
    let app = seeded().await;
    let (_, id) = open(&app, "1ms").await;
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let body = json!({"scroll_id": id});
    let (s, _) = call(&app, Method::POST, "/_search/scroll", Some(body)).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn scroll_rejects_from_and_bad_ids() {
    let app = seeded().await;
    let (s, r) = call(
        &app,
        Method::POST,
        "/idx/_search?scroll=1m",
        Some(json!({"from": 1})),
    )
    .await;
    assert_eq!(
        (s, &r["error"]["reason"]),
        (
            StatusCode::BAD_REQUEST,
            &json!("Validation Failed: 1: using [from] is not allowed in a scroll context;")
        )
    );

    let body = json!({"scroll_id": "garbage"});
    let (s, r) = call(&app, Method::POST, "/_search/scroll", Some(body)).await;
    assert_eq!(
        (s, &r["error"]["reason"]),
        (StatusCode::BAD_REQUEST, &json!("Cannot parse scroll id"))
    );
}
