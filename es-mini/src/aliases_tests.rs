use crate::{app, indices_tests::call};
use axum::{
    Router,
    http::{Method, StatusCode},
};
use serde_json::{Value, json};

async fn setup() -> Router {
    let app = app();
    call(
        &app,
        Method::PUT,
        "/a",
        Some(json!({"aliases": {"both": {}, "fa": {"filter": {"term": {"n": 1}}}}})),
    )
    .await;
    call(&app, Method::PUT, "/b", None).await;
    let (s, r) = call(
        &app,
        Method::PUT,
        "/b/_alias/both",
        Some(json!({"is_write_index": true})),
    )
    .await;
    assert_eq!((s, r), (StatusCode::OK, json!({"acknowledged": true})));
    call(&app, Method::PUT, "/a/_doc/1", Some(json!({"n": 1}))).await;
    call(&app, Method::PUT, "/a/_doc/2", Some(json!({"n": 2}))).await;
    call(&app, Method::PUT, "/b/_doc/3", Some(json!({"n": 3}))).await;
    app
}

fn ids(r: &Value) -> Vec<(String, String)> {
    let hits = r["hits"]["hits"].as_array().unwrap();
    hits.iter()
        .map(|h| {
            (
                h["_index"].as_str().unwrap().into(),
                h["_id"].as_str().unwrap().into(),
            )
        })
        .collect()
}

