//! Errors Elasticsearch reports for malformed parts of a request. Every expected value here is
//! what Elasticsearch 9.5.5 answered to the same requests, but for the `[line:col]` prefix of
//! x_content_parse_exception reasons, which es-mini does not give.

use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

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

/// Index `t` with one document.
async fn setup() -> axum::Router {
    let app = es_mini::app();
    let mappings =
        json!({"mappings": {"properties": {"n": {"type": "long"}, "k": {"type": "keyword"}}}});
    call(&app, "PUT", "/t", mappings).await;
    call(
        &app,
        "PUT",
        "/t/_doc/1?refresh=true",
        json!({"n": 1, "k": "a"}),
    )
    .await;
    app
}

#[tokio::test]
async fn error_positions_malformed_order_path_elements() {
    let app = setup().await;
    let max = json!({"a": {"max": {"field": "n"}}});
    let filter = json!({"f": {"filter": {"match_all": {}}, "aggs": max}});
    for (path, subs, element) in [
        ("a[", &max, "a["),
        ("[x]", &max, "[x]"),
        ("a[]", &max, "a[]"),
        ("a[x]y", &max, "a[x]y"),
        ("a[x", &max, "a[x"),
        ("a.", &max, "a."),
        (".a", &max, ".a"),
        ("f>[x]", &filter, "[x]"),
        ("f[x>a", &filter, "f[x"),
    ] {
        let body = json!({"size": 0, "aggs": {"x": {"terms": {"field": "k", "order": {path: "asc"}}, "aggs": subs}}});
        let outer = json!({"type": "x_content_parse_exception", "reason": "[terms] failed to parse field [order]"});
        let mut error = outer.clone();
        error["root_cause"] = json!([outer]);
        error["caused_by"] = json!({"type": "illegal_argument_exception",
            "reason": format!("Invalid path element [{element}] in path [{path}]")});
        let expected = json!({"error": error, "status": 400});
        assert_eq!(
            call(&app, "POST", "/t/_search", body).await,
            (400, expected),
            "{path}"
        );
    }
}

/// The `(type, reason)` chain of an error, outermost first.
fn chain(mut e: &Value) -> Vec<(String, String)> {
    let mut out = vec![];
    while e.is_object() {
        out.push((
            e["type"].as_str().unwrap().into(),
            e["reason"].as_str().unwrap().into(),
        ));
        e = &e["caused_by"];
    }
    out
}

/// The innermost (Lucene) reasons are not compared: ES's are JavaCC's, listing the tokens it
/// expected, which es-mini's parser does not word alike.
#[tokio::test]
async fn error_positions_query_string_syntax_fails_each_shard() {
    let app = setup().await;
    call(&app, "PUT", "/u", json!({})).await;
    let uuid = |i: &str| {
        let app = app.clone();
        let i = i.to_owned();
        async move {
            let (_, s) = call(&app, "GET", &format!("/{i}/_settings"), Value::Null).await;
            s[&i]["settings"]["index"]["uuid"].clone()
        }
    };
    let (t, u) = (uuid("t").await, uuid("u").await);
    let qs = |q: &str| json!({"query": {"query_string": {"query": q}}});
    for (uri, body, q, indices, inner) in [
        (
            "/t/_search",
            qs("k:"),
            "k:",
            vec![("t", &t)],
            "parse_exception",
        ),
        (
            "/t/_search",
            qs("\"foo"),
            "\"foo",
            vec![("t", &t)],
            "token_mgr_error",
        ),
        (
            "/t/_search?q=k:(",
            Value::Null,
            "k:(",
            vec![("t", &t)],
            "parse_exception",
        ),
        (
            "/t,u/_search",
            qs("a AND"),
            "a AND",
            vec![("t", &t), ("u", &u)],
            "parse_exception",
        ),
        (
            "/t/_count",
            qs("k:"),
            "k:",
            vec![("t", &t)],
            "parse_exception",
        ),
    ] {
        let (status, got) = call(&app, "POST", uri, body).await;
        assert_eq!(status, 400, "{uri} {got}");
        let e = &got["error"];
        assert_eq!(
            (&e["type"], &e["reason"]),
            (
                &json!("search_phase_execution_exception"),
                &json!("all shards failed")
            ),
            "{uri} {got}"
        );
        let reason = format!("Failed to parse query [{q}]");
        let roots: Vec<_> = (indices.iter())
            .map(|(i, id)| json!({"type": "query_shard_exception", "reason": reason, "index_uuid": id, "index": i}))
            .collect();
        assert_eq!(e["root_cause"], json!(roots), "{uri}");
        let failed = e["failed_shards"].as_array().unwrap();
        assert_eq!(failed.len(), indices.len(), "{uri}");
        for (f, (i, id)) in failed.iter().zip(&indices) {
            assert_eq!((&f["index"], &f["reason"]["index_uuid"]), (&json!(i), *id));
            let c = chain(&f["reason"]);
            let kinds: Vec<_> = c.iter().map(|(k, _)| k.as_str()).collect();
            assert_eq!(
                kinds,
                ["query_shard_exception", "parse_exception", inner],
                "{uri}"
            );
            assert_eq!(c[0].1, reason);
            assert_eq!(c[1].1, format!("Cannot parse '{q}': {}", c[2].1));
        }
    }
    // without a shard to fail on, the search succeeds
    let (status, _) = call(&app, "POST", "/none*/_search", qs("k:")).await;
    assert_eq!(status, 200);
}
