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

#[tokio::test]
async fn api_gaps_field_caps_index_filter_drops_indices_without_matches() {
    let app = app();
    call(
        &app,
        Method::PUT,
        "/p/_doc/1?refresh=true",
        Some(json!({"k": "x"})),
    )
    .await;
    call(
        &app,
        Method::PUT,
        "/q/_doc/1?refresh=true",
        Some(json!({"k": "y", "m": 1})),
    )
    .await;
    let body = json!({"fields": ["*"], "index_filter": {"term": {"m": 1}}});
    let (s, r) = call(&app, Method::POST, "/_field_caps", Some(body)).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(r["indices"], json!(["q"]));
    assert!(r["fields"].get("m").is_some());
}

#[tokio::test]
async fn api_fidelity_field_caps_index_filter_drops_what_rewrites_to_match_none() {
    // Elasticsearch 9.5.5 drops an index only where the filter rewrites to match_none: unmapped
    // fields, constant fields, date ranges outside the shard's values, and bools of those.
    let app = app();
    let date = json!({"mappings": {"properties": {"d": {"type": "date"}}}});
    for i in ["d1", "d2", "d3"] {
        call(&app, Method::PUT, &format!("/{i}"), Some(date.clone())).await;
    }
    call(&app, Method::PUT, "/e", None).await;
    for (path, doc) in [
        ("/p/_doc/1", json!({"k": "x"})),
        ("/q/_doc/1", json!({"k": "y", "m": 1})),
        ("/d1/_doc/1", json!({"d": "2020-01-01"})),
        ("/d2/_doc/1", json!({"d": "2022-01-01"})),
        ("/d2/_doc/2", json!({"d": "2024-01-01"})),
    ] {
        call(
            &app,
            Method::PUT,
            &format!("{path}?refresh=true"),
            Some(doc),
        )
        .await;
    }
    let all = json!(["d1", "d2", "d3", "e", "p", "q"]);
    let mapped = json!(["d1", "d2", "d3", "p", "q"]);
    for (filter, indices) in [
        (json!({"term": {"k.keyword": "y"}}), json!(["p", "q"])),
        (json!({"term": {"m": 1}}), json!(["q"])),
        (json!({"match": {"m": 1}}), all.clone()),
        (
            json!({"range": {"d": {"gte": "2021-01-01"}}}),
            json!(["d2"]),
        ),
        (
            json!({"range": {"d": {"gte": "2022-06-01", "lt": "2023-01-01"}}}),
            json!(["d2"]),
        ),
        (json!({"term": {"_index": "p"}}), json!(["p"])),
        (json!({"term": {"_index": "D1"}}), json!([])),
        (
            json!({"prefix": {"_index": "d"}}),
            json!(["d1", "d2", "d3"]),
        ),
        (
            json!({"wildcard": {"_index": "d*"}}),
            json!(["d1", "d2", "d3"]),
        ),
        (
            json!({"bool": {"must_not": {"term": {"_index": "p"}}}}),
            json!(["d1", "d2", "d3", "e", "q"]),
        ),
        (json!({"exists": {"field": "m"}}), json!(["q"])),
        (json!({"terms": {"m": []}}), json!([])),
        (json!({"ids": {"values": ["1"]}}), mapped.clone()),
        (json!({"term": {"_tier": "data_content"}}), mapped.clone()),
        (json!({"term": {"_tier": "nope"}}), json!([])),
        (
            json!({"bool": {"should": [{"term": {"zz": 1}}, {"exists": {"field": "d"}}]}}),
            json!(["d1", "d2", "d3"]),
        ),
        (
            json!({"constant_score": {"filter": {"term": {"m": 1}}}}),
            json!(["q"]),
        ),
        (json!({"range": {"k": {"gte": "a"}}}), json!(["p", "q"])),
        (json!({"bool": {}}), all.clone()),
        (
            json!({"bool": {"filter": [{"match_all": {}}], "must_not": [{"match_all": {}}]}}),
            json!([]),
        ),
    ] {
        let body = json!({"fields": ["zz"], "index_filter": filter});
        let (s, r) = call(&app, Method::POST, "/_field_caps", Some(body)).await;
        assert_eq!(s, StatusCode::OK, "{filter}: {r}");
        assert_eq!(r["indices"], indices, "{filter}");
    }
}

#[tokio::test]
async fn api_gaps_field_caps_runtime_mappings_add_fields() {
    let app = seeded().await;
    let body = json!({"fields": ["rt", "n"], "runtime_mappings": {
        "rt": {"type": "keyword", "script": "emit('x')"}}});
    let (s, r) = call(&app, Method::POST, "/a/_field_caps", Some(body)).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        r["fields"]["rt"],
        json!({"keyword": cap("keyword", true, true)})
    );
    assert!(r["fields"].get("n").is_some());
}

#[tokio::test]
async fn field_caps_reports_the_mapping_runtime_fields() {
    // Elasticsearch 9.5.5 reports the leaves `dynamic: runtime` maps as runtime fields.
    let app = app();
    let body =
        json!({"mappings": {"dynamic": "runtime", "properties": {"k": {"type": "keyword"}}}});
    call(&app, Method::PUT, "/r", Some(body)).await;
    let doc = json!({"a": "x", "n": 1, "f": 1.5, "b": true});
    call(&app, Method::PUT, "/r/_doc/1?refresh=true", Some(doc)).await;
    let (s, r) = call(&app, Method::GET, "/r/_field_caps?fields=a,n,f,b,k", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        r,
        json!({"indices": ["r"], "fields": {
            "a": {"keyword": cap("keyword", true, true)},
            "b": {"boolean": cap("boolean", true, true)},
            "f": {"double": cap("double", true, true)},
            "k": {"keyword": cap("keyword", true, true)},
            "n": {"long": cap("long", true, true)},
        }})
    );
}