#[tokio::test]
async fn get_aliases_shapes() {
    let app = setup().await;
    let (s, r) = call(&app, Method::GET, "/_alias", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        r,
        json!({
            "a": {"aliases": {"both": {}, "fa": {"filter": {"term": {"n": 1}}}}},
            "b": {"aliases": {"both": {"is_write_index": true}}}
        })
    );
    let (_, r) = call(&app, Method::GET, "/_alias/fa", None).await;
    assert_eq!(
        r,
        json!({"a": {"aliases": {"fa": {"filter": {"term": {"n": 1}}}}}})
    );
    let (_, r) = call(&app, Method::GET, "/b/_alias", None).await;
    assert_eq!(
        r,
        json!({"b": {"aliases": {"both": {"is_write_index": true}}}})
    );
    let (_, r) = call(&app, Method::GET, "/a", None).await;
    assert_eq!(r["a"]["aliases"]["both"], json!({}));

    let (s, r) = call(&app, Method::GET, "/_alias/fa,nope", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(
        (&r["error"], &r["status"]),
        (&json!("alias [nope] missing"), &json!(404))
    );
    assert!(r["a"]["aliases"]["fa"].is_object());

    assert_eq!(
        call(&app, Method::HEAD, "/_alias/both", None).await.0,
        StatusCode::OK
    );
    assert_eq!(
        call(&app, Method::HEAD, "/b/_alias/fa", None).await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn reads_span_alias_indices_and_apply_filters() {
    let app = setup().await;
    let (_, r) = call(
        &app,
        Method::POST,
        "/both/_search",
        Some(json!({"sort": ["n"]})),
    )
    .await;
    assert_eq!(
        ids(&r),
        [
            ("a".into(), "1".into()),
            ("a".into(), "2".into()),
            ("b".into(), "3".into())
        ]
    );
    let (_, r) = call(&app, Method::POST, "/fa/_search", None).await;
    assert_eq!(ids(&r), [("a".into(), "1".into())]);
    let (_, r) = call(&app, Method::POST, "/a,b/_count", None).await;
    assert_eq!(r["count"], 3);
    let (_, r) = call(&app, Method::POST, "/fa/_count", None).await;
    assert_eq!(r["count"], 1);
    let (_, r) = call(
        &app,
        Method::POST,
        "/_query",
        Some(json!({"query": "FROM fa | KEEP n"})),
    )
    .await;
    assert_eq!(r["values"], json!([[1]]));

    let (s, r) = call(&app, Method::GET, "/fa/_doc/1", None).await;
    assert_eq!((s, &r["_index"]), (StatusCode::OK, &json!("a")));
    let (s, r) = call(&app, Method::GET, "/both/_doc/1", None).await;
    assert_eq!(
        (s, &r["error"]["type"]),
        (
            StatusCode::BAD_REQUEST,
            &json!("illegal_argument_exception")
        )
    );
}

#[tokio::test]
async fn writes_go_to_the_write_index() {
    let app = setup().await;
    let (s, r) = call(&app, Method::PUT, "/both/_doc/9", Some(json!({"n": 9}))).await;
    assert_eq!((s, &r["_index"]), (StatusCode::CREATED, &json!("b")));
    let (_, r) = call(
        &app,
        Method::POST,
        "/both/_update/9",
        Some(json!({"doc": {"m": 1}})),
    )
    .await;
    assert_eq!(r["_index"], "b");
    let (s, r) = call(&app, Method::PUT, "/fa/_doc/9", Some(json!({}))).await;
    assert_eq!((s, &r["_index"]), (StatusCode::CREATED, &json!("a")));
    call(&app, Method::PUT, "/b/_alias/fa", None).await;
    let (s, r) = call(&app, Method::PUT, "/fa/_doc/9", Some(json!({}))).await;
    assert_eq!(
        (s, &r["error"]["type"]),
        (
            StatusCode::BAD_REQUEST,
            &json!("illegal_argument_exception")
        )
    );
    let (s, r) = call(&app, Method::DELETE, "/both/_doc/9", None).await;
    assert_eq!((s, &r["result"]), (StatusCode::OK, &json!("deleted")));
}

#[tokio::test]
async fn delete_alias() {
    let app = setup().await;
    let (s, r) = call(&app, Method::DELETE, "/a/_alias/fa", None).await;
    assert_eq!((s, r), (StatusCode::OK, json!({"acknowledged": true})));
    let (s, r) = call(&app, Method::DELETE, "/a/_alias/fa", None).await;
    assert_eq!(
        (s, &r["error"]["type"]),
        (StatusCode::NOT_FOUND, &json!("aliases_not_found_exception"))
    );
    assert_eq!(r["error"]["reason"], "aliases [fa] missing");
    let (s, _) = call(&app, Method::DELETE, "/_all/_alias/*", None).await;
    assert_eq!(s, StatusCode::OK);
    let (_, r) = call(&app, Method::GET, "/_alias", None).await;
    assert_eq!(r, json!({"a": {"aliases": {}}, "b": {"aliases": {}}}));
}

#[tokio::test]
async fn update_aliases_actions_are_atomic() {
    let app = setup().await;
    let body = json!({"actions": [
        {"add": {"indices": ["a", "b"], "alias": "x"}},
        {"remove": {"index": "a", "aliases": ["both"]}},
    ]});
    let (s, r) = call(&app, Method::POST, "/_aliases", Some(body)).await;
    assert_eq!(
        (s, r),
        (
            StatusCode::OK,
            json!({"acknowledged": true, "errors": false})
        )
    );
    let (_, r) = call(&app, Method::GET, "/_alias/x,both", None).await;
    assert_eq!(
        r,
        json!({"a": {"aliases": {"x": {}}}, "b": {"aliases": {"both": {"is_write_index": true}, "x": {}}}})
    );

    // A failing action applies nothing.
    let body = json!({"actions": [{"add": {"index": "a", "alias": "y"}}, {"remove": {"index": "a", "alias": "nope", "must_exist": true}}]});
    let (s, r) = call(&app, Method::POST, "/_aliases", Some(body)).await;
    assert_eq!(
        (s, &r["error"]["type"]),
        (StatusCode::NOT_FOUND, &json!("aliases_not_found_exception"))
    );
    assert_eq!(
        call(&app, Method::HEAD, "/_alias/y", None).await.0,
        StatusCode::NOT_FOUND
    );

    let body = json!({"actions": [{"remove_index": {"index": "b"}}]});
    call(&app, Method::POST, "/_aliases", Some(body)).await;
    assert_eq!(
        call(&app, Method::HEAD, "/b", None).await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn name_conflicts_and_bad_filters() {
    let app = setup().await;
    let (s, r) = call(&app, Method::PUT, "/a/_alias/b", None).await;
    assert_eq!(
        (s, &r["error"]["type"]),
        (
            StatusCode::BAD_REQUEST,
            &json!("invalid_alias_name_exception")
        )
    );
    let (s, r) = call(&app, Method::PUT, "/both", None).await;
    assert_eq!(
        (s, &r["error"]["type"]),
        (
            StatusCode::BAD_REQUEST,
            &json!("invalid_index_name_exception")
        )
    );
    let (s, _) = call(
        &app,
        Method::PUT,
        "/a/_alias/z",
        Some(json!({"filter": {"bogus": {}}})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, r) = call(&app, Method::PUT, "/nope/_alias/z", None).await;
    assert_eq!(
        (s, &r["error"]["type"]),
        (StatusCode::NOT_FOUND, &json!("index_not_found_exception"))
    );
}

#[tokio::test]
async fn create_resolves_date_math_alias_names() {
    let app = app();
    let (s, _) = call(
        &app,
        Method::PUT,
        "/a",
        Some(json!({"aliases": {"<logs_{2022-12-31||/d{yyyy-MM-dd}}>": {}}})),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = call(&app, Method::HEAD, "/_alias/logs_2022-12-31", None).await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test]
async fn mapping_and_settings_resolve_expressions_like_get_index() {
    let app = setup().await;
    for path in ["_mapping", "_settings"] {
        let (s, r) = call(&app, Method::GET, &format!("/fa/{path}"), None).await;
        assert_eq!(s, StatusCode::OK, "{path}");
        assert_eq!(
            r.as_object().unwrap().keys().collect::<Vec<_>>(),
            ["a"],
            "{path}"
        );
        let (_, r) = call(&app, Method::GET, &format!("/a,b/{path}"), None).await;
        assert_eq!(r.as_object().unwrap().len(), 2, "{path}");
        let (s, _) = call(&app, Method::GET, &format!("/a,nope/{path}"), None).await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{path}");
        let (s, r) = call(
            &app,
            Method::GET,
            &format!("/a,nope/{path}?ignore_unavailable=true"),
            None,
        )
        .await;
        assert_eq!(
            (s, r.as_object().unwrap().len()),
            (StatusCode::OK, 1),
            "{path}"
        );
    }
}

#[tokio::test]
async fn shards_total_counts_one_per_target_index() {
    let app = setup().await;
    for (m, uri) in [
        (Method::POST, "/a,b/_search"),
        (Method::POST, "/a,b/_count"),
        (Method::POST, "/a,b/_refresh"),
    ] {
        let (_, r) = call(&app, m, uri, None).await;
        assert_eq!(r["_shards"]["total"], 2, "{uri}");
        assert_eq!(r["_shards"]["successful"], 2, "{uri}");
    }
}

#[test]
fn date_math_names_resolve_now_units_formats_and_zones() {
    let now = 1_700_000_000_000; // 2023-11-14T22:13:20Z
    let r = |n: &str| crate::aliases::resolve_date_math_at(n, now).ok();
    assert_eq!(r("plain"), Some("plain".into()));
    assert_eq!(r("<logs-{now/d}>"), Some("logs-2023.11.14".into()));
    assert_eq!(r("<logs-{now/M{yyyy.MM}}>"), Some("logs-2023.11".into()));
    assert_eq!(r("<l-{now-1d/d{yyyy-MM-dd}}>"), Some("l-2023-11-13".into()));
    assert_eq!(
        r("<l-{now/h{yyyy.MM.dd.HH}}>"),
        Some("l-2023.11.14.22".into())
    );
    assert_eq!(r("<l-{now/y{yyyy}}>"), Some("l-2023".into()));
    assert_eq!(
        r("<l-{now/d{yyyy.MM.dd|+03:00}}>"),
        Some("l-2023.11.15".into())
    );
    assert_eq!(r(r"<a\{b-{now/d{yyyy}}>"), Some("a{b-2023".into()));
    assert!(r("<l-{now/q}>").is_none());
    assert!(r("<l-{now/d>").is_none());
}
