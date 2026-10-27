use crate::{app, indices_tests::call};
use axum::http::{Method, StatusCode};
use serde_json::json;

#[tokio::test]
async fn index_get_delete_with_auto_created_index() {
    let app = app();
    let (s, r) = call(&app, Method::PUT, "/idx/_doc/1", Some(json!({"a": 1}))).await;
    assert_eq!(s, StatusCode::CREATED);
    assert_eq!(
        r,
        json!({
            "_index": "idx", "_id": "1", "_version": 1, "result": "created",
            "_shards": {"total": 2, "successful": 1, "failed": 0},
            "_seq_no": 0, "_primary_term": 1
        })
    );

    let (s, r) = call(&app, Method::POST, "/idx/_doc/1", Some(json!({"a": 2}))).await;
    assert_eq!(
        (s, &r["result"], &r["_version"]),
        (StatusCode::OK, &json!("updated"), &json!(2))
    );

    let (s, r) = call(&app, Method::GET, "/idx/_doc/1", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        r,
        json!({
            "_index": "idx", "_id": "1", "_version": 2, "_seq_no": 1, "_primary_term": 1,
            "found": true, "_source": {"a": 2}
        })
    );

    let (s, r) = call(&app, Method::DELETE, "/idx/_doc/1", None).await;
    assert_eq!(
        (s, &r["result"], &r["_version"]),
        (StatusCode::OK, &json!("deleted"), &json!(3))
    );

    let (s, r) = call(&app, Method::GET, "/idx/_doc/1", None).await;
    assert_eq!(
        (s, r),
        (
            StatusCode::NOT_FOUND,
            json!({"_index": "idx", "_id": "1", "found": false})
        )
    );

    let (s, r) = call(&app, Method::DELETE, "/idx/_doc/1", None).await;
    assert_eq!(
        (s, &r["result"]),
        (StatusCode::NOT_FOUND, &json!("not_found"))
    );
}

#[tokio::test]
async fn auto_id() {
    let app = app();
    let (s, r) = call(&app, Method::POST, "/idx/_doc", Some(json!({"a": 1}))).await;
    assert_eq!((s, &r["result"]), (StatusCode::CREATED, &json!("created")));
    let id = r["_id"].as_str().unwrap();
    assert_eq!(id.len(), 20);
    let (_, r2) = call(&app, Method::POST, "/idx/_doc", Some(json!({"a": 1}))).await;
    assert_ne!(r2["_id"], r["_id"]);
    // The sequence counting up is checked in es-index/tests/auto_id.rs, alone in its process.
    let (s, _) = call(&app, Method::GET, &format!("/idx/_doc/{id}"), None).await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test]
async fn create_vs_index() {
    let app = app();
    let (s, r) = call(&app, Method::PUT, "/idx/_create/1", Some(json!({}))).await;
    assert_eq!((s, &r["result"]), (StatusCode::CREATED, &json!("created")));
    for uri in ["/idx/_create/1", "/idx/_doc/1?op_type=create"] {
        let (s, r) = call(&app, Method::PUT, uri, Some(json!({}))).await;
        assert_eq!(s, StatusCode::CONFLICT, "{uri}");
        assert_eq!(r["error"]["type"], "version_conflict_engine_exception");
        assert_eq!(
            r["error"]["reason"],
            "[1]: version conflict, document already exists (current version [1])"
        );
    }
}

#[tokio::test]
async fn get_from_missing_index_is_index_not_found() {
    let (s, r) = call(&app(), Method::GET, "/nope/_doc/1", None).await;
    assert_eq!(
        (s, &r["error"]["type"]),
        (StatusCode::NOT_FOUND, &json!("index_not_found_exception"))
    );
}

