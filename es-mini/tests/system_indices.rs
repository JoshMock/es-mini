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

/// Elasticsearch's 403 for `elastic` doing `action` on the restricted index `index`.
fn elastic_restricted(action: &str, index: &str, privileges: &str) -> Value {
    let reason = format!(
        "action [{action}] is unauthorized for user [elastic] with effective roles [superuser] on restricted indices [{index}], this action is granted by the index privileges [{privileges}]"
    );
    let e = json!({"type": "security_exception", "reason": reason});
    json!({"error": {"root_cause": [e], "type": "security_exception", "reason": reason}, "status": 403})
}

#[tokio::test]
async fn elastic_may_not_create_write_or_delete_system_names() {
    let (app, _) = secured("elastic").await;
    let create = "create_index,manage,all";
    let write = "create_doc,create,delete,index,write,all";
    let auto = "auto_configure,create_index,manage,all";
    let delete = "delete_index,manage,all";
    let backing = ".ds-.fleet-actions-results-2026.01.01-000001";
    let cases = [
        ("PUT", "/.tasks", "indices:admin/create", ".tasks", create),
        (
            "PUT",
            "/.kibana_1",
            "indices:admin/create",
            ".kibana_1",
            create,
        ),
        (
            "PUT",
            "/.ml-config",
            "indices:admin/create",
            ".ml-config",
            create,
        ),
        (
            "POST",
            "/.tasks/_doc",
            "indices:data/write/bulk[s]",
            ".tasks",
            write,
        ),
        (
            "PUT",
            "/.tasks/_create/1",
            "indices:data/write/bulk[s]",
            ".tasks",
            write,
        ),
        (
            "DELETE",
            "/.tasks/_doc/1",
            "indices:data/write/bulk[s]",
            ".tasks",
            write,
        ),
        (
            "POST",
            "/.async-search/_doc",
            "indices:admin/auto_create",
            ".async-search",
            auto,
        ),
        (
            "PUT",
            "/.kibana_1/_doc/1",
            "indices:admin/auto_create",
            ".kibana_1",
            auto,
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
            "DELETE",
            "/.tasks",
            "indices:admin/delete",
            ".tasks",
            delete,
        ),
        (
            "DELETE",
            "/.tasks,logs",
            "indices:admin/delete",
            ".tasks",
            delete,
        ),
    ];
    for (method, uri, action, index, privileges) in cases {
        let body = (method != "DELETE").then(|| json!({"a": 1}));
        let got = call(&app, ELASTIC, method, uri, body).await;
        assert_eq!(
            got,
            (403, elastic_restricted(action, index, privileges)),
            "{method} {uri}"
        );
    }
    // Nothing was created, written or deleted.
    let uri = "/_cat/indices?expand_wildcards=all&format=json";
    let (status, body) = call(&app, ELASTIC, "GET", uri, None).await;
    let mut names: Vec<&str> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["index"].as_str().unwrap())
        .collect();
    names.sort();
    assert_eq!((status, names), (200, vec![".tasks", "logs"]));
    let (status, doc) = call(&app, ELASTIC, "GET", "/.tasks/_doc/1", None).await;
    assert_eq!((status, &doc["_source"]), (200, &json!({"a": 1})));
}

/// The value of setting `index.hidden` of `index`, as `elastic` sees it with security `off`.
async fn hidden(app: &axum::Router, index: &str) -> Value {
    let uri = format!("/{index}/_settings/index.hidden?expand_wildcards=all");
    let (status, body) = call(app, "", "GET", &uri, None).await;
    assert_eq!(status, 200, "{body}");
    body[index]["settings"]["index"]["hidden"].clone()
}

