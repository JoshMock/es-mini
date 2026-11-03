//! Alias API responses recorded from Elasticsearch 9.5.5.

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
    (status, serde_json::from_slice(&bytes).unwrap())
}

fn error(kind: &str, reason: &str, extra: Value) -> Value {
    let mut cause = json!({"type": kind, "reason": reason});
    cause
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    let mut error = cause.clone();
    error["root_cause"] = json!([cause]);
    error
}

fn invalid(alias: &str, why: &str) -> Value {
    let reason = format!("Invalid alias name [{alias}]: {why}");
    json!({"error": error("invalid_alias_name_exception", &reason, json!({})), "status": 400})
}

const CHARS: &str =
    r#"must not contain the following characters [' ','"','*',',','/','<','>','?','\','|']"#;
const START: &str = "must not start with '_', '-', or '+'";

#[tokio::test]
async fn invalid_alias_names_are_refused() {
    let app = es_mini::app();
    call(&app, "PUT", "/i", json!({})).await;
    call(&app, "PUT", "/i2", json!({})).await;
    for (path, alias, why) in [
        ("a*b", "a*b", CHARS),
        ("a,b", "a,b", CHARS),
        ("a%20b", "a b", CHARS),
        ("a%23b", "a#b", "must not contain '#'"),
        ("a:b", "a:b", "must not contain ':'"),
        ("_x", "_x", START),
        ("-x", "-x", START),
        ("+x", "+x", START),
        (
            "i2",
            "i2",
            "an index, data stream, ESQL view, or ESQL dataset exists with the same name as the alias",
        ),
    ] {
        let out = call(&app, "PUT", &format!("/i/_alias/{path}"), json!({})).await;
        assert_eq!(out, (400, invalid(alias, why)), "{path}");
    }
    let add = |alias: &str| json!({"actions": [{"add": {"index": "i", "alias": alias}}]});
    let out = call(&app, "POST", "/_aliases", add("b*")).await;
    assert_eq!(out, (400, invalid("b*", CHARS)));
    let out = call(&app, "POST", "/_aliases", add("_b")).await;
    assert_eq!(out, (400, invalid("_b", START)));
    let out = call(&app, "PUT", "/i3", json!({"aliases": {"c*": {}}})).await;
    assert_eq!(out, (400, invalid("c*", CHARS)));

    // Upper case and date math (resolved before the check) are allowed.
    let ok = (200, json!({"acknowledged": true}));
    assert_eq!(call(&app, "PUT", "/i/_alias/Upper", json!({})).await, ok);
    let math = "/i/_alias/%3Cfoo%7Bnow%2Fd%7D%3E";
    assert_eq!(call(&app, "PUT", math, json!({})).await, ok);
    let (_, all) = call(&app, "GET", "/i/_alias", json!({})).await;
    let names: Vec<&String> = all["i"]["aliases"].as_object().unwrap().keys().collect();
    assert!(names.iter().any(|n| n.starts_with("foo2")), "{all}");
}
