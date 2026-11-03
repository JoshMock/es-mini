//! The `elastic` superuser reads the security index `.security-7` (behind the hidden alias
//! `.security`) and sees it in alias listings, but may not write or delete it; no API key
//! reaches it. Expected bodies were recorded from a live Elasticsearch 9.5.5
//! (`xpack.security.enabled=true`), whose index also holds its reserved roles.

use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::collections::HashMap;
use tower::ServiceExt;

const ELASTIC: &str = "Basic ZWxhc3RpYzpjaGFuZ2VtZQ=="; // elastic:changeme

/// A secured node holding one API key `k1` (with every privilege on `*`), as its id and its
/// `ApiKey` authorization header.
async fn app() -> (axum::Router, String, String) {
    let settings = HashMap::from([
        ("xpack.security.enabled".to_owned(), "true".to_owned()),
        ("bootstrap.password".to_owned(), "changeme".to_owned()),
    ]);
    let app = es_mini::app_with_settings(&settings).unwrap();
    let roles = json!({"r": {"indices": [{"names": ["*"], "privileges": ["all"]}]}});
    let body = json!({"name": "k1", "role_descriptors": roles});
    let (status, key) = call(&app, ELASTIC, "POST", "/_security/api_key", Some(body)).await;
    assert_eq!(status, 200, "{key}");
    let header = format!("ApiKey {}", key["encoded"].as_str().unwrap());
    (app, key["id"].as_str().unwrap().to_owned(), header)
}