#[tokio::test]
async fn security_off_creates_system_indices_hidden() {
    // ponytail: es-mini creates them without their descriptors' mappings and settings (as
    // `.tasks`' `dynamic: strict`), so writes ES refuses as strict (`.async-search`) succeed
    let app = es_mini::app();
    for (method, uri, index) in [
        ("PUT", "/.tasks", ".tasks"),
        ("PUT", "/.ml-config", ".ml-config"),
        ("PUT", "/.kibana_1", ".kibana_1"),
        ("PUT", "/.kibana_3/_doc/1", ".kibana_3"),
    ] {
        let body = uri.contains("_doc").then(|| json!({"a": 1}));
        let (status, res) = call(&app, "", method, uri, body).await;
        assert!([200, 201].contains(&status), "{uri}: {res}");
        assert_eq!(hidden(&app, index).await, json!("true"), "{uri}");
    }
    // A system data stream's name is an ordinary index's.
    let (status, res) = call(&app, "", "PUT", "/.fleet-actions-results", None).await;
    assert_eq!(status, 200, "{res}");
    assert_eq!(hidden(&app, ".fleet-actions-results").await, Value::Null);
    // A system index cannot be created visible.
    let body = json!({"settings": {"index.hidden": false}});
    let reason = "Cannot create system index [.kibana_2] with [index.hidden] set to 'false'";
    let e = json!({"type": "illegal_state_exception", "reason": reason});
    let error = json!({"root_cause": [e], "type": "illegal_state_exception", "reason": reason});
    let got = call(&app, "", "PUT", "/.kibana_2", Some(body)).await;
    assert_eq!(got, (500, json!({"error": error, "status": 500})));
}

