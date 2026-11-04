//! Elasticsearch's system indices and system data streams (`SystemIndices.getSystemNameAutomaton`)
//! are restricted: only index privileges with `allow_restricted_indices` reach them. Expected
//! bodies were recorded from a live Elasticsearch 9.5.5 (`xpack.security.enabled=true`).

use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::collections::HashMap;
use tower::ServiceExt;

const ELASTIC: &str = "Basic ZWxhc3RpYzpjaGFuZ2VtZQ=="; // elastic:changeme

/// Status and body (JSON, or null) of `method uri` with `body`, authenticated by `auth`.
async fn call(
    app: &axum::Router,
    auth: &str,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (u16, Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json");
    if !auth.is_empty() {
        req = req.header("authorization", auth);
    }
    let req = req
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

/// A secured node over a data directory holding `logs` and the system index `.tasks` (one
/// document each), with API keys on `*` without and with restricted indices, as (app, [key id,
/// key header, restricted key id, restricted key header]).
async fn secured(test: &str) -> (axum::Router, [String; 4]) {
    let dir = std::env::temp_dir().join(format!("esm-system-{test}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let open = es_mini::app_persisted(&dir).unwrap();
    for index in ["logs", ".tasks"] {
        let (status, body) = call(
            &open,
            "",
            "PUT",
            &format!("/{index}/_doc/1"),
            Some(json!({"a": 1})),
        )
        .await;
        assert_eq!(status, 201, "{body}");
    }
    drop(open);
    let settings = HashMap::from([
        ("xpack.security.enabled".to_owned(), "true".to_owned()),
        ("bootstrap.password".to_owned(), "changeme".to_owned()),
    ]);
    let app = es_mini::app_persisted_with_settings(&dir, &settings).unwrap();
    let key = async |allow: bool| {
        let entry =
            json!({"names": ["*"], "privileges": ["all"], "allow_restricted_indices": allow});
        let body = json!({"name": "k", "role_descriptors": {"r": {"indices": [entry]}}});
        let (status, key) = call(&app, ELASTIC, "POST", "/_security/api_key", Some(body)).await;
        assert_eq!(status, 200, "{key}");
        let header = format!("ApiKey {}", key["encoded"].as_str().unwrap());
        [key["id"].as_str().unwrap().to_owned(), header]
    };
    let [k, kh] = key(false).await;
    let [r, rh] = key(true).await;
    let _ = std::fs::remove_dir_all(&dir);
    (app, [k, kh, r, rh])
}

/// Elasticsearch's 403 for API key `id` doing `action` on the restricted index `index`.
fn restricted(id: &str, action: &str, index: &str, privileges: &str) -> Value {
    let reason = format!(
        "action [{action}] is unauthorized for API key id [{id}] of user [elastic] on restricted indices [{index}], this action is granted by the index privileges [{privileges}]"
    );
    let e = json!({"type": "security_exception", "reason": reason});
    json!({"error": {"root_cause": [e], "type": "security_exception", "reason": reason}, "status": 403})
}

#[tokio::test]
async fn api_keys_without_restricted_indices_are_refused_system_names() {
    let (app, [k, kh, ..]) = secured("refused").await;
    let read = "read,all";
    let create = "create_index,manage,all";
    let write = "create_doc,create,delete,index,write,all";
    let auto = "auto_configure,create_index,manage,all";
    let backing = ".ds-.fleet-actions-results-2026.01.01-000001";
    let cases = [
        (
            "GET",
            "/.tasks/_search?size=0",
            "indices:data/read/search",
            ".tasks",
            read,
        ),
        (
            "GET",
            "/.tasks,logs/_search?size=0",
            "indices:data/read/search",
            ".tasks",
            read,
        ),
        (
            "GET",
            "/.kibana_1/_doc/1",
            "indices:data/read/get",
            ".kibana_1",
            read,
        ),
        (
            "GET",
            "/.kibana/_search",
            "indices:data/read/search",
            ".kibana",
            read,
        ),
        (
            "PUT",
            "/.kibana_1",
            "indices:admin/create",
            ".kibana_1",
            create,
        ),
        (
            "PUT",
            "/.tasks/_doc/1",
            "indices:data/write/bulk[s]",
            ".tasks",
            write,
        ),
        (
            "POST",
            "/.fleet-actions-results/_doc",
            "indices:admin/auto_create",
            ".fleet-actions-results",
            auto,
        ),
        (
            "POST",
            &format!("/{backing}/_doc"),
            "indices:admin/auto_create",
            backing,
            auto,
        ),
        (
            "PUT",
            "/.ml-config/_doc/1",
            "indices:admin/auto_create",
            ".ml-config",
            auto,
        ),
        (
            "DELETE",
            "/.tasks",
            "indices:admin/delete",
            ".tasks",
            "delete_index,manage,all",
        ),
    ];
    for (method, uri, action, index, privileges) in cases {
        let body = (method != "GET" && method != "DELETE").then(|| json!({"a": 1}));
        let got = call(&app, &kh, method, uri, body).await;
        assert_eq!(
            got,
            (403, restricted(&k, action, index, privileges)),
            "{method} {uri}"
        );
    }
}

#[tokio::test]
async fn wildcards_and_ignore_unavailable_skip_system_names_for_api_keys() {
    let (app, [_, kh, _, rh]) = secured("wildcards").await;
    let shards = async |auth: &str, uri: &str| {
        let (status, body) = call(&app, auth, "GET", uri, None).await;
        assert_eq!(status, 200, "{uri}: {body}");
        (
            body["_shards"]["total"].clone(),
            body["hits"]["total"]["value"].clone(),
        )
    };
    assert_eq!(
        shards(&kh, "/.tas*/_search?size=0&expand_wildcards=all").await,
        (json!(0), json!(0))
    );
    assert_eq!(
        shards(&kh, "/*/_search?size=0&expand_wildcards=all").await,
        (json!(1), json!(1))
    );
    let unavailable = "/.tasks,logs/_search?size=0&ignore_unavailable=true";
    assert_eq!(shards(&kh, unavailable).await, (json!(1), json!(1)));
    // allow_restricted_indices reaches them.
    assert_eq!(
        shards(&rh, "/.tasks/_search?size=0").await,
        (json!(1), json!(1))
    );
    assert_eq!(
        shards(&rh, "/.tas*/_search?size=0&expand_wildcards=all").await,
        (json!(1), json!(1))
    );
}

#[tokio::test]
async fn restricted_api_keys_are_limited_by_elastic() {
    let (app, [.., r, rh]) = secured("limited").await;
    let write = "create_doc,create,delete,index,write,all";
    let got = call(&app, &rh, "PUT", "/.tasks/_doc/1", Some(json!({"a": 1}))).await;
    assert_eq!(
        got,
        (
            403,
            restricted(&r, "indices:data/write/bulk[s]", ".tasks", write)
        )
    );
    let create = "create_index,manage,all";
    let got = call(&app, &rh, "PUT", "/.kibana_1", Some(json!({}))).await;
    assert_eq!(
        got,
        (
            403,
            restricted(&r, "indices:admin/create", ".kibana_1", create)
        )
    );
}