/// Status and body (JSON, or null) of `method uri` with `body`, authenticated by `auth`.
async fn call(
    app: &axum::Router,
    auth: &str,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (u16, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", auth)
        .header("content-type", "application/json")
        .body(body.map_or(Body::empty(), |b| Body::from(b.to_string())))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status().as_u16();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// Elasticsearch's 403 for `elastic` doing `action` on the restricted index `index`.
fn restricted(action: &str, index: &str, privileges: &str) -> Value {
    let reason = format!(
        "action [{action}] is unauthorized for user [elastic] with effective roles [superuser] on restricted indices [{index}], this action is granted by the index privileges [{privileges}]"
    );
    let e = json!({"type": "security_exception", "reason": reason});
    json!({"error": {"root_cause": [e], "type": "security_exception", "reason": reason}, "status": 403})
}

#[tokio::test]
async fn elastic_gets_documents_of_the_security_index() {
    let (app, id, _) = app().await;
    for index in [".security-7", ".security"] {
        let (status, doc) = call(&app, ELASTIC, "GET", &format!("/{index}/_doc/{id}"), None).await;
        assert_eq!(status, 200, "{doc}");
        let meta = json!({"_index": ".security-7", "_id": id, "_version": 1, "_seq_no": 0, "_primary_term": 1, "found": true});
        for (k, v) in meta.as_object().unwrap() {
            assert_eq!(&doc[k], v, "{index} {k}: {doc}");
        }
        assert_eq!(doc["_source"]["doc_type"], "api_key", "{doc}");
        assert_eq!(doc["_source"]["name"], "k1", "{doc}");
    }
    let got = call(&app, ELASTIC, "GET", "/.security-7/_doc/missing", None).await;
    let missing = json!({"_index": ".security-7", "_id": "missing", "found": false});
    assert_eq!(got, (404, missing));
}

#[tokio::test]
async fn elastic_searches_the_security_index() {
    let (app, id, _) = app().await;
    for index in [".security", ".security-7"] {
        let (status, r) = call(&app, ELASTIC, "GET", &format!("/{index}/_search"), None).await;
        assert_eq!(status, 200, "{r}");
        assert_eq!(r["_shards"]["total"], 1, "{r}");
        let hits = r["hits"]["hits"].as_array().unwrap();
        assert!(
            (hits.iter()).any(|h| h["_id"] == id.as_str() && h["_index"] == ".security-7"),
            "{index}: {r}"
        );
    }
    // Hidden, so not in a search of every index.
    let (_, r) = call(&app, ELASTIC, "GET", "/_search", None).await;
    assert_eq!(r["hits"]["total"]["value"], 0, "{r}");
}

#[tokio::test]
async fn elastic_may_not_write_or_delete_the_security_index() {
    let (app, id, _) = app().await;
    let bulk = "create_doc,create,delete,index,write,all";
    for index in [".security-7", ".security"] {
        let got = call(
            &app,
            ELASTIC,
            "PUT",
            &format!("/{index}/_doc/x"),
            Some(json!({"a": 1})),
        )
        .await;
        assert_eq!(
            got,
            (403, restricted("indices:data/write/bulk[s]", index, bulk))
        );
        let got = call(&app, ELASTIC, "DELETE", &format!("/{index}"), None).await;
        let expected = restricted("indices:admin/delete", index, "delete_index,manage,all");
        assert_eq!(got, (403, expected));
    }
    let got = call(
        &app,
        ELASTIC,
        "DELETE",
        &format!("/.security-7/_doc/{id}"),
        None,
    )
    .await;
    assert_eq!(
        got,
        (
            403,
            restricted("indices:data/write/bulk[s]", ".security-7", bulk)
        )
    );
    let got = call(
        &app,
        ELASTIC,
        "GET",
        &format!("/.security-7/_doc/{id}"),
        None,
    )
    .await;
    assert_eq!(got.0, 200, "{}", got.1);
}

#[tokio::test]
async fn elastic_sees_the_security_alias() {
    let (app, _, _) = app().await;
    call(
        &app,
        ELASTIC,
        "PUT",
        "/a",
        Some(json!({"aliases": {"aa": {}}})),
    )
    .await;
    let security = json!({".security-7": {"aliases": {".security": {"is_hidden": true}}}});
    let mut all = security.clone();
    all["a"] = json!({"aliases": {"aa": {}}});
    for (uri, expected) in [
        ("/_alias", &all),
        ("/_aliases", &all),
        ("/_alias/.security", &security),
        ("/_alias/.sec*", &security),
        ("/.security-7/_alias", &security),
    ] {
        assert_eq!(
            call(&app, ELASTIC, "GET", uri, None).await,
            (200, expected.clone()),
            "{uri}"
        );
    }
    assert_eq!(
        call(&app, ELASTIC, "HEAD", "/_alias/.security", None)
            .await
            .0,
        200
    );
    let row = |alias: &str, index: &str| json!({"alias": alias, "index": index, "filter": "-", "routing.index": "-", "routing.search": "-", "is_write_index": "-"});
    let got = call(&app, ELASTIC, "GET", "/_cat/aliases?format=json", None).await;
    assert_eq!(
        got,
        (
            200,
            json!([row(".security", ".security-7"), row("aa", "a")])
        )
    );
    let got = call(
        &app,
        ELASTIC,
        "GET",
        "/_cat/aliases/.security?format=json",
        None,
    )
    .await;
    assert_eq!(got, (200, json!([row(".security", ".security-7")])));
}

/// Even a key with every privilege on `*` reads, lists and deletes none of it.
#[tokio::test]
async fn api_keys_still_cannot_reach_the_security_index() {
    let (app, id, key) = app().await;
    for (m, uri) in [
        ("GET", format!("/.security-7/_doc/{id}")),
        ("GET", format!("/.security/_doc/{id}")),
        ("GET", "/.security/_search".to_owned()),
        ("GET", "/.security-7/_search".to_owned()),
        ("GET", "/_alias/.security".to_owned()),
        ("GET", "/.security-7/_alias".to_owned()),
        ("DELETE", "/.security-7".to_owned()),
    ] {
        let (status, r) = call(&app, &key, m, &uri, None).await;
        assert!(status >= 400, "{m} {uri}: {status} {r}");
        assert!(!r.to_string().contains("api_key_hash"), "{m} {uri}: {r}");
    }
    let (status, r) = call(&app, &key, "GET", "/_alias", None).await;
    assert_eq!((status, r.get(".security-7")), (200, None), "{r}");
    let (status, r) = call(&app, &key, "GET", "/_cat/aliases?format=json", None).await;
    assert_eq!((status, r), (200, json!([])));
}