#[tokio::test]
async fn security_off_refuses_writes_to_system_data_streams() {
    let app = es_mini::app();
    let reason =
        "Data stream(s) [.fleet-actions-results] use and access is reserved for system operations";
    let e = json!({"type": "illegal_argument_exception", "reason": reason});
    let error = json!({"root_cause": [e], "type": "illegal_argument_exception", "reason": reason});
    let doc = json!({"@timestamp": "2026-01-01T00:00:00Z", "a": 1});
    let got = call(&app, "", "POST", "/.fleet-actions-results/_doc", Some(doc)).await;
    assert_eq!(got, (400, json!({"error": error, "status": 400})));
    let (status, _) = call(&app, "", "GET", "/.fleet-actions-results", None).await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn elastic_may_not_alias_system_indices() {
    let (app, _) = secured("aliases").await;
    for index in [".tasks", "logs,.tasks"] {
        let action = json!({"add": {"index": index, "alias": "a"}});
        let body = json!({"actions": [action]});
        let got = call(&app, ELASTIC, "POST", "/_aliases", Some(body)).await;
        let expected = elastic_restricted("indices:admin/aliases", ".tasks", "manage,all");
        assert_eq!(got, (403, expected), "{index}");
    }
    for method in ["PUT", "DELETE"] {
        let got = call(&app, ELASTIC, method, "/.tasks/_alias/a", None).await;
        let expected = elastic_restricted("indices:admin/aliases", ".tasks", "manage,all");
        assert_eq!(got, (403, expected), "{method}");
    }
    let (status, body) = call(&app, ELASTIC, "GET", "/_alias/a", None).await;
    assert_eq!(status, 404, "{body}");
}

/// The `Warning` header of `method uri` with `body`, authenticated by `auth`, after asserting
/// the request succeeds.
async fn warning(
    app: &axum::Router,
    auth: &str,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> Option<String> {
    let json = ("content-type", "application/json");
    let mut req = (Request::builder().method(method).uri(uri)).header(json.0, json.1);
    if !auth.is_empty() {
        req = req.header("authorization", auth);
    }
    let body = body.map_or(Body::empty(), |b| Body::from(b.to_string()));
    let res = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let warnings = res.headers().get_all("warning").iter();
    let warnings: Vec<_> = warnings.map(|w| w.to_str().unwrap().to_owned()).collect();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    assert!(status.as_u16() < 300, "{method} {uri}: {bytes:?}");
    assert!(warnings.len() < 2, "{method} {uri}: {warnings:?}");
    warnings.into_iter().next()
}

/// Elasticsearch's deprecation warning for a request accessing the system indices `names`.
fn accesses(names: &str) -> Option<String> {
    Some(format!(
        "299 Elasticsearch-9.5.5-48acba663b09e70b115dd879329141506c377890 \"this request accesses system indices: [{names}], but in a future major version, direct access to system indices will be prevented by default\""
    ))
}

#[tokio::test]
async fn accessing_system_indices_warns_of_deprecation() {
    let app = es_mini::app();
    let doc = || Some(json!({"a": 1}));
    // Creating one does not warn; auto-creating one by a write does.
    assert_eq!(warning(&app, "", "PUT", "/.tasks", None).await, None);
    let kibana = warning(&app, "", "PUT", "/.kibana_1/_doc/1?refresh=true", doc()).await;
    assert_eq!(kibana, accesses(".kibana_1"));
    let mget = json!({"docs": [{"_index": ".tasks", "_id": "1"}]});
    let alias = json!({"actions": [{"add": {"index": ".kibana_1", "alias": "kalias"}}]});
    let all = json!({"query": {"match_all": {}}});
    for (method, uri, body) in [
        ("PUT", "/.tasks/_doc/1?refresh=true", doc()),
        ("GET", "/.tasks/_doc/1", None),
        ("GET", "/.tasks/_search", None),
        ("GET", "/.tasks/_count", None),
        ("GET", "/.t*/_search", None),
        ("GET", "/.tasks/_mapping", None),
        ("GET", "/.tasks/_settings", None),
        ("POST", "/.tasks/_refresh", None),
        ("POST", "/_mget", Some(mget)),
        ("DELETE", "/.tasks/_doc/1", None),
        ("POST", "/.tasks/_delete_by_query", Some(all)),
    ] {
        let got = warning(&app, "", method, uri, body).await;
        assert_eq!(got, accesses(".tasks"), "{method} {uri}");
    }
    let both = warning(&app, "", "GET", "/.kibana_1,.tasks/_search", None).await;
    assert_eq!(both, accesses(".kibana_1, .tasks"));
    let all = warning(&app, "", "GET", "/_search?expand_wildcards=all", None).await;
    assert_eq!(all, accesses(".kibana_1, .tasks"));
    let aliased = warning(&app, "", "POST", "/_aliases", Some(alias)).await;
    assert_eq!(aliased, accesses(".kibana_1"));
    let via_alias = warning(&app, "", "GET", "/kalias/_search", None).await;
    assert_eq!(via_alias, accesses(".kibana_1"));
    // System data streams' backing indices and `_cat/indices` do not warn.
    let backing = "/.ds-.fleet-actions-results-2025.01.01-000001";
    for (method, uri, body) in [
        ("GET", "/_cat/indices/.tasks".to_owned(), None),
        ("PUT", format!("{backing}/_doc/1"), doc()),
        ("GET", format!("{backing}/_search"), None),
    ] {
        assert_eq!(warning(&app, "", method, &uri, body).await, None, "{uri}");
    }
    for index in [".kibana_1", ".tasks"] {
        let got = warning(&app, "", "DELETE", &format!("/{index}"), None).await;
        assert_eq!(got, accesses(index));
    }
}

#[tokio::test]
async fn elastic_reading_system_indices_is_warned_of_deprecation() {
    let (app, _) = secured("warning").await;
    for uri in ["/.tasks/_search", "/.t*/_search", "/.tasks/_doc/1"] {
        let got = warning(&app, ELASTIC, "GET", uri, None).await;
        assert_eq!(got, accesses(".tasks"), "{uri}");
    }
    for uri in ["/.security-7/_search", "/.security/_search"] {
        let got = warning(&app, ELASTIC, "GET", uri, None).await;
        assert_eq!(got, accesses(".security-7"), "{uri}");
    }
}
