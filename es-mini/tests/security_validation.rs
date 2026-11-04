//! With security off, `.security-7` is no restricted index: Elasticsearch 9.5.5 creates it from
//! its system index descriptor (SecuritySystemIndices.java) on `PUT /.security-7` or on a
//! document write to it or its `.security` alias, and deletes it on request.

use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

async fn call(app: &axum::Router, method: &str, uri: &str, body: &str) -> (u16, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_owned()))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status().as_u16();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// Elasticsearch's response to a write refused by `dynamic: strict`.
fn strict(at: &str, field: &str, parent: &str) -> (u16, Value) {
    let reason = format!(
        "[{at}] mapping set to strict, dynamic introduction of [{field}] within [{parent}] is not allowed"
    );
    let e = json!({"type": "strict_dynamic_mapping_exception", "reason": reason});
    let body = json!({"error": {"root_cause": [e], "type": e["type"], "reason": e["reason"]}, "status": 400});
    (400, body)
}

/// An API key document as Elasticsearch's ApiKeyService writes it, abridged.
const KEY: &str = r#"{"doc_type":"api_key","name":"k","api_key_hash":"h","creation_time":1,
  "role_descriptors":{"r":{"cluster":["all"]}},"limited_by_role_descriptors":{"superuser":{}},
  "metadata_flattened":{"a":{"b":1}},"creator":{"principal":"elastic","realm":"reserved"}}"#;

#[tokio::test]
async fn security_validation_security_off_creates_the_managed_index() {
    let app = es_mini::app();
    // The request's own mappings and aliases give way to the descriptor's.
    let body = r#"{"mappings":{"properties":{"x":{"type":"long"}}},"aliases":{"a":{}}}"#;
    let created =
        json!({"acknowledged": true, "shards_acknowledged": true, "index": ".security-7"});
    assert_eq!(
        call(&app, "PUT", "/.security-7", body).await,
        (200, created)
    );
    let alias = json!({".security-7": {"aliases": {".security": {"is_write_index": true, "is_hidden": true}}}});
    let got = call(&app, "GET", "/.security-7/_alias", "").await;
    assert_eq!(got, (200, alias));
    let (_, settings) = call(&app, "GET", "/.security-7/_settings?flat_settings=true", "").await;
    let settings = &settings[".security-7"]["settings"];
    for (k, v) in [
        ("index.hidden", "true"),
        ("index.number_of_shards", "1"),
        ("index.auto_expand_replicas", "0-1"),
        ("index.priority", "1000"),
        ("index.format", "6"),
        ("index.analysis.analyzer.email.tokenizer", "uax_url_email"),
    ] {
        assert_eq!(settings[k], v, "{k}: {settings}");
    }

    let doc = r#"{"username":"u","nope":1}"#;
    let got = call(&app, "PUT", "/.security-7/_doc/u", doc).await;
    assert_eq!(got, strict("1:24", "nope", "_doc"));
    let doc = r#"{"creator":{"principal":"p","x":"y"}}"#;
    let got = call(&app, "PUT", "/.security-7/_doc/u", doc).await;
    assert_eq!(got, strict("1:33", "x", "creator"));
    let (status, r) = call(&app, "PUT", "/.security/_doc/k", KEY).await;
    assert_eq!((status, &r["_index"]), (201, &json!(".security-7")), "{r}");

    let got = call(&app, "DELETE", "/.security-7", "").await;
    assert_eq!(got, (200, json!({"acknowledged": true})));
    let (status, _) = call(&app, "GET", "/.security-7/_alias", "").await;
    assert_eq!(status, 404);
}

/// A write to `.security` (as to `.security-7`) auto-creates `.security-7` from the descriptor.
#[tokio::test]
async fn security_validation_security_off_writes_auto_create_the_managed_index() {
    for target in [".security", ".security-7"] {
        let app = es_mini::app();
        let got = call(&app, "PUT", &format!("/{target}/_doc/1"), r#"{"x":1}"#).await;
        assert_eq!(got, strict("1:6", "x", "_doc"), "{target}");
        let (status, r) = call(&app, "PUT", &format!("/{target}/_doc/k"), KEY).await;
        assert_eq!((status, &r["_index"]), (201, &json!(".security-7")), "{r}");
        let (_, s) = call(&app, "GET", "/.security-7/_settings?flat_settings=true", "").await;
        let provided = &s[".security-7"]["settings"]["index.provided_name"];
        assert_eq!(provided, target, "{s}");
        let (_, m) = call(&app, "GET", "/.security-7/_mapping", "").await;
        assert_eq!(m[".security-7"]["mappings"]["dynamic"], "strict", "{m}");
    }
}

/// Reopened with security off, the index stays an ordinary one: still read, written, deleted.
#[tokio::test]
async fn security_validation_security_off_reopened_index_is_deletable() {
    let dir = std::env::temp_dir().join(format!("esm-secval-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let app = es_mini::app_persisted(&dir).unwrap();
    assert_eq!(call(&app, "PUT", "/.security-7/_doc/k", KEY).await.0, 201);
    drop(app);
    let app = es_mini::app_persisted(&dir).unwrap();
    let (status, r) = call(&app, "GET", "/.security-7/_doc/k", "").await;
    assert_eq!((status, &r["found"]), (200, &json!(true)), "{r}");
    let got = call(&app, "PUT", "/.security-7/_doc/1", r#"{"x":1}"#).await;
    assert_eq!(got, strict("1:6", "x", "_doc"));
    let got = call(&app, "DELETE", "/.security-7", "").await;
    assert_eq!(got, (200, json!({"acknowledged": true})));
    let _ = std::fs::remove_dir_all(&dir);
}