#[tokio::test]
async fn update() {
    let app = app();
    call(&app, Method::PUT, "/idx/_doc/1", Some(json!({"a": 1}))).await;
    let (s, r) = call(
        &app,
        Method::POST,
        "/idx/_update/1",
        Some(json!({"doc": {"b": 2}})),
    )
    .await;
    assert_eq!(
        (s, &r["result"], &r["_version"]),
        (StatusCode::OK, &json!("updated"), &json!(2))
    );
    let (_, r) = call(&app, Method::GET, "/idx/_doc/1", None).await;
    assert_eq!(r["_source"], json!({"a": 1, "b": 2}));

    let (s, r) = call(
        &app,
        Method::POST,
        "/idx/_update/1",
        Some(json!({"doc": {"b": 2}})),
    )
    .await;
    assert_eq!((s, &r["result"]), (StatusCode::OK, &json!("noop")));

    let (s, r) = call(
        &app,
        Method::POST,
        "/idx/_update/2",
        Some(json!({"doc": {"b": 2}})),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(r["error"]["type"], "document_missing_exception");
    assert_eq!(r["error"]["reason"], "[2]: document missing");

    let body = json!({"doc": {"b": 2}, "doc_as_upsert": true});
    let (s, r) = call(&app, Method::POST, "/idx/_update/3", Some(body)).await;
    assert_eq!((s, &r["result"]), (StatusCode::CREATED, &json!("created")));

    let body = json!({"doc": {"b": 2}, "upsert": {"u": 1}});
    call(&app, Method::POST, "/idx/_update/4", Some(body)).await;
    let (_, r) = call(&app, Method::GET, "/idx/_doc/4", None).await;
    assert_eq!(r["_source"], json!({"u": 1}));
}

#[tokio::test]
async fn update_unknown_field_suggests_doc() {
    let (s, r) = call(
        &app(),
        Method::POST,
        "/idx/_update/1",
        Some(json!({"dac": {}})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(
        r["error"]["reason"],
        "[UpdateRequest] unknown field [dac] did you mean [doc]?"
    );
}

#[tokio::test]
async fn non_object_source_is_400() {
    let (s, r) = call(&app(), Method::PUT, "/idx/_doc/1", Some(json!([1]))).await;
    assert_eq!(
        (s, &r["error"]["type"]),
        (StatusCode::BAD_REQUEST, &json!("mapper_parsing_exception"))
    );
}

#[tokio::test]
async fn date_not_in_the_field_format_is_rejected() {
    let app = app();
    let mappings =
        json!({"mappings": {"properties": {"d": {"type": "date", "format": "epoch_second"}}}});
    call(&app, Method::PUT, "/idx", Some(mappings)).await;
    let doc = json!({"d": "2020-03-08T06:15:00Z"});
    let (s, r) = call(&app, Method::PUT, "/idx/_doc/1", Some(doc)).await;
    assert_eq!(
        (s, &r["error"]["type"]),
        (
            StatusCode::BAD_REQUEST,
            &json!("document_parsing_exception")
        )
    );
    for d in [json!(1000), json!("2000")] {
        let (s, _) = call(&app, Method::PUT, "/idx/_doc/2", Some(json!({"d": d}))).await;
        assert!(s.is_success());
    }
}

#[tokio::test]
async fn rejected_document_adds_no_dynamic_mappings() {
    let app = app();
    let mappings =
        json!({"mappings": {"properties": {"d": {"type": "date", "format": "epoch_second"}}}});
    call(&app, Method::PUT, "/idx", Some(mappings)).await;
    let doc = json!({"d": "not a date", "new": 1});
    let (s, _) = call(&app, Method::PUT, "/idx/_doc/1", Some(doc)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (_, r) = call(&app, Method::GET, "/idx/_mapping", None).await;
    assert_eq!(r["idx"]["mappings"]["properties"]["new"], json!(null));
}

#[tokio::test]
async fn object_fields_map_dynamically_and_are_searchable() {
    let app = app();
    let doc = json!({"o": {"n": 5, "t": "Hi there"}});
    call(&app, Method::PUT, "/idx/_doc/1?refresh=true", Some(doc)).await;
    let (_, r) = call(&app, Method::GET, "/idx/_mapping", None).await;
    assert_eq!(
        r["idx"]["mappings"]["properties"]["o"]["properties"]["n"],
        json!({"type": "long"})
    );
    for q in [
        json!({"term": {"o.n": 5}}),
        json!({"term": {"o.t.keyword": "Hi there"}}),
    ] {
        let (_, r) = call(&app, Method::POST, "/idx/_count", Some(json!({"query": q}))).await;
        assert_eq!(r["count"], 1, "{q}");
    }
}

#[tokio::test]
async fn id_over_512_bytes_is_rejected() {
    let uri = format!("/idx/_doc/{}", "a".repeat(513));
    let (s, _) = call(&app(), Method::POST, &uri, Some(json!({"a": 1}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn update_require_alias_rejects_non_alias() {
    let app = app();
    let (s, r) = call(
        &app,
        Method::POST,
        "/idx/_update/1?require_alias=true",
        Some(json!({"doc": {"a": 1}, "doc_as_upsert": true})),
    )
    .await;
    assert_eq!(
        (s, &r["error"]["type"]),
        (StatusCode::NOT_FOUND, &json!("index_not_found_exception"))
    );
}

#[tokio::test]
async fn non_realtime_get_sees_refreshed_docs_only() {
    let app = app();
    call(
        &app,
        Method::PUT,
        "/idx",
        Some(json!({"settings": {"refresh_interval": "-1"}})),
    )
    .await;
    call(&app, Method::PUT, "/idx/_doc/1", Some(json!({}))).await;
    call(&app, Method::POST, "/idx/_refresh", None).await;
    call(&app, Method::PUT, "/idx/_doc/2", Some(json!({}))).await;
    let found = |r: serde_json::Value| r["found"].clone();
    assert_eq!(
        found(
            call(&app, Method::GET, "/idx/_doc/1?realtime=false", None)
                .await
                .1
        ),
        json!(true)
    );
    assert_eq!(
        found(
            call(&app, Method::GET, "/idx/_doc/2?realtime=false", None)
                .await
                .1
        ),
        json!(false)
    );
    assert_eq!(
        found(call(&app, Method::GET, "/idx/_doc/2", None).await.1),
        json!(true)
    );
}

#[tokio::test]
async fn refresh_wait_for_returns_once_the_write_is_searchable() {
    let app = app();
    let settings = json!({"settings": {"refresh_interval": "1h"}});
    call(&app, Method::PUT, "/idx", Some(settings)).await;
    let (_, r) = call(
        &app,
        Method::PUT,
        "/idx/_doc/1?refresh=wait_for",
        Some(json!({})),
    )
    .await;
    assert_eq!(r.get("forced_refresh"), None);
    let (_, r) = call(&app, Method::GET, "/idx/_doc/1?realtime=false", None).await;
    assert_eq!(r["found"], json!(true));
}

#[tokio::test]
async fn every_write_endpoint_honors_refresh_and_require_alias() {
    let app = app();
    let doc = || Some(json!({"a": 1}));
    for (m, uri) in [
        (Method::POST, "/idx/_doc?refresh=true"),
        (Method::PUT, "/idx/_create/c?refresh=true"),
        (Method::DELETE, "/idx/_doc/c?refresh=true"),
    ] {
        let (_, r) = call(&app, m, uri, doc()).await;
        assert_eq!(r["forced_refresh"], json!(true), "{uri}");
    }
    for (m, uri) in [
        (Method::POST, "/new/_doc?require_alias=true"),
        (Method::PUT, "/new/_create/c?require_alias=true"),
        (Method::DELETE, "/new/_doc/c?require_alias=true"),
    ] {
        let (s, r) = call(&app, m, uri, doc()).await;
        assert_eq!(
            (s, &r["error"]["type"]),
            (StatusCode::NOT_FOUND, &json!("index_not_found_exception")),
            "{uri}"
        );
    }
}

#[tokio::test]
async fn search_window_and_negative_from_are_rejected() {
    let app = app();
    call(
        &app,
        Method::PUT,
        "/idx",
        Some(json!({"settings": {"max_result_window": 5}})),
    )
    .await;
    let (s, r) = call(
        &app,
        Method::POST,
        "/idx/_search",
        Some(json!({"from": 3, "size": 3})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(r["error"]["reason"].as_str().unwrap().starts_with(
        "Result window is too large, from + size must be less than or equal to: [5] but was [6]."
    ));
    let (s, r) = call(
        &app,
        Method::POST,
        "/idx/_search",
        Some(json!({"from": -1})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(
        r["error"]["reason"],
        "[from] parameter cannot be negative, found [-1]"
    );
    let (s, _) = call(
        &app,
        Method::POST,
        "/idx/_search",
        Some(json!({"from": 2, "size": 3})),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test]
async fn get_source_endpoint_and_filtering() {
    let app = app();
    call(
        &app,
        Method::PUT,
        "/idx/_doc/1",
        Some(json!({"a": 1, "b": {"c": 2, "d": 3}})),
    )
    .await;
    let (s, r) = call(&app, Method::GET, "/idx/_source/1", None).await;
    assert_eq!(
        (s, r),
        (StatusCode::OK, json!({"a": 1, "b": {"c": 2, "d": 3}}))
    );
    let (_, r) = call(
        &app,
        Method::GET,
        "/idx/_source/1?_source_excludes=b.d",
        None,
    )
    .await;
    assert_eq!(r, json!({"a": 1, "b": {"c": 2}}));
    let (_, r) = call(&app, Method::GET, "/idx/_doc/1?_source=a", None).await;
    assert_eq!(r["_source"], json!({"a": 1}));
    let (_, r) = call(&app, Method::GET, "/idx/_doc/1?_source=false", None).await;
    assert!(r.get("_source").is_none());
    let (s, _) = call(&app, Method::GET, "/idx/_source/9", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn get_source_errors_match_elasticsearch() {
    let app = app();
    call(&app, Method::PUT, "/idx/_doc/1", Some(json!({"a": 1}))).await;
    let (s, r) = call(&app, Method::GET, "/idx/_source/9", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(r["error"]["type"], "resource_not_found_exception");
    assert_eq!(r["error"]["reason"], "Document not found [idx]/[9]");
    let (s, r) = call(&app, Method::GET, "/idx/_source/1?_source=false", None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(
        r["error"]["reason"],
        "Validation Failed: 1: fetching source can not be disabled;"
    );
}

#[tokio::test]
async fn refresh_without_an_index_refreshes_every_index() {
    let app = app();
    for idx in ["a", "b"] {
        let settings = json!({"settings": {"refresh_interval": "-1"}});
        call(&app, Method::PUT, &format!("/{idx}"), Some(settings)).await;
        call(
            &app,
            Method::PUT,
            &format!("/{idx}/_doc/1"),
            Some(json!({})),
        )
        .await;
    }
    for method in [Method::POST, Method::GET] {
        let (s, r) = call(&app, method, "/_refresh", None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(r["_shards"]["successful"], 2);
    }
    let (_, r) = call(&app, Method::GET, "/b/_doc/1?realtime=false", None).await;
    assert_eq!(r["found"], true);
}

#[test]
fn writes_to_different_indices_run_concurrently() {
    let state = std::sync::Arc::new(crate::store::Store::new(es_index::Indices::from(
        ["a", "b"].map(|n| (n.to_owned(), es_index::Index::default())),
    )));
    let shared = state.read();
    let writing_a = shared.write("a").unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let to_b = std::thread::spawn({
        let state = state.clone();
        move || {
            let wrote = crate::docs::put_or_create(&state, "b", "1", json!({}), false, 0);
            tx.send(wrote.is_ok()).unwrap();
        }
    });
    let wrote_b = rx.recv_timeout(std::time::Duration::from_secs(10));
    drop(writing_a);
    drop(shared);
    to_b.join().unwrap();
    assert_eq!(wrote_b, Ok(true));
}
