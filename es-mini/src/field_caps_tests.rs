use crate::{app, indices_tests::call};
use axum::http::{Method, StatusCode};
use serde_json::{Value, json};

// Expected bodies below are Elasticsearch 9.5.5's responses to the same requests.

fn cap(t: &str, searchable: bool, aggregatable: bool) -> Value {
    json!({"type": t, "metadata_field": t.starts_with('_'), "searchable": searchable,
        "aggregatable": aggregatable, "inference": false})
}

async fn seeded() -> axum::Router {
    let app = app();
    let a = json!({"mappings": {"properties": {"k": {"type": "keyword"}, "n": {"type": "short"},
        "d": {"type": "date"}, "b": {"type": "boolean"}, "f": {"type": "float"},
        "t": {"type": "text"}}}});
    call(&app, Method::PUT, "/a", Some(a)).await;
    let b = json!({"mappings": {"properties": {"k": {"type": "text"}, "n": {"type": "short"}}}});
    call(&app, Method::PUT, "/b", Some(b)).await;
    let c = json!({"s": "hello", "x": 1.5, "i": 3});
    call(&app, Method::PUT, "/c/_doc/1", Some(c)).await;
    app
}

#[tokio::test]
async fn field_caps_mapped_types() {
    let app = seeded().await;
    let (s, r) = call(
        &app,
        Method::GET,
        "/a/_field_caps?fields=*&filters=-metadata",
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        r,
        json!({"indices": ["a"], "fields": {
            "b": {"boolean": cap("boolean", true, true)},
            "d": {"date": cap("date", true, true)},
            "f": {"float": cap("float", true, true)},
            "k": {"keyword": cap("keyword", true, true)},
            "n": {"short": cap("short", true, true)},
            "t": {"text": cap("text", true, false)},
        }})
    );
}

#[tokio::test]
async fn field_caps_wildcards_multifields_and_metadata() {
    let app = seeded().await;
    let (_, r) = call(&app, Method::GET, "/_field_caps?fields=s*,i", None).await;
    assert_eq!(
        r,
        json!({"indices": ["a", "b", "c"], "fields": {
            "s": {"text": cap("text", true, false)},
            "s.keyword": {"keyword": cap("keyword", true, true)},
            "i": {"long": cap("long", true, true)},
        }})
    );
    let (_, r) = call(&app, Method::GET, "/c/_field_caps?fields=_i*,_v*", None).await;
    assert_eq!(
        r,
        json!({"indices": ["c"], "fields": {
            "_id": {"_id": cap("_id", true, false)},
            "_ignored": {"_ignored": cap("_ignored", true, true)},
            "_ignored_source": {"_ignored_source": cap("_ignored_source", false, false)},
            "_index": {"_index": cap("_index", true, true)},
            "_index_mode": {"_index_mode": cap("_index_mode", true, true)},
            "_inference_fields": {"_inference_fields": cap("_inference_fields", false, false)},
            "_version": {"_version": cap("_version", false, true)},
        }})
    );
    let (_, r) = call(&app, Method::GET, "/c/_field_caps?fields=*", None).await;
    assert_eq!(r["fields"].as_object().unwrap().len(), 16 + 4);
    let mut tier = cap("keyword", true, true);
    tier["metadata_field"] = json!(true);
    assert_eq!(r["fields"]["_tier"], json!({"keyword": tier}));
}

#[tokio::test]
async fn field_caps_conflicting_and_unmapped() {
    let app = seeded().await;
    let (_, r) = call(&app, Method::GET, "/_field_caps?fields=k,n", None).await;
    let mut kw = cap("keyword", true, true);
    kw["indices"] = json!(["a"]);
    let mut text = cap("text", true, false);
    text["indices"] = json!(["b"]);
    assert_eq!(
        r,
        json!({"indices": ["a", "b", "c"], "fields": {
            "k": {"keyword": kw, "text": text},
            "n": {"short": cap("short", true, true)},
        }})
    );
    let (_, r) = call(&app, Method::GET, "/a,c/_field_caps?fields=x", None).await;
    assert_eq!(
        r["fields"],
        json!({"x": {"float": cap("float", true, true)}})
    );
    let (_, r) = call(
        &app,
        Method::GET,
        "/a,c/_field_caps?fields=x&include_unmapped=true",
        None,
    )
    .await;
    let mut float = cap("float", true, true);
    float["indices"] = json!(["c"]);
    let mut unmapped = cap("unmapped", false, false);
    unmapped["indices"] = json!(["a"]);
    assert_eq!(
        r["fields"],
        json!({"x": {"float": float, "unmapped": unmapped}})
    );
}

#[tokio::test]
async fn field_caps_body_types_and_errors() {
    let app = seeded().await;
    let (s, r) = call(
        &app,
        Method::POST,
        "/_field_caps",
        Some(json!({"fields": ["n"], "index_filter": {"match_all": {}}})),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        r["fields"],
        json!({"n": {"short": cap("short", true, true)}})
    );
    let (_, r) = call(
        &app,
        Method::GET,
        "/a/_field_caps?fields=*&types=keyword",
        None,
    )
    .await;
    let mut tier = cap("keyword", true, true);
    tier["metadata_field"] = json!(true);
    assert_eq!(
        r["fields"],
        json!({"_tier": {"keyword": tier}, "k": {"keyword": cap("keyword", true, true)}})
    );
    let (_, r) = call(&app, Method::GET, "/_field_caps?fields=nomatch", None).await;
    assert_eq!(r, json!({"indices": ["a", "b", "c"], "fields": {}}));
    let (_, r) = call(&app, Method::GET, "/nope*/_field_caps?fields=x", None).await;
    assert_eq!(r, json!({"indices": [], "fields": {}}));

    let (s, r) = call(&app, Method::GET, "/_field_caps", None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(
        r["error"]["reason"],
        "Validation Failed: 1: no fields specified;"
    );
    let (s, r) = call(&app, Method::GET, "/nope/_field_caps?fields=x", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(r["error"]["type"], "index_not_found_exception");
    let (s, r) = call(
        &app,
        Method::POST,
        "/a/_field_caps?fields=k",
        Some(json!({"fields": ["n"]})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(
        r["error"]["reason"],
        "can't specify a request body and [fields] request parameter, either specify a request body or the [fields] request parameter"
    );
}

#[tokio::test]
async fn mapping_validation_empty_objects_are_left_out() {
    let app = app();
    let m = json!({"mappings": {"properties": {"e": {"type": "object"}}}});
    call(&app, Method::PUT, "/o", Some(m)).await;
    // Elasticsearch 9.5.5 leaves empty objects out.
    let (_, r) = call(
        &app,
        Method::GET,
        "/o/_field_caps?fields=*&filters=-metadata",
        None,
    )
    .await;
    assert_eq!(r, json!({"indices": ["o"], "fields": {}}));
}
