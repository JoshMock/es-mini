use crate::{app, indices_tests::call, template::render};
use axum::{
    body::Body,
    http::{Method, Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

#[test]
fn search_template_render_mustache() {
    let params = json!({
        "q": "say \"hi\"", "n": 3, "on": true, "off": false, "empty": [],
        "tags": ["a", "b"], "user": {"name": "x"}, "items": [{"k": 1}, {"k": 2}]
    });
    let r = |s: &str| render(s, &params).unwrap();
    assert_eq!(
        r("{{q}}|{{{q}}}|{{&q}}|{{n}}|{{missing}}|{{! note }}"),
        "say \\\"hi\\\"|say \"hi\"|say \"hi\"|3||"
    );
    assert_eq!(
        r("{{#on}}yes{{/on}}{{#off}}no{{/off}}{{^off}}inv{{/off}}{{^empty}}e{{/empty}}"),
        "yesinve"
    );
    assert_eq!(
        r("{{#tags}}[{{.}}]{{/tags}}{{#items}}{{k}}{{n}},{{/items}}"),
        "[a][b]13,23,"
    );
    assert_eq!(
        r("{{user.name}}{{#user}}{{name}}{{/user}}{{tags.1}}"),
        "xxb"
    );
    assert_eq!(r("{{#tags}}{{#on}}{{n}}{{/on}}{{/tags}}"), "33");
    assert!(render("{{#on}}x", &params).is_err());
    assert!(render("x{{/on}}", &params).is_err());
}

#[tokio::test]
async fn search_template_index_and_global_endpoints() {
    let app = app();
    call(
        &app,
        Method::PUT,
        "/a/_doc/1",
        Some(json!({"t": "red", "n": 1})),
    )
    .await;
    call(
        &app,
        Method::PUT,
        "/a/_doc/2",
        Some(json!({"t": "blue", "n": 2})),
    )
    .await;
    let body = json!({
        "source": "{\"query\":{\"match\":{\"t\":\"{{color}}\"}}{{#small}},\"size\":{{size}}{{/small}}}",
        "params": {"color": "red", "small": true, "size": 1}
    });
    let (s, r) = call(&app, Method::POST, "/a/_search/template", Some(body)).await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(r["hits"]["total"]["value"], 1);
    assert_eq!(r["hits"]["hits"][0]["_id"], "1");

    let body = json!({"source": {"query": {"term": {"n": "{{n}}"}}}, "params": {"n": 2}});
    let (s, r) = call(&app, Method::GET, "/_search/template", Some(body)).await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(r["hits"]["hits"][0]["_id"], "2");

    let (s, r) = call(
        &app,
        Method::POST,
        "/a/_search/template",
        Some(json!({"id": "x"})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{r}");
}

#[tokio::test]
async fn search_template_msearch() {
    let app = app();
    call(&app, Method::PUT, "/a/_doc/1", Some(json!({"t": "red"}))).await;
    call(&app, Method::PUT, "/b/_doc/1", Some(json!({"t": "blue"}))).await;
    let body = concat!(
        "{}\n{\"source\":{\"query\":{\"match\":{\"t\":\"{{c}}\"}}},\"params\":{\"c\":\"red\"}}\n",
        "{\"index\":\"b\"}\n{\"source\":\"{\\\"size\\\":{{s}}}\",\"params\":{\"s\":0}}\n",
        "{\"index\":\"b\"}\n{\"source\":\"{{#x}}\"}\n",
    );
    let req = Request::builder()
        .method(Method::POST)
        .uri("/a/_msearch/template")
        .header("content-type", "application/x-ndjson")
        .body(Body::from(body))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let r: Value =
        serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let rs = r["responses"].as_array().unwrap();
    assert_eq!(rs.len(), 3);
    assert_eq!(rs[0]["hits"]["hits"][0]["_index"], "a");
    assert_eq!(rs[1]["hits"]["total"]["value"], 1);
    assert_eq!(rs[1]["hits"]["hits"], json!([]));
    assert_eq!(rs[2]["status"], 400);
}
