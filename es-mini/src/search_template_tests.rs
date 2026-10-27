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

/// Outputs recorded from Elasticsearch 9.5.5's `_render/template`: a top-level list renders as
/// its index map, maps in Java `HashMap` order, doubles as `Double.toString`.
#[test]
fn error_shapes_template_renders_lists_and_maps_like_java() {
    let params = json!({
        "a": [1, "x", {"k": "v"}, null, true, 1.5],
        "o": {"k": [1, 2], "z": "q\"w"},
        "n": (0..13).collect::<Vec<_>>(),
        "big": {"a": 1, "b": 2, "c": 3, "d": 4, "e": 5, "f": 6, "g": 7, "h": 8, "i": 9, "j": 10,
            "k": 11, "l": 12, "m": 13, "aa": 14, "bb": 15, "é": 16, "€x": 17},
        "nest": [[1, [2]], {"q": [3]}], "e": [], "m": {"Aa": 1, "BB": 2},
        "x": -0.0, "y": 1e-5, "z": 123456789.0, "g": 1e20, "f": 1.0
    });
    let r = |s: &str| render(s, &params).unwrap();
    assert_eq!(r("{{a}}"), "{0=1, 1=x, 2={k=v}, 3=null, 4=true, 5=1.5}");
    assert_eq!(
        r("{{o}}|{{{o}}}"),
        "{z=q\\\"w, k=[1, 2]}|{z=q\"w, k=[1, 2]}"
    );
    assert_eq!(
        r("{{n}}"),
        "{0=0, 1=1, 2=2, 3=3, 4=4, 5=5, 6=6, 7=7, 8=8, 9=9, 10=10, 11=11, 12=12}"
    );
    assert_eq!(
        r("{{big}}"),
        "{aa=14, bb=15, a=1, b=2, c=3, d=4, e=5, f=6, g=7, h=8, i=9, é=16, j=10, k=11, l=12, m=13, €x=17}"
    );
    assert_eq!(
        r("{{nest}} {{e}} {{m}}"),
        "{0=[1, [2]], 1={q=[3]}} {} {Aa=1, BB=2}"
    );
    assert_eq!(
        r("{{x}} {{y}} {{z}} {{g}} {{f}}"),
        "-0.0 1.0E-5 1.23456789E8 1.0E20 1.0"
    );
}
