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

async fn open_pit(app: &axum::Router, keep_alive: &str) -> String {
    let (s, r) = call(
        app,
        Method::POST,
        &format!("/idx/_pit?keep_alive={keep_alive}"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(r["_shards"]["successful"], 1);
    r["id"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn pit_searches_a_snapshot() {
    let app = seeded().await;
    let id = open_pit(&app, "1m").await;

    call(&app, Method::DELETE, "/idx/_doc/3", None).await;
    call(&app, Method::PUT, "/idx/_doc/0", Some(json!({"n": 0}))).await;
    call(&app, Method::PUT, "/idx/_doc/1", Some(json!({"n": 9}))).await;

    let body = json!({
        "pit": {"id": id, "keep_alive": "1m"},
        "query": {"range": {"n": {"lte": 3}}},
        "sort": ["n"],
    });
    let (s, r) = call(&app, Method::POST, "/_search", Some(body)).await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(ids(&r), ["1", "2", "3"]);
    assert_eq!(r["hits"]["hits"][0]["_index"], "idx");
    assert_eq!(r["hits"]["hits"][0]["_source"], json!({"n": 1}));
    assert_eq!(r["pit_id"], json!(id));

    // The live index has moved on.
    let (_, r) = call(
        &app,
        Method::POST,
        "/idx/_search",
        Some(json!({"sort": ["n"]})),
    )
    .await;
    assert_eq!(ids(&r), ["0", "2", "4", "5", "1"]);
}

#[tokio::test]
async fn pit_close_frees_it() {
    let app = seeded().await;
    let id = open_pit(&app, "1m").await;
    let (s, r) = call(&app, Method::DELETE, "/_pit", Some(json!({"id": id}))).await;
    assert_eq!(
        (s, r),
        (StatusCode::OK, json!({"succeeded": true, "num_freed": 1}))
    );
    let body = json!({"pit": {"id": id}});
    let (s, r) = call(&app, Method::POST, "/_search", Some(body)).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{r}");
    assert_eq!(
        r["error"]["root_cause"][0]["type"],
        "search_context_missing_exception"
    );
    let (s, r) = call(&app, Method::DELETE, "/_pit", Some(json!({"id": id}))).await;
    assert_eq!(
        (s, r),
        (
            StatusCode::NOT_FOUND,
            json!({"succeeded": true, "num_freed": 0})
        )
    );
}

#[tokio::test]
async fn pit_expires_after_its_keep_alive() {
    let app = seeded().await;
    let id = open_pit(&app, "1ms").await;
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let body = json!({"pit": {"id": id}});
    let (s, _) = call(&app, Method::POST, "/_search", Some(body)).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn pit_rejects_indices_and_missing_keep_alive() {
    let app = seeded().await;
    let (s, r) = call(&app, Method::POST, "/idx/_pit", None).await;
    assert_eq!(
        (s, &r["error"]["reason"]),
        (
            StatusCode::BAD_REQUEST,
            &json!("Validation Failed: 1: [keep_alive] is not specified;")
        )
    );
    let id = open_pit(&app, "1m").await;
    let body = json!({"pit": {"id": id}});
    let (s, r) = call(&app, Method::POST, "/idx/_search", Some(body)).await;
    assert_eq!(
        (s, &r["error"]["reason"]),
        (
            StatusCode::BAD_REQUEST,
            &json!(
                "Validation Failed: 1: [indices] cannot be used with point in time. Do not specify any index with point in time.;"
            )
        )
    );
}

#[tokio::test]
async fn pit_keeps_on_disk_documents_across_saves() {
    let dir = std::env::temp_dir().join(format!("esm-pit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let app = crate::app_persisted(&dir).unwrap();
    call(&app, Method::PUT, "/idx/_doc/1", Some(json!({"n": 1}))).await;
    // Reopen so the document is served from its segment.
    let app = crate::app_persisted(&dir).unwrap();
    let id = open_pit(&app, "1m").await;
    call(&app, Method::DELETE, "/idx/_doc/1", None).await;
    let body = json!({"pit": {"id": id}});
    let (s, r) = call(&app, Method::POST, "/_search", Some(body)).await;
    std::fs::remove_dir_all(&dir).unwrap();
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(r["hits"]["hits"][0]["_source"], json!({"n": 1}));
}

/// Elasticsearch 9.5.5's errors for ids that are not base64 (URL-safe, as Java decodes them).
#[tokio::test]
async fn error_shapes_undecodable_ids_fail_like_elasticsearch() {
    let app = app();
    for (id, reason) in [
        (
            "a",
            "Input byte[] should at least have 2 bytes for base64 bytes",
        ),
        ("bogus", "Last unit does not have enough valid bits"),
        ("ab=", "Input byte array has wrong 4-byte ending unit"),
        ("abcd=", "Input byte array has wrong 4-byte ending unit"),
        ("abc=d", "Input byte array has incorrect ending byte at 4"),
        ("ab==c", "Input byte array has incorrect ending byte at 4"),
        ("a+b/", "Illegal base64 character 2b"),
        ("a.b", "Illegal base64 character 2e"),
        ("abé", "Illegal base64 character -17"),
        ("", "search context id is is not provided"),
    ] {
        let (s, r) = call(&app, Method::DELETE, "/_pit", Some(json!({"id": id}))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{id}");
        let cause = json!({"type": "illegal_argument_exception", "reason": reason});
        assert_eq!(
            r["error"],
            json!({"root_cause": [cause], "type": "illegal_argument_exception", "reason": reason}),
            "{id}"
        );
    }
    let cause =
        json!({"type": "illegal_argument_exception", "reason": "Illegal base64 character 2e"});
    let (s, r) = call(
        &app,
        Method::POST,
        "/_search",
        Some(json!({"pit": {"id": "a.b"}})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let reason = "[1:14] [pit] failed to parse field [id]";
    let root = json!({"type": "x_content_parse_exception", "reason": reason});
    assert_eq!(
        r["error"],
        json!({"root_cause": [root], "type": "x_content_parse_exception", "reason": reason, "caused_by": cause})
    );
    let body = json!({"scroll_id": "a.b"});
    let (s, r) = call(&app, Method::POST, "/_search/scroll", Some(body)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let root = json!({"type": "illegal_argument_exception", "reason": "Cannot parse scroll id"});
    assert_eq!(
        r["error"],
        json!({"root_cause": [root], "type": "illegal_argument_exception", "reason": "Cannot parse scroll id", "caused_by": cause})
    );
}

#[tokio::test]
async fn scroll_now_is_fixed_when_the_scroll_opens() {
    let app = app();
    let mappings = json!({"mappings": {"properties": {"t": {"type": "date"}}}});
    call(&app, Method::PUT, "/idx", Some(mappings)).await;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    // two documents in the past, one a second ahead
    for (id, t) in [("1", now - 2000), ("2", now - 1000), ("3", now + 1000)] {
        let path = format!("/idx/_doc/{id}?refresh=true");
        call(&app, Method::PUT, &path, Some(json!({"t": t}))).await;
    }
    let body = json!({"size": 1, "sort": ["t"], "query": {"range": {"t": {"lte": "now"}}}});
    let (s, r) = call(&app, Method::POST, "/idx/_search?scroll=1m", Some(body)).await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(
        (ids(&r), &r["hits"]["total"]["value"]),
        (vec!["1"], &json!(2))
    );
    let id = r["_scroll_id"].as_str().unwrap().to_owned();

    // once document 3 is in the past, the scroll's `now` still leaves it out
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let body = json!({"scroll": "1m", "scroll_id": id});
    let (_, r) = call(&app, Method::POST, "/_search/scroll", Some(body.clone())).await;
    assert_eq!(
        (ids(&r), &r["hits"]["total"]["value"]),
        (vec!["2"], &json!(2))
    );
    let (_, r) = call(&app, Method::POST, "/_search/scroll", Some(body)).await;
    assert_eq!(ids(&r), Vec::<&str>::new());
}
