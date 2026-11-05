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
    assert_eq!(
        (s, r),
        (
            StatusCode::OK,
            json!({"acknowledged": true, "errors": false})
        )
    );
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
    assert_eq!(
        (s, r),
        (
            StatusCode::OK,
            json!({"acknowledged": true, "errors": false})
        )
    );
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
    // Elasticsearch builds the filter's Lucene query, so a malformed query_string fails too.
    let bad = json!({"filter": {"query_string": {"query": "a:("}}});
    let (s, _) = call(&app, Method::PUT, "/a/_alias/z", Some(bad.clone())).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let bad = json!({"actions": [{"add": {"index": "a", "alias": "z", "filter": bad["filter"]}}]});
    let (s, _) = call(&app, Method::POST, "/_aliases", Some(bad)).await;
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
    // a refresh also counts each index's unassigned default replica
    for (m, uri, total) in [
        (Method::POST, "/a,b/_search", 2),
        (Method::POST, "/a,b/_count", 2),
        (Method::POST, "/a,b/_refresh", 4),
    ] {
        let (_, r) = call(&app, m, uri, None).await;
        assert_eq!(r["_shards"]["total"], total, "{uri}");
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

/// Recorded from Elasticsearch 9.5.5, which lists indices in hash order (h, a, b) where
/// es-mini lists them by name.
#[tokio::test]
async fn cat_aliases_lists_each_alias_of_each_index() {
    let app = app();
    let b = json!({"aliases": {"zz": {}, "aa": {"filter": {"term": {"x": 1}}, "index_routing": "1", "search_routing": "2,3", "is_write_index": true}}});
    let a = json!({"aliases": {"zz": {"is_write_index": false}, "mm": {"routing": "5"}}});
    let h = json!({"settings": {"index.hidden": true}, "aliases": {"hh": {"is_hidden": true}}});
    for (uri, body) in [("/b", b), ("/a", a), ("/h", h)] {
        assert_eq!(
            call(&app, Method::PUT, uri, Some(body)).await.0,
            StatusCode::OK
        );
    }
    let row = |alias, index, filter, ri, rs, w| json!({"alias": alias, "index": index, "filter": filter, "routing.index": ri, "routing.search": rs, "is_write_index": w});
    let mm = row("mm", "a", "-", "5", "5", "-");
    let zz_a = row("zz", "a", "-", "-", "-", "false");
    let aa = row("aa", "b", "*", "1", "2,3", "true");
    let zz_b = row("zz", "b", "-", "-", "-", "-");
    let hh = row("hh", "h", "-", "-", "-", "-");
    for (uri, rows) in [
        ("/_cat/aliases?format=json", json!([mm, zz_a, aa, zz_b, hh])),
        ("/_cat/aliases/zz?format=json", json!([zz_a, zz_b])),
        ("/_cat/aliases/z*,mm?format=json", json!([mm, zz_a, zz_b])),
        ("/_cat/aliases/nope?format=json", json!([])),
        ("/_cat/aliases/nope,zz?format=json", json!([zz_a, zz_b])),
        ("/_cat/aliases/-zz?format=json", json!([])),
    ] {
        assert_eq!(
            call(&app, Method::GET, uri, None).await,
            (StatusCode::OK, rows),
            "{uri}"
        );
    }
    let req = axum::http::Request::get("/_cat/aliases/z*,mm?v")
        .body(axum::body::Body::empty())
        .unwrap();
    let res = tower::ServiceExt::oneshot(app, req).await.unwrap();
    let text = http_body_util::BodyExt::collect(res.into_body())
        .await
        .unwrap()
        .to_bytes();
    assert_eq!(
        std::str::from_utf8(&text).unwrap(),
        "alias index filter routing.index routing.search is_write_index\n\
         mm    a     -      5             5              -\n\
         zz    a     -      -             -              false\n\
         zz    b     -      -             -              -\n"
    );
}
