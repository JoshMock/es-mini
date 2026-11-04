//! API keys are authorized by their role descriptors: cluster privileges, and index privileges
//! on index names and patterns (aliases included), limited by their creator's. Expected bodies
//! were recorded from a live Elasticsearch 9.5.5 (`xpack.security.enabled=true`, basic license).

use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::collections::HashMap;
use tower::ServiceExt;

const ELASTIC: &str = "Basic ZWxhc3RpYzpjaGFuZ2VtZQ=="; // elastic:changeme

/// Status and body of `method uri` with `body`, authenticated by `auth`.
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
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body)
}

async fn ok(app: &axum::Router, auth: &str, method: &str, uri: &str, body: Option<Value>) -> Value {
    let (status, out) = call(app, auth, method, uri, body).await;
    assert!(status < 300, "{method} {uri}: {status} {out}");
    out
}

/// A key `elastic` creates with `roles`, as its `ApiKey` authorization header, and its id.
async fn key(app: &axum::Router, auth: &str, roles: Value) -> (String, String) {
    let body = json!({"name": "k", "role_descriptors": roles});
    let key = ok(app, auth, "POST", "/_security/api_key", Some(body)).await;
    let header = format!("ApiKey {}", key["encoded"].as_str().unwrap());
    (header, key["id"].as_str().unwrap().to_owned())
}

/// Indices a1, a2 (closed), b1 (behind alias `al`) and c1 (behind alias `ca`), one doc each.
async fn app() -> axum::Router {
    let settings = HashMap::from([
        ("xpack.security.enabled".to_owned(), "true".to_owned()),
        ("bootstrap.password".to_owned(), "changeme".to_owned()),
    ]);
    let app = es_mini::app_with_settings(&settings).unwrap();
    for (index, body) in [
        ("a1", json!({})),
        ("a2", json!({})),
        ("b1", json!({"aliases": {"al": {}}})),
        ("c1", json!({"aliases": {"ca": {}}})),
    ] {
        ok(&app, ELASTIC, "PUT", &format!("/{index}"), Some(body)).await;
        let doc = Some(json!({"x": 1}));
        let uri = format!("/{index}/_doc/1?refresh=true");
        ok(&app, ELASTIC, "PUT", &uri, doc).await;
    }
    ok(&app, ELASTIC, "POST", "/a2/_close", None).await;
    app
}

fn read_a() -> Value {
    json!({"r": {"indices": [{"names": ["a*"], "privileges": ["read"]}]}})
}

/// Elasticsearch's 403 for API key `id` denied `action`, granted by `privileges` (cluster or
/// index ones) and naming `indices` if any.
fn denied(id: &str, action: &str, indices: &str, privileges: &str) -> Value {
    let on = match indices {
        "" => String::new(),
        i => format!(" on indices [{i}]"),
    };
    let kind = if action.starts_with("cluster:") {
        "cluster"
    } else {
        "index"
    };
    let reason = format!(
        "action [{action}] is unauthorized for API key id [{id}] of user [elastic]{on}, this action is granted by the {kind} privileges [{privileges}]"
    );
    let e = json!({"type": "security_exception", "reason": reason});
    json!({"error": {"root_cause": [e], "type": "security_exception", "reason": reason}, "status": 403})
}

async fn assert_denied(app: &axum::Router, auth: &str, method: &str, uri: &str, expected: Value) {
    let (status, body) = call(app, auth, method, uri, Some(json!({}))).await;
    assert_eq!(status, 403, "{method} {uri}: {body}");
    assert_eq!(body, expected, "{method} {uri}");
}

/// The indices of the hits of `GET uri`.
async fn hit_indices(app: &axum::Router, auth: &str, uri: &str) -> Vec<String> {
    let out = ok(app, auth, "GET", uri, None).await;
    let hits = out["hits"]["hits"].as_array().unwrap();
    let mut indices: Vec<String> = (hits.iter())
        .map(|h| h["_index"].as_str().unwrap().to_owned())
        .collect();
    indices.sort();
    indices
}

#[tokio::test]
async fn cluster_privileges() {
    let app = app().await;
    let (kr, id) = key(&app, ELASTIC, read_a()).await;
    let monitor = "monitor,manage,all";
    for (uri, action) in [
        ("/", "cluster:monitor/main"),
        ("/_cluster/health", "cluster:monitor/health"),
        ("/_cluster/health/a1", "cluster:monitor/health"),
        ("/_nodes", "cluster:monitor/nodes/info"),
    ] {
        assert_denied(&app, &kr, "GET", uri, denied(&id, action, "", monitor)).await;
    }
    let stats = "cross_cluster_search,monitor_stats,monitor,manage,all";
    let expected = denied(&id, "cluster:monitor/stats", "", stats);
    assert_denied(&app, &kr, "GET", "/_cluster/stats", expected).await;
    let get = "read_security,manage_api_key,manage_security,all";
    let expected = denied(&id, "cluster:admin/xpack/security/api_key/get", "", get);
    assert_denied(&app, &kr, "GET", "/_security/api_key", expected).await;
    let create = "manage_own_api_key,manage_api_key,manage_security,all";
    let expected = denied(
        &id,
        "cluster:admin/xpack/security/api_key/create",
        "",
        create,
    );
    assert_denied(&app, &kr, "POST", "/_security/api_key", expected).await;
    // Any key may ask who it is.
    ok(&app, &kr, "GET", "/_security/_authenticate", None).await;

    let roles = json!({"r": {"cluster": ["monitor"]}});
    let (kc, _) = key(&app, ELASTIC, roles).await;
    ok(&app, &kc, "GET", "/_cluster/health", None).await;
    let roles = json!({"r": {"cluster": ["cluster:monitor/main"]}});
    let (by_action, id) = key(&app, ELASTIC, roles).await;
    ok(&app, &by_action, "GET", "/", None).await;
    let expected = denied(&id, "cluster:monitor/health", "", monitor);
    assert_denied(&app, &by_action, "GET", "/_cluster/health", expected).await;
}

#[tokio::test]
async fn wildcards_expand_to_authorized_names_only() {
    let app = app().await;
    let (kr, id) = key(&app, ELASTIC, read_a()).await;
    // `a*` grants the alias `al`, so b1 is readable through it, but not by its own name.
    for uri in ["/_search", "/*/_search", "/_all/_search", "/a*/_search"] {
        assert_eq!(hit_indices(&app, &kr, uri).await, ["a1", "b1"], "{uri}");
    }
    assert_eq!(hit_indices(&app, &kr, "/al/_search").await, ["b1"]);
    assert_eq!(hit_indices(&app, &kr, "/a*,-a1/_search").await, ["b1"]);
    let out = ok(&app, &kr, "GET", "/b*/_search", None).await;
    assert_eq!(out["hits"]["total"]["value"], 0);
    let out = ok(&app, &kr, "GET", "/_count", None).await;
    assert_eq!(out["count"], 2);

    let read = "read,all";
    let search = "indices:data/read/search";
    for (uri, names) in [
        ("/b1/_search", "b1"),
        ("/a1,b1/_search", "b1"),
        // Elasticsearch lists them in Java HashSet order.
        ("/b1,c1/_search", "c1,b1"),
        ("/nope/_search", "nope"),
    ] {
        assert_denied(&app, &kr, "GET", uri, denied(&id, search, names, read)).await;
    }
    let expected = denied(&id, "indices:data/read/get", "b1", read);
    assert_denied(&app, &kr, "GET", "/b1/_doc/1", expected).await;

    let (status, out) = call(&app, &kr, "GET", "/b*/_search?allow_no_indices=false", None).await;
    assert_eq!(status, 404, "{out}");
    assert_eq!(out["error"]["reason"], "no such index [b*]");
    let (status, out) = call(&app, &kr, "GET", "/a9/_search", None).await;
    assert_eq!(status, 404, "{out}");
    let (status, out) = call(&app, &kr, "GET", "/a2/_search", None).await;
    assert_eq!(
        (status, &out["error"]["type"]),
        (400, &json!("index_closed_exception"))
    );

    // Privileges on the index do not cover reading other aliases of it.
    let roles = json!({"r": {"indices": [{"names": ["al", "c1"], "privileges": ["read"]}]}});
    let (kal, id) = key(&app, ELASTIC, roles).await;
    assert_eq!(hit_indices(&app, &kal, "/_search").await, ["b1", "c1"]);
    let expected = denied(&id, search, "ca", read);
    assert_denied(&app, &kal, "GET", "/ca/_search", expected).await;
}

#[tokio::test]
async fn keys_without_the_privilege_anywhere() {
    let app = app().await;
    let (kz, id) = key(&app, ELASTIC, json!({"r": {}})).await;
    let search = "indices:data/read/search";
    for uri in ["/_search", "/*/_search", "/_count"] {
        assert_denied(&app, &kz, "GET", uri, denied(&id, search, "", "read,all")).await;
    }
    let expected = denied(&id, search, "a1", "read,all");
    assert_denied(&app, &kz, "GET", "/a1,b*/_search", expected).await;
    for (method, uri, action, names, privileges) in [
        (
            "GET",
            "/a1",
            "indices:admin/get",
            "a1",
            "view_index_metadata,manage,all",
        ),
        (
            "GET",
            "/_mapping",
            "indices:admin/mappings/get",
            "",
            "view_index_metadata,manage,all",
        ),
        (
            "GET",
            "/_settings",
            "indices:monitor/settings/get",
            "",
            "monitor,view_index_metadata,manage,all",
        ),
        (
            "GET",
            "/_alias",
            "indices:admin/aliases/get",
            "",
            "view_index_metadata,manage,all",
        ),
        (
            "GET",
            "/a1/_stats",
            "indices:monitor/stats",
            "a1",
            "monitor,cross_cluster_replication,manage,all",
        ),
        (
            "GET",
            "/_field_caps?fields=*",
            "indices:data/read/field_caps",
            "",
            "view_index_metadata,manage,read,all",
        ),
        (
            "PUT",
            "/a3",
            "indices:admin/create",
            "a3",
            "create_index,manage,all",
        ),
        (
            "DELETE",
            "/a1",
            "indices:admin/delete",
            "a1",
            "delete_index,manage,all",
        ),
        (
            "POST",
            "/a1/_refresh",
            "indices:admin/refresh",
            "a1",
            "maintenance,manage,all",
        ),
        (
            "POST",
            "/a1/_close",
            "indices:admin/close",
            "a1",
            "manage_follow_index,manage,all",
        ),
        (
            "PUT",
            "/a1/_mapping",
            "indices:admin/mapping/put",
            "a1",
            "manage,all",
        ),
        (
            "PUT",
            "/a1/_doc/1",
            "indices:data/write/index",
            "a1",
            "create_doc,create,index,write,all",
        ),
        (
            "PUT",
            "/zz/_doc/1",
            "indices:data/write/index",
            "zz",
            "create_doc,create,index,write,all",
        ),
        (
            "DELETE",
            "/a1/_doc/1",
            "indices:data/write/delete",
            "a1",
            "delete,write,all",
        ),
        (
            "POST",
            "/a1/_update/1",
            "indices:data/write/update",
            "a1",
            "index,write,all",
        ),
        (
            "POST",
            "/a1/_update_by_query",
            "indices:data/write/update/byquery",
            "a1",
            "index,write,all",
        ),
        (
            "POST",
            "/_bulk",
            "indices:data/write/bulk",
            "",
            "create_doc,create,delete,index,write,all",
        ),
        ("POST", "/_mget", "indices:data/read/mget", "", "read,all"),
        (
            "POST",
            "/_query",
            "indices:data/read/esql",
            "",
            "read_cross_cluster,read,all",
        ),
        (
            "POST",
            "/a1/_pit?keep_alive=1m",
            "indices:data/read/open_point_in_time",
            "a1",
            "read,all",
        ),
    ] {
        let expected = denied(&id, action, names, privileges);
        assert_denied(&app, &kz, method, uri, expected).await;
    }
}

#[tokio::test]
async fn write_privileges() {
    let app = app().await;
    let roles = json!({"r": {"indices": [
        {"names": ["a*"], "privileges": ["write"]},
        {"names": ["n*"], "privileges": ["create_index"]},
    ]}});
    let (kw, id) = key(&app, ELASTIC, roles).await;
    ok(&app, &kw, "PUT", "/a1/_doc/2", Some(json!({"x": 1}))).await;
    ok(&app, &kw, "POST", "/a1/_doc", Some(json!({"x": 1}))).await;
    ok(&app, &kw, "PUT", "/a1/_create/3", Some(json!({"x": 1}))).await;
    let update = Some(json!({"doc": {"x": 5}}));
    ok(&app, &kw, "POST", "/a1/_update/2", update).await;
    ok(&app, &kw, "DELETE", "/a1/_doc/2", None).await;
    let auto = "auto_configure,create_index,manage,all";
    let expected = denied(&id, "indices:admin/auto_create", "a5", auto);
    assert_denied(&app, &kw, "PUT", "/a5/_doc/1", expected).await;
    ok(&app, &kw, "PUT", "/n1", None).await;
    let bulk = "create_doc,create,delete,index,write,all";
    let expected = denied(&id, "indices:data/write/bulk[s]", "n1", bulk);
    assert_denied(&app, &kw, "PUT", "/n1/_doc/1", expected).await;
    let expected = denied(&id, "indices:admin/create", "a6", "create_index,manage,all");
    assert_denied(&app, &kw, "PUT", "/a6", expected).await;
    // By-query writes search first.
    let expected = denied(&id, "indices:data/read/search", "a1", "read,all");
    assert_denied(&app, &kw, "POST", "/a1/_update_by_query", expected).await;
}

#[tokio::test]
async fn manage_privileges() {
    let app = app().await;
    let roles = json!({"r": {"indices": [{"names": ["a*"], "privileges": ["manage"]}]}});
    let (km, id) = key(&app, ELASTIC, roles).await;
    let out = ok(&app, &km, "GET", "/_mapping", None).await;
    let names = out.as_object().unwrap();
    assert!(
        names.contains_key("a1") && !names.contains_key("c1"),
        "{out}"
    );
    let out = ok(&app, &km, "GET", "/b*/_mapping", None).await;
    assert_eq!(out, json!({}));
    ok(&app, &km, "PUT", "/a8", None).await;
    // Adding an alias needs the privilege on the alias name too.
    ok(&app, &km, "PUT", "/a1/_alias/a3", None).await;
    let expected = denied(&id, "indices:admin/aliases", "", "manage,all");
    assert_denied(&app, &km, "PUT", "/a1/_alias/z", expected).await;
    let state = "cross_cluster_replication,read_ccr,transport_client,manage_ccr,monitor,manage,all";
    let expected = denied(&id, "cluster:monitor/state", "", state);
    assert_denied(&app, &km, "GET", "/_cat/shards", expected.clone()).await;
    assert_denied(&app, &km, "GET", "/_cat/indices", expected).await;
    ok(&app, &km, "DELETE", "/a1", None).await;
}

#[tokio::test]
async fn roles_of_a_key_add_up() {
    let app = app().await;
    let roles = json!({
        "r": {"indices": [{"names": ["a*"], "privileges": ["read"]}]},
        "s": {"indices": [{"names": ["b*", "c*"], "privileges": ["read"]}], "cluster": ["monitor"]},
    });
    let (k2, _) = key(&app, ELASTIC, roles).await;
    assert_eq!(hit_indices(&app, &k2, "/_search").await, ["a1", "b1", "c1"]);
    ok(&app, &k2, "GET", "/_cluster/health", None).await;
}

#[tokio::test]
async fn derived_keys_are_limited_by_their_creator() {
    let app = app().await;
    let roles =
        json!({"r": {"cluster": ["all"], "indices": [{"names": ["*"], "privileges": ["all"]}]}});
    let (kall, _) = key(&app, ELASTIC, roles).await;
    let (derived, id) = key(&app, &kall, json!({"x": {}})).await;
    let expected = denied(&id, "indices:data/read/search", "a1", "read,all");
    assert_denied(&app, &derived, "GET", "/a1/_search", expected).await;
    let expected = denied(&id, "cluster:monitor/health", "", "monitor,manage,all");
    assert_denied(&app, &derived, "GET", "/_cluster/health", expected).await;
    // A key without role descriptors has its creator's (here `elastic`'s) privileges.
    let body = json!({"name": "plain"});
    let plain = ok(&app, ELASTIC, "POST", "/_security/api_key", Some(body)).await;
    let plain = format!("ApiKey {}", plain["encoded"].as_str().unwrap());
    assert_eq!(
        hit_indices(&app, &plain, "/_search").await,
        ["a1", "b1", "c1"]
    );
}

/// Field- and document-level security need a license es-mini does not have, so a key whose
/// role uses them gets the error Elasticsearch gives on its basic license.
#[tokio::test]
async fn field_and_document_level_security_are_refused() {
    let app = app().await;
    for entry in [
        json!({"names": ["a*"], "privileges": ["read"], "field_security": {"grant": ["x"]}}),
        json!({"names": ["a*"], "privileges": ["read"], "query": {"term": {"x": 5}}}),
    ] {
        let (k, _) = key(&app, ELASTIC, json!({"r": {"indices": [entry]}})).await;
        let reason = "current license is non-compliant for [field and document level security]";
        let e = json!({
            "type": "security_exception",
            "reason": reason,
            "license.expired.feature": "field and document level security",
            "indices_with_dls_or_fls": "a1",
        });
        let mut error = e.clone();
        error["root_cause"] = json!([e]);
        let expected = json!({"error": error, "status": 403});
        for uri in ["/a1/_search", "/a1/_doc/1"] {
            assert_denied(&app, &k, "GET", uri, expected.clone()).await;
        }
    }
}

/// `_has_privileges` answers as Elasticsearch 9.5.5 did for the keys and requests recorded in
/// `data/has_privileges.json` (a `null` role is a key without role descriptors, which has
/// `elastic`'s privileges, and `elastic` itself).
#[tokio::test]
async fn has_privileges_as_recorded() {
    let app = app().await;
    let data: Value =
        serde_json::from_str(include_str!("data/has_privileges.json")).expect("valid JSON");
    for case in data["cases"].as_array().unwrap() {
        let auths = match &case["roles"] {
            Value::Null => {
                let body = json!({"name": "plain"});
                let plain = ok(&app, ELASTIC, "POST", "/_security/api_key", Some(body)).await;
                vec![
                    ELASTIC.to_owned(),
                    format!("ApiKey {}", plain["encoded"].as_str().unwrap()),
                ]
            }
            roles => vec![key(&app, ELASTIC, roles.clone()).await.0],
        };
        for auth in auths {
            for method in ["GET", "POST"] {
                let uri = "/_security/user/_has_privileges";
                let request = Some(case["request"].clone());
                let (status, out) = call(&app, &auth, method, uri, request).await;
                assert_eq!(status, 200, "{case}: {out}");
                // Compared as text, for the order of the keys.
                assert_eq!(out.to_string(), case["response"].to_string(), "{case}");
            }
        }
    }
}

#[tokio::test]
async fn has_privileges_errors_as_recorded() {
    let app = app().await;
    let (k, _) = key(&app, ELASTIC, read_a()).await;
    let data: Value =
        serde_json::from_str(include_str!("data/has_privileges.json")).expect("valid JSON");
    for case in data["errors"].as_array().unwrap() {
        let request = case["request"].as_str().unwrap();
        let req = Request::builder()
            .method("POST")
            .uri("/_security/user/_has_privileges")
            .header("authorization", &k)
            .header("content-type", "application/json")
            .body(Body::from(request.to_owned()))
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status().as_u16(), 400, "{request}");
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let out: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(out, case["response"], "{request}");
    }
}

/// Status and body of `POST uri` with the NDJSON `body`, authenticated by `auth`.
async fn ndjson(app: &axum::Router, auth: &str, uri: &str, body: &str) -> (u16, Value) {
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("authorization", auth)
        .header("content-type", "application/x-ndjson")
        .body(Body::from(body.to_owned()))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status().as_u16();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}

/// Each bulk item is authorized on its own index, failing alone (recorded from Elasticsearch
/// 9.5.5).
#[tokio::test]
async fn bulk_items_are_authorized_by_index() {
    let app = app().await;
    let roles = json!({"r": {"indices": [
        {"names": ["a*"], "privileges": ["write"]},
        {"names": ["n*"], "privileges": ["create_index"]},
    ]}});
    let (kw, id) = key(&app, ELASTIC, roles).await;
    let body = concat!(
        "{\"index\":{\"_index\":\"a1\",\"_id\":\"5\"}}\n{\"x\":1}\n",
        "{\"index\":{\"_index\":\"b1\",\"_id\":\"5\"}}\n{\"x\":1}\n",
        "{\"create\":{\"_index\":\"a7\",\"_id\":\"5\"}}\n{\"x\":1}\n",
    );
    let (status, out) = ndjson(&app, &kw, "/_bulk", body).await;
    assert_eq!(status, 200, "{out}");
    assert_eq!(out["errors"], true);
    assert_eq!(out["items"][0]["index"]["status"], 201, "{out}");
    let reason = |action: &str, index: &str, privileges: &str| {
        format!(
            "action [{action}] is unauthorized for API key id [{id}] of user [elastic] on indices [{index}], this action is granted by the index privileges [{privileges}]"
        )
    };
    let bulk = "create_doc,create,delete,index,write,all";
    let expected = json!([
        {"index": {"_index": "b1", "_id": "5", "status": 403, "error": {
            "type": "security_exception",
            "reason": reason("indices:data/write/bulk[s]", "b1", bulk),
        }}},
        {"create": {"_index": "a7", "_id": "5", "status": 403, "error": {
            "type": "security_exception",
            "reason": reason("indices:admin/auto_create", "a7", "auto_configure,create_index,manage,all"),
        }}},
    ]);
    // Compared as text, for the order of the keys.
    let denied = json!(out["items"].as_array().unwrap()[1..]);
    assert_eq!(denied.to_string(), expected.to_string());
    let (_, b1) = call(&app, ELASTIC, "GET", "/b1/_doc/5", None).await;
    assert_eq!(b1["found"], false, "{b1}");
}

/// Each mget item is authorized by the names that reach its index: an index any of whose names
/// the key may not read fails all its items, named by the concrete index (recorded from
/// Elasticsearch 9.5.5: an alias grants its items only, `ca` fails `c1`'s too).
#[tokio::test]
async fn mget_items_are_authorized_by_index() {
    let app = app().await;
    let roles = json!({"r": {"indices": [
        {"names": ["al"], "privileges": ["read"]},
        {"names": ["c1"], "privileges": ["read"]},
    ]}});
    let (kal, id) = key(&app, ELASTIC, roles).await;
    let docs: Vec<Value> = ["al", "b1", "ca", "c1", "c*"]
        .iter()
        .map(|i| json!({"_index": i, "_id": "1"}))
        .collect();
    let out = ok(&app, &kal, "POST", "/_mget", Some(json!({"docs": docs}))).await;
    let mget = "indices:data/read/mget[shard]";
    let on = |index: &str, names: &str| json!({"_index": index, "error": denied(&id, mget, names, "read,all")["error"]});
    let got: Vec<Value> = (out["docs"].as_array().unwrap().iter())
        .map(|d| json!({"_index": d["_index"], "error": d["error"]}))
        .collect();
    assert_eq!(
        got[..4],
        [
            on("b1", "b1"),
            on("b1", "b1"),
            on("c1", "ca"),
            on("c1", "ca")
        ]
    );
    assert_eq!(got[4]["error"]["type"], "index_not_found_exception");
    let out = ok(&app, &kal, "POST", "/al/_mget", Some(json!({"ids": ["1"]}))).await;
    assert_eq!(out["docs"][0]["found"], true, "{out}");
    let out = ok(&app, &kal, "POST", "/b1/_mget", Some(json!({"ids": ["1"]}))).await;
    assert_eq!(out["docs"][0]["error"], on("b1", "b1")["error"]);
}

/// mget items are authorized per shard like Elasticsearch's shard requests: a name the key may
/// not read fails the items on its shard only (ids 12 and 1 route to shards 0 and 1 of two).
#[tokio::test]
async fn shard_reads_mget_items_are_authorized_by_shard() {
    let app = app().await;
    let body = json!({"settings": {"number_of_shards": 2}, "aliases": {"sx": {}}});
    ok(&app, ELASTIC, "PUT", "/s2", Some(body)).await;
    for id in ["1", "12"] {
        let uri = format!("/s2/_doc/{id}?refresh=true");
        ok(&app, ELASTIC, "PUT", &uri, Some(json!({"x": 1}))).await;
    }
    let roles = json!({"r": {"indices": [{"names": ["s2"], "privileges": ["read"]}]}});
    let (k, id) = key(&app, ELASTIC, roles).await;
    let docs = json!([
        {"_index": "sx", "_id": "12"},
        {"_index": "s2", "_id": "12"},
        {"_index": "s2", "_id": "1"},
    ]);
    let out = ok(&app, &k, "POST", "/_mget", Some(json!({"docs": docs}))).await;
    let error = denied(&id, "indices:data/read/mget[shard]", "sx", "read,all")["error"].clone();
    assert_eq!(out["docs"][0]["error"], error, "{out}");
    assert_eq!(out["docs"][1]["error"], error, "{out}");
    assert_eq!(out["docs"][2]["found"], true, "{out}");
}

/// Each msearch search is authorized alone as `indices:data/read/search`: a name the key may
/// not read fails its search with the 403, unmatched wildcards find nothing (recorded from
/// Elasticsearch 9.5.5).
#[tokio::test]
async fn msearch_searches_are_authorized_one_by_one() {
    let app = app().await;
    let roles = json!({"r": {"indices": [
        {"names": ["al"], "privileges": ["read"]},
        {"names": ["c1"], "privileges": ["read"]},
    ]}});
    let (kal, id) = key(&app, ELASTIC, roles).await;
    let body: String = ["al", "ca", "c1,b1", "nope", "x*", "*"]
        .iter()
        .map(|i| format!("{{\"index\":\"{i}\"}}\n{{}}\n"))
        .collect();
    let (status, out) = ndjson(&app, &kal, "/_msearch", &body).await;
    assert_eq!(status, 200, "{out}");
    let r = &out["responses"];
    assert_eq!(r[0]["status"], 200, "{out}");
    assert_eq!(r[0]["hits"]["hits"][0]["_index"], "b1", "{out}");
    let search = |names| denied(&id, "indices:data/read/search", names, "read,all");
    assert_eq!(r[1], search("ca"));
    assert_eq!(r[2], search("b1"));
    assert_eq!(r[3], search("nope"));
    assert_eq!(r[4]["status"], 200, "{out}");
    assert_eq!(r[4]["hits"]["total"]["value"], 0, "{out}");
    // A wildcard expands to what the key may read: al (b1) and c1.
    assert_eq!(r[5]["hits"]["total"]["value"], 2, "{out}");
    let (status, out) = ndjson(&app, &kal, "/al/_msearch", "{}\n{}\n").await;
    assert_eq!(status, 200, "{out}");
    assert_eq!(out["responses"][0]["hits"]["total"]["value"], 1, "{out}");
}

async fn esql(app: &axum::Router, auth: &str, q: &str) -> (u16, Value) {
    call(app, auth, "POST", "/_query", Some(json!({ "query": q }))).await
}

/// The `_index` of every row `FROM sources` returns, sorted.
async fn esql_indices(app: &axum::Router, auth: &str, sources: &str) -> Value {
    let q = format!("FROM {sources} METADATA _index | KEEP _index | SORT _index");
    let (status, out) = esql(app, auth, &q).await;
    assert_eq!(status, 200, "{q}: {out}");
    out["values"].clone()
}

/// ES|QL reads only the FROM names the key may read: wildcards expand to those, and a name it
/// may not read is unknown when nothing else matches, else a 403 (recorded from
/// Elasticsearch 9.5.5).
#[tokio::test]
async fn esql_reads_authorized_indices_only() {
    let app = app().await;
    let (kr, id) = key(&app, ELASTIC, read_a()).await;
    let unknown = |names: &str| {
        let e =
            json!({"type": "verification_exception", "reason": format!("Unknown index [{names}]")});
        json!({"error": {"root_cause": [e.clone()], "type": e["type"], "reason": e["reason"]}, "status": 400})
    };
    for q in ["b1", "b1,nope", "b*,b1"] {
        let out = esql(&app, &kr, &format!("FROM {q} | LIMIT 5")).await;
        assert_eq!(out, (400, unknown(q)), "{q}");
    }
    let shards = |id, names| {
        denied(
            id,
            "indices:data/read/esql/search_shards",
            names,
            "read,all",
        )
    };
    for (q, names) in [("a1,b1", "b1"), ("a1,nope", "nope")] {
        let out = esql(&app, &kr, &format!("FROM {q} | LIMIT 5")).await;
        assert_eq!(out, (403, shards(&id, names)), "{q}");
    }
    // `a*` matches the alias `al` of b1 too.
    assert_eq!(esql_indices(&app, &kr, "*").await, json!([["a1"], ["b1"]]));
    let (status, out) = esql(&app, &kr, "FROM b* | LIMIT 5").await;
    assert_eq!((status, &out["values"]), (200, &json!([])), "{out}");
    let roles = json!({"r": {"indices": [
        {"names": ["al"], "privileges": ["read"]},
        {"names": ["c1"], "privileges": ["read"]},
    ]}});
    let (kal, id) = key(&app, ELASTIC, roles).await;
    let all = esql_indices(&app, &kal, "*").await;
    assert_eq!(all, json!([["b1"], ["c1"]]));
    let out = esql(&app, &kal, "FROM al,ca | LIMIT 5").await;
    assert_eq!(out, (403, shards(&id, "ca")));
    let out = esql(&app, &kal, "FROM b1 | LIMIT 5").await;
    assert_eq!(out, (400, unknown("b1")));
}

/// The aliases of every index, as elastic sees them.
async fn aliases(app: &axum::Router) -> Value {
    ok(app, ELASTIC, "GET", "/_alias", None).await
}

/// POST _aliases checks each action's indices, then their alias names, and expands wildcards
/// to the names the key may manage; an alias may not grant more than its indices (recorded
/// from Elasticsearch 9.5.5).
#[tokio::test]
async fn alias_actions_need_manage_on_indices_and_aliases() {
    let app = app().await;
    let post = |actions: Value| Some(json!({ "actions": actions }));
    let manage_a = json!({"r": {"indices": [{"names": ["a*"], "privileges": ["manage"]}]}});
    let (km, id) = key(&app, ELASTIC, manage_a).await;
    let manage = |names| denied(&id, "indices:admin/aliases", names, "manage,all");
    for (actions, names) in [
        (json!([{"add": {"index": "a1", "alias": "x1"}}]), ""),
        (
            json!([{"add": {"index": "b1", "alias": "zz"}}, {"add": {"index": "a1", "alias": "yy"}}]),
            "b1",
        ),
        (json!([{"add": {"index": "nope", "alias": "a-v"}}]), "nope"),
        (json!([{"remove": {"index": "b1", "alias": "*"}}]), "b1"),
        (json!([{"remove_index": {"index": "b1"}}]), "b1"),
    ] {
        let out = call(&app, &km, "POST", "/_aliases", post(actions.clone())).await;
        assert_eq!(out, (403, manage(names)), "{actions}");
    }
    let (status, out) = call(
        &app,
        &km,
        "POST",
        "/_aliases",
        post(json!([{"add": {"index": "q*", "alias": "a-v"}}])),
    )
    .await;
    assert_eq!(
        (status, &out["error"]["reason"]),
        (404, &json!("no such index [q*]"))
    );
    let add_all = json!([{"add": {"index": "*", "alias": "a-v"}}]);
    ok(&app, &km, "POST", "/_aliases", post(add_all)).await;
    let all = aliases(&app).await;
    assert_eq!(all["a1"]["aliases"], json!({"a-v": {}}));
    assert_eq!(all["b1"]["aliases"], json!({"al": {}}));
    let remove_all = json!([{"remove": {"index": "*", "alias": "*"}}]);
    ok(&app, &km, "POST", "/_aliases", post(remove_all)).await;
    let all = aliases(&app).await;
    assert_eq!(all["a1"]["aliases"], json!({}));
    assert_eq!(all["b1"]["aliases"], json!({"al": {}}));

    let roles = json!({"r": {"indices": [
        {"names": ["a*"], "privileges": ["manage"]},
        {"names": ["ax*"], "privileges": ["manage", "read"]},
    ]}});
    let (ks, _) = key(&app, ELASTIC, roles).await;
    let reason = "Adding an alias is not allowed when the alias has more permissions than any of the indices";
    let e = json!({"type": "security_exception", "reason": reason});
    let more = json!({"error": {"root_cause": [e], "type": "security_exception", "reason": reason}, "status": 403});
    let out = call(
        &app,
        &ks,
        "POST",
        "/_aliases",
        post(json!([{"add": {"index": "a1", "alias": "ax1"}}])),
    )
    .await;
    assert_eq!(out, (403, more));
    ok(
        &app,
        &ks,
        "POST",
        "/_aliases",
        post(json!([{"add": {"index": "a1", "alias": "a-ok"}}])),
    )
    .await;

    let delete_a = json!({"r": {"indices": [{"names": ["a*"], "privileges": ["delete_index"]}]}});
    let (kd, id) = key(&app, ELASTIC, delete_a).await;
    let out = call(
        &app,
        &kd,
        "POST",
        "/_aliases",
        post(json!([{"remove_index": {"index": "a1"}}])),
    )
    .await;
    assert_eq!(
        out,
        (
            403,
            denied(&id, "indices:admin/aliases", "a1", "manage,all")
        )
    );
}

#[tokio::test]
async fn percent_encoded_paths_are_authorized_decoded() {
    let app = app().await;
    let (k, _) = key(&app, ELASTIC, read_a()).await;
    // `a1%2Cb1` is `a1,b1` once decoded: b1 must not be readable through it.
    for uri in ["/a1%2Cb1/_search", "/a1%2cb1/_count", "/%62%31/_search"] {
        let (status, body) = call(&app, &k, "GET", uri, None).await;
        assert_eq!(status, 403, "{uri}: {body}");
    }
    // An encoded name the key may read still works.
    let (status, body) = call(&app, &k, "GET", "/%61%31/_search", None).await;
    assert_eq!(
        (status, &body["hits"]["total"]["value"]),
        (200, &json!(1)),
        "{body}"
    );
}

/// Every route the server defines (read from its source, so a new route is covered without
/// editing this list) refuses a key with no privileges, except those Elasticsearch lets any
/// key use. Guards against a route reaching `steps`' catch-all, which checks nothing.
#[tokio::test]
async fn every_route_needs_a_privilege() {
    // Handlers that check items themselves, or actions that need no privilege in Elasticsearch.
    let open = [
        "GET /_security/_authenticate",
        "GET /_security/user/_has_privileges",
        "POST /_security/user/_has_privileges",
        "DELETE /_pit",
        "DELETE /_search/scroll",
        "DELETE /_search/scroll/{scroll_id}",
        // Each action is authorized by the handler; `{}` has none and fails validation first.
        "POST /_aliases",
    ];
    let source = [
        include_str!("../src/lib.rs"),
        include_str!("../src/security.rs"),
    ]
    .concat();
    let route =
        regex::Regex::new(r#"\.route\(\s*"([^"]+)",\s*((?:\.?\w+\([\w:]+\)\s*)+)"#).unwrap();
    let method = regex::Regex::new(r"(\w+)\([\w:]+\)").unwrap();
    let app = app().await;
    let (k, _) = key(&app, ELASTIC, json!({"none": {}})).await;
    let mut open_routes = vec![];
    let mut checked = 0;
    for r in route.captures_iter(&source) {
        let path = &r[1];
        for m in method.captures_iter(&r[2]) {
            let m = m[1].to_uppercase();
            let route = format!("{m} {path}");
            let uri = (path.replace("{index}", "a1").replace("{id}", "1"))
                .replace("{name}", "x")
                .replace("{metric}", "docs")
                .replace("{filter}", "_all")
                .replace("{scroll_id}", "x");
            let body = (m != "GET" && m != "HEAD" && m != "DELETE").then(|| json!({}));
            let (status, out) = call(&app, &k, &m, &uri, body).await;
            checked += 1;
            if (status == 403) == open.contains(&route.as_str()) {
                open_routes.push(format!("{route}: {status} {out}"));
            }
        }
    }
    assert!(checked > 60, "parsed only {checked} routes from the source");
    assert!(open_routes.is_empty(), "{}", open_routes.join("\n"));
}

/// Recorded from Elasticsearch 9.5.5: only the key that opened a scroll may continue it; another
/// key finds no context, and its attempt frees the scroll. Anyone may clear a scroll. (Points in
/// time are not tied to their creator either.)
#[tokio::test]
async fn only_the_scrolls_creator_may_continue_it() {
    let app = app().await;
    let (a, _) = key(&app, ELASTIC, read_a()).await;
    let (b, _) = key(&app, ELASTIC, read_a()).await;
    let open = |auth: String| {
        let app = app.clone();
        async move {
            let r = ok(
                &app,
                &auth,
                "POST",
                "/a1/_search?scroll=1m",
                Some(json!({"size": 1})),
            )
            .await;
            json!({"scroll": "1m", "scroll_id": r["_scroll_id"]})
        }
    };
    let missing = |(status, r): (u16, Value)| {
        let kind = &r["error"]["root_cause"][0]["type"];
        assert_eq!(
            (status, kind),
            (404, &json!("search_context_missing_exception")),
            "{r}"
        );
    };

    let scroll = open(a.clone()).await;
    ok(&app, &a, "POST", "/_search/scroll", Some(scroll.clone())).await;
    missing(call(&app, &b, "POST", "/_search/scroll", Some(scroll.clone())).await);
    missing(call(&app, &a, "POST", "/_search/scroll", Some(scroll.clone())).await);
    missing(call(&app, ELASTIC, "POST", "/_search/scroll", Some(scroll)).await);

    let scroll = open(a.clone()).await;
    let r = ok(&app, &b, "DELETE", "/_search/scroll", Some(scroll.clone())).await;
    assert_eq!(r, json!({"succeeded": true, "num_freed": 1}));
}

/// Recorded from Elasticsearch 9.5.5: a key searches a point in time by the indices it was opened
/// on, whoever opened it; one it may not read is refused without naming the indices, and an
/// index in the path is still invalid with a point in time.
#[tokio::test]
async fn points_in_time_are_authorized_by_their_indices() {
    let app = app().await;
    let (a, id) = key(&app, ELASTIC, read_a()).await;
    let (b, _) = key(&app, ELASTIC, read_a()).await;
    let pit = |auth: String, path: &'static str| {
        let app = app.clone();
        async move {
            let r = ok(&app, &auth, "POST", path, None).await;
            json!({"size": 1, "pit": {"id": r["id"]}})
        }
    };
    let mine = pit(a.clone(), "/a1/_pit?keep_alive=1m").await;
    for auth in [&a, &b] {
        let r = ok(&app, auth, "POST", "/_search", Some(mine.clone())).await;
        assert_eq!(r["hits"]["total"]["value"], 1, "{r}");
    }
    let search = "indices:data/read/search";
    for path in ["/a1,b1/_pit?keep_alive=1m", "/b1/_pit?keep_alive=1m"] {
        let body = pit(ELASTIC.to_owned(), path).await;
        let expected = denied(&id, search, "", "read,all");
        let (status, r) = call(&app, &a, "POST", "/_search", Some(body)).await;
        assert_eq!((status, r), (403, expected), "{path}");
    }
    let (status, r) = call(&app, &a, "POST", "/a1/_search", Some(mine)).await;
    assert_eq!(
        (status, &r["error"]["type"]),
        (400, &json!("action_request_validation_exception")),
        "{r}"
    );
}

/// The responses of `_msearch` at `path` with the NDJSON `lines`, as `auth`.
async fn msearch(app: &axum::Router, auth: &str, path: &str, lines: &[Value]) -> (u16, Value) {
    let ndjson: String = lines.iter().map(|l| format!("{l}\n")).collect();
    let req = Request::builder()
        .method("POST")
        .uri(path)
        .header("authorization", auth)
        .header("content-type", "application/x-ndjson")
        .body(Body::from(ndjson))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status().as_u16();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// Recorded from Elasticsearch 9.5.5: a point in time is authorized by the concrete indices it was
/// opened on, even through an alias, so a privilege on the alias alone does not grant it.
#[tokio::test]
async fn a_point_in_time_opened_through_an_alias_needs_its_indices() {
    let app = app().await;
    ok(&app, ELASTIC, "PUT", "/a1/_alias/aa", None).await;
    let (a, _) = key(&app, ELASTIC, read_a()).await;
    let al = json!({"r": {"indices": [{"names": ["al"], "privileges": ["read"]}]}});
    let (k, id) = key(&app, ELASTIC, al).await;
    let mut pits = vec![];
    for alias in ["aa", "al"] {
        let r = ok(
            &app,
            ELASTIC,
            "POST",
            &format!("/{alias}/_pit?keep_alive=1m"),
            None,
        )
        .await;
        pits.push(json!({"size": 0, "pit": {"id": r["id"]}}));
    }
    let r = ok(&app, &a, "POST", "/_search", Some(pits[0].clone())).await;
    assert_eq!(r["hits"]["total"]["value"], 1, "{r}");
    let (status, r) = call(&app, &a, "POST", "/_search", Some(pits[1].clone())).await;
    assert_eq!(status, 403, "{r}");
    // `al` is readable as an alias, but its point in time is not.
    ok(&app, &k, "POST", "/al/_search", Some(json!({"size": 0}))).await;
    let (status, r) = call(&app, &k, "POST", "/_search", Some(pits[1].clone())).await;
    let expected = denied(&id, "indices:data/read/search", "", "read,all");
    assert_eq!((status, r), (403, expected));
}

/// Recorded from Elasticsearch 9.5.5: each point-in-time search of an msearch is authorized
/// alone, failing only its own entry; an index named beside a point in time anywhere fails the
/// whole request.
#[tokio::test]
async fn msearch_points_in_time_are_authorized_one_by_one() {
    let app = app().await;
    let (a, id) = key(&app, ELASTIC, read_a()).await;
    let mut pit = std::collections::HashMap::new();
    for target in ["a1", "b1", "a1,b1"] {
        let r = ok(
            &app,
            ELASTIC,
            "POST",
            &format!("/{target}/_pit?keep_alive=1m"),
            None,
        )
        .await;
        pit.insert(target, json!({"size": 0, "pit": {"id": r["id"]}}));
    }
    let lines = [
        json!({}),
        pit["a1"].clone(),
        json!({}),
        pit["b1"].clone(),
        json!({}),
        pit["a1,b1"].clone(),
        json!({"index": "a1"}),
        json!({"size": 0}),
    ];
    let (status, r) = msearch(&app, &a, "/_msearch", &lines).await;
    assert_eq!(status, 200, "{r}");
    let statuses: Vec<&Value> = (r["responses"].as_array().unwrap().iter())
        .map(|i| &i["status"])
        .collect();
    assert_eq!(
        statuses,
        [&json!(200), &json!(403), &json!(403), &json!(200)],
        "{r}"
    );
    let mut expected = denied(&id, "indices:data/read/search", "", "read,all");
    assert_eq!(r["responses"][0]["hits"]["total"]["value"], 1, "{r}");
    expected["status"] = 403.into();
    assert_eq!(r["responses"][1], expected);

    let invalid = "action_request_validation_exception";
    for (path, header) in [
        ("/_msearch", json!({"index": "a1"})),
        ("/_msearch", json!({"index": "_all"})),
        ("/a1/_msearch", json!({})),
    ] {
        let lines = [
            json!({}),
            json!({"size": 0}),
            header.clone(),
            pit["a1"].clone(),
        ];
        let (status, r) = msearch(&app, &a, path, &lines).await;
        assert_eq!(
            (status, &r["error"]["type"]),
            (400, &json!(invalid)),
            "{path} {header}: {r}"
        );
    }
}

/// Role index names are Elasticsearch's `Automatons` patterns: `?` is any one character, `\`
/// escapes, and `/.../` is a Lucene regular expression (recorded from Elasticsearch 9.5.5).
#[tokio::test]
async fn security_authz_gaps_question_marks_and_regexes_in_role_names() {
    let app = app().await;
    let roles = json!({"r": {"indices": [{"names": ["a?", "/c.*/"], "privileges": ["read"]}]}});
    let (k, id) = key(&app, ELASTIC, roles).await;
    assert_eq!(hit_indices(&app, &k, "/a1/_search").await, ["a1"]);
    // `a?` matches alias `al`, which reaches b1.
    assert_eq!(
        hit_indices(&app, &k, "/*/_search").await,
        ["a1", "b1", "c1"]
    );
    let read = "read,all";
    let expected = denied(&id, "indices:data/read/search", "b1", read);
    assert_denied(&app, &k, "GET", "/b1/_search", expected).await;
    let body = json!({"index": [{"names": ["a1", "b1", "ca"], "privileges": ["read"]}]});
    let out = ok(
        &app,
        &k,
        "POST",
        "/_security/user/_has_privileges",
        Some(body),
    )
    .await;
    let expected = json!({"a1": {"read": true}, "b1": {"read": false}, "ca": {"read": true}});
    assert_eq!(out["index"], expected);

    let roles =
        json!({"r": {"indices": [{"names": ["a\\1", "/~([ab].*)/"], "privileges": ["read"]}]}});
    let (k, _) = key(&app, ELASTIC, roles).await;
    assert_eq!(hit_indices(&app, &k, "/*/_search").await, ["a1", "c1"]);

    for bad in ["/[a/", "/abc", "/"] {
        let roles = json!({"r": {"indices": [{"names": [bad], "privileges": ["read"]}]}});
        let body = json!({"name": "k", "role_descriptors": roles});
        let (status, out) = call(&app, ELASTIC, "POST", "/_security/api_key", Some(body)).await;
        assert_eq!(status, 400, "{bad}: {out}");
        let reason = format!("Validation Failed: 1: invalid regular expression pattern [{bad}];");
        assert_eq!(out["error"]["reason"], reason, "{bad}");
    }
}

/// A removed alias pattern matching no alias the key may manage removes nothing and fails as
/// Elasticsearch's does: an action that removes nothing is a 404 naming the aliases as given,
/// and `-` excludes (recorded from Elasticsearch 9.5.5).
#[tokio::test]
async fn security_authz_gaps_alias_removal_without_manageable_match() {
    let app = app().await;
    let post = |actions: Value| Some(json!({ "actions": actions }));
    let add = json!([{"add": {"index": "a1", "aliases": ["al1", "al2", "bl1"]}}]);
    ok(&app, ELASTIC, "POST", "/_aliases", post(add)).await;
    let roles = json!({"r": {"indices": [{"names": ["a1", "al*"], "privileges": ["manage"]}]}});
    let (k, _) = key(&app, ELASTIC, roles).await;
    let missing = |out: &Value, names: &str| {
        let reason = format!("aliases [{names}] missing");
        assert_eq!(out["error"]["type"], "aliases_not_found_exception", "{out}");
        assert_eq!(out["error"]["reason"], reason, "{out}");
    };
    let remove = |extra: Value| {
        let mut r = json!({"index": "a1", "alias": "bl*"});
        r.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        post(json!([{ "remove": r }]))
    };
    let (status, out) = call(&app, &k, "POST", "/_aliases", remove(json!({}))).await;
    assert_eq!(status, 404, "{out}");
    missing(&out, "bl*");
    let (status, out) = call(
        &app,
        &k,
        "POST",
        "/_aliases",
        remove(json!({"must_exist": true})),
    )
    .await;
    assert_eq!(status, 404, "{out}");
    missing(&out, "*");

    let actions = json!([
        {"remove": {"index": "a1", "alias": "bl*"}},
        {"add": {"index": "a1", "alias": "al3"}},
    ]);
    let out = ok(&app, &k, "POST", "/_aliases", post(actions)).await;
    assert_eq!(out["errors"], true, "{out}");
    let results = &out["action_results"];
    let action = json!({"type": "remove", "indices": ["a1"], "aliases": ["bl*"]});
    assert_eq!(
        (&results[0]["action"], &results[0]["status"]),
        (&action, &json!(404))
    );
    missing(&json!({"error": results[0]["error"]}), "bl*");
    assert_eq!(results[1]["status"], 200);

    let actions = json!([{"remove": {"index": "a1", "aliases": ["al*", "-al2"]}}]);
    ok(&app, &k, "POST", "/_aliases", post(actions)).await;
    let a1 = &aliases(&app).await["a1"]["aliases"];
    assert_eq!(a1, &json!({"al2": {}, "bl1": {}}));
}

/// Alias names in paths are checked as Elasticsearch checks them: a deleted wildcard narrows to
/// the aliases the key may manage (failing if none), and an added name is one literal name
/// (recorded from Elasticsearch 9.5.5).
#[tokio::test]
async fn security_authz_gaps_alias_wildcards_in_paths() {
    let app = app().await;
    let add = |names: Value| {
        let actions = json!([{"add": {"index": "a1", "aliases": names}}]);
        Some(json!({ "actions": actions }))
    };
    ok(
        &app,
        ELASTIC,
        "POST",
        "/_aliases",
        add(json!(["al1", "al2", "bl1"])),
    )
    .await;
    let roles = json!({"r": {"indices": [{"names": ["a1", "al*"], "privileges": ["manage"]}]}});
    let (k, id) = key(&app, ELASTIC, roles).await;
    let manage = denied(&id, "indices:admin/aliases", "", "manage,all");
    let (status, out) = call(&app, &k, "DELETE", "/a1/_alias/bl*", None).await;
    assert_eq!(status, 404, "{out}");
    assert_eq!(out["error"]["reason"], "aliases [bl*] missing");
    let out = call(&app, &k, "DELETE", "/a1/_alias/bl*,zz", None).await;
    assert_eq!(out, (403, manage.clone()));
    ok(&app, &k, "DELETE", "/a1/_alias/b*,al1", None).await;
    let a1 = |all: Value| all["a1"]["aliases"].clone();
    assert_eq!(a1(aliases(&app).await), json!({"al2": {}, "bl1": {}}));
    ok(&app, &k, "DELETE", "/a1/_alias/*", None).await;
    assert_eq!(a1(aliases(&app).await), json!({"bl1": {}}));
    ok(
        &app,
        ELASTIC,
        "POST",
        "/_aliases",
        add(json!(["al1", "al2"])),
    )
    .await;
    ok(&app, &k, "DELETE", "/a1/_alias/al*,-al2", None).await;
    assert_eq!(a1(aliases(&app).await), json!({"al2": {}, "bl1": {}}));

    let out = call(&app, &k, "PUT", "/a1/_alias/b*", None).await;
    assert_eq!(out, (403, manage));
    // Elasticsearch authorizes these, then refuses them as invalid alias names.
    for name in ["al*", "al9,b9"] {
        let (status, out) = call(&app, &k, "PUT", &format!("/a1/_alias/{name}"), None).await;
        assert_ne!(status, 403, "{name}: {out}");
    }
}

/// `_has_privileges` compares any named privilege with action patterns through the actions
/// Elasticsearch's `ClusterPrivilegeResolver` and `IndexPrivilege` give it, with an action name
/// standing for itself and its sub-actions (`name*`) (recorded from Elasticsearch 9.5.5).
#[tokio::test]
async fn security_authz_gaps_named_privileges_implied_by_action_patterns() {
    let app = app().await;
    let cases = [
        (
            json!({"cluster": ["cluster:admin/slm/*", "cluster:admin/ilm/start", "cluster:admin/ilm/stop", "cluster:admin/ilm/operation_mode/get"],
                "indices": [{"names": ["a*"], "privileges": ["indices:admin/create", "indices:admin/auto_create", "indices:admin/data_stream/*"]}]}),
            json!({"cluster": ["manage_slm", "read_slm", "monitor_snapshot", "manage_ilm", "monitor_data_stream_global_retention"],
                "index": [{"names": ["a1"], "privileges": ["create_index", "delete_index", "auto_configure", "manage_data_stream_lifecycle"]}]}),
            json!({"username":"elastic","has_all_requested":false,"cluster":{"manage_ilm":false,"read_slm":true,"monitor_data_stream_global_retention":true,"manage_slm":true,"monitor_snapshot":false},"index":{"a1":{"auto_configure":false,"create_index":true,"delete_index":false,"manage_data_stream_lifecycle":true}},"application":{}}),
        ),
        (
            json!({"cluster": ["all"], "indices": [{"names": ["*"], "privileges": ["all"]}]}),
            json!({"cluster": ["manage_slm", "create_snapshot", "manage_connector", "read_fleet_secrets"],
                "index": [{"names": ["a1"], "privileges": ["manage_view", "view_index_metadata", "cross_cluster_replication_internal", "read_failure_store"]}]}),
            json!({"username":"elastic","has_all_requested":true,"cluster":{"read_fleet_secrets":true,"manage_slm":true,"manage_connector":true,"create_snapshot":true},"index":{"a1":{"cross_cluster_replication_internal":true,"manage_view":true,"read_failure_store":true,"view_index_metadata":true}},"application":{}}),
        ),
        (
            json!({"cluster": ["manage"], "indices": [{"names": ["a1"], "privileges": ["read"]}, {"names": ["b1"], "privileges": ["view_index_metadata"]}]}),
            json!({"cluster": ["manage_saml", "manage_connector", "read_connector_secrets", "manage_slm", "delegate_pki"],
                "index": [{"names": ["a1"], "privileges": ["read_cross_cluster", "indices:data/read/search", "view_index_metadata", "read_failure_store", "indices:admin/resolve/cluster"]},
                    {"names": ["b1"], "privileges": ["indices:admin/validate/query", "indices:data/read/field_caps", "indices:admin/mappings/fields/get[index]", "read"]}]}),
            json!({"username":"elastic","has_all_requested":false,"cluster":{"read_connector_secrets":true,"manage_slm":true,"delegate_pki":false,"manage_connector":true,"manage_saml":false},"index":{"a1":{"indices:admin/resolve/cluster":false,"indices:data/read/search":true,"read_cross_cluster":true,"read_failure_store":false,"view_index_metadata":false},"b1":{"indices:admin/mappings/fields/get[index]":true,"indices:admin/validate/query":true,"indices:data/read/field_caps":true,"read":false}},"application":{}}),
        ),
        (
            json!({"cluster": ["manage_connector"]}),
            json!({"cluster": ["monitor_connector", "read_connector_secrets", "manage_connector", "none"]}),
            json!({"username":"elastic","has_all_requested":false,"cluster":{"monitor_connector":true,"read_connector_secrets":false,"none":true,"manage_connector":true},"index":{},"application":{}}),
        ),
        (
            json!({"cluster": ["manage_own_api_key"]}),
            json!({"cluster": ["monitor_data_stream_global_retention", "none"]}),
            json!({"username":"elastic","has_all_requested":false,"cluster":{"monitor_data_stream_global_retention":false,"none":false},"index":{},"application":{}}),
        ),
        (
            json!({"cluster": ["none"]}),
            json!({"cluster": ["monitor_data_stream_global_retention", "none"]}),
            json!({"username":"elastic","has_all_requested":true,"cluster":{"monitor_data_stream_global_retention":true,"none":true},"index":{},"application":{}}),
        ),
    ];
    for (role, request, expected) in cases {
        let (k, _) = key(&app, ELASTIC, json!({"r": role})).await;
        let uri = "/_security/user/_has_privileges";
        let out = ok(&app, &k, "POST", uri, Some(request.clone())).await;
        // Compared as text, for the order of the keys.
        assert_eq!(out.to_string(), expected.to_string(), "{role} {request}");
    }
}

/// Patterns that can name the security index (`?`, `/regex/`) still cannot reach it: it is
/// outside the indices any request but the security APIs resolves.
#[tokio::test]
async fn patterns_cannot_reach_the_security_index() {
    let app = app().await;
    for names in [
        json!(["/.*/"]),
        json!(["?security*", ".security*"]),
        json!(["*"]),
    ] {
        let roles = json!({"r": {"indices": [{"names": names, "privileges": ["all"]}]}});
        let (k, _) = key(&app, ELASTIC, roles).await;
        for (m, uri, body) in [
            ("GET", "/.security-7/_search", None),
            ("GET", "/.security/_search", None),
            ("GET", "/.security*/_search", None),
            ("GET", "/.security-7/_doc/x", None),
            ("PUT", "/.security-7/_doc/x", Some(json!({"a": 1}))),
            ("DELETE", "/.security-7", None),
            (
                "POST",
                "/_query",
                Some(json!({"query": "FROM .security* | LIMIT 1"})),
            ),
        ] {
            let (status, r) = call(&app, &k, m, uri, body).await;
            assert!(
                status >= 400 || r["hits"]["total"]["value"] == 0,
                "{names} {m} {uri}: {status} {r}"
            );
            assert!(
                !r.to_string().contains("api_key_hash"),
                "{names} {m} {uri}: {r}"
            );
        }
        let hits = hit_indices(&app, &k, "/_all/_search?size=100").await;
        assert!(
            hits.iter().all(|i| !i.starts_with(".security")),
            "{names}: {hits:?}"
        );
    }
}

/// Recorded from Elasticsearch 9.5.5 (fixtures/security_has_privileges.json): what a key holding
/// one named privilege reports for every named privilege es-mini knows.
#[tokio::test]
async fn has_privileges_matches_elasticsearch_for_each_named_privilege() {
    let recorded: Value =
        serde_json::from_str(include_str!("../../fixtures/security_has_privileges.json")).unwrap();
    let app = app().await;
    for (case, expected) in recorded.as_object().unwrap() {
        let (kind, held) = case.split_once(':').unwrap();
        let (roles, body) = match kind {
            "cluster" => (
                json!({"r": {"cluster": [held]}}),
                json!({"cluster": expected["cluster"].as_object().unwrap().keys().collect::<Vec<_>>()}),
            ),
            _ => {
                let asked = expected["index"]["i1"]
                    .as_object()
                    .unwrap()
                    .keys()
                    .collect::<Vec<_>>();
                (
                    json!({"r": {"indices": [{"names": ["i*"], "privileges": [held]}]}}),
                    json!({"index": [{"names": ["i1"], "privileges": asked}]}),
                )
            }
        };
        let (k, _) = key(&app, ELASTIC, roles).await;
        let r = ok(
            &app,
            &k,
            "POST",
            "/_security/user/_has_privileges",
            Some(body),
        )
        .await;
        for field in ["has_all_requested", "cluster", "index"] {
            assert_eq!(r[field], expected[field], "{case}: {field}");
        }
    }
}

/// Recorded from Elasticsearch 9.5.5: a `must_exist` removal fails with 404 if any of its indices
/// lacks the alias, whatever their order, and removes nothing.
#[tokio::test]
async fn must_exist_alias_removal_needs_the_alias_on_every_index() {
    let app = app().await;
    ok(&app, ELASTIC, "PUT", "/a1/_alias/x", None).await;
    for indices in [json!(["a1", "c1"]), json!(["c1", "a1"])] {
        let action = json!({"remove": {"indices": indices, "alias": "x", "must_exist": true}});
        let body = json!({"actions": [action]});
        let (status, r) = call(&app, ELASTIC, "POST", "/_aliases", Some(body)).await;
        assert_eq!(
            (status, &r["error"]["reason"]),
            (404, &json!("aliases [x] missing")),
            "{indices}: {r}"
        );
    }
    let r = ok(&app, ELASTIC, "GET", "/a1/_alias/x", None).await;
    assert_eq!(r["a1"]["aliases"], json!({"x": {}}));
}

/// Recorded from Elasticsearch 9.5.5: date math rounding in histogram bounds rounds down at both
/// ends (`max` is not rounded up to the end of the unit).
#[tokio::test]
async fn histogram_bound_date_math_rounds_down() {
    let app = app().await;
    let mapping = json!({"mappings": {"properties": {"d": {"type": "date"}}}});
    ok(&app, ELASTIC, "PUT", "/h", Some(mapping)).await;
    ok(
        &app,
        ELASTIC,
        "PUT",
        "/h/_doc/1?refresh=true",
        Some(json!({"d": "2016-01-03T10:00:00Z"})),
    )
    .await;
    let agg = |bounds: Value| {
        let mut h = json!({"field": "d", "calendar_interval": "day"});
        h.as_object_mut()
            .unwrap()
            .extend(bounds.as_object().unwrap().clone());
        json!({"size": 0, "aggs": {"a": {"date_histogram": h}}})
    };
    let hard = json!({"hard_bounds": {"min": "2016-01-02||/d", "max": "2016-01-03||/d"}});
    let r = ok(&app, ELASTIC, "POST", "/h/_search", Some(agg(hard))).await;
    assert_eq!(r["aggregations"]["a"]["buckets"], json!([]), "{r}");
    let reversed = json!({"hard_bounds": {"min": "2016-01-03||/d", "max": "2016-01-02||/d"}});
    let (status, r) = call(&app, ELASTIC, "POST", "/h/_search", Some(agg(reversed))).await;
    let reason = "[hard_bounds.min][1451779200000] cannot be greater than [hard_bounds.max][1451692800000] for histogram aggregation [a]";
    assert_eq!(
        (status, &r["error"]["root_cause"][0]["reason"]),
        (400, &json!(reason)),
        "{r}"
    );
    let extended = json!({"extended_bounds": {"min": "2016-01-01||/d", "max": "2016-01-05||/d"}});
    let r = ok(&app, ELASTIC, "POST", "/h/_search", Some(agg(extended))).await;
    assert_eq!(
        r["aggregations"]["a"]["buckets"].as_array().unwrap().len(),
        5,
        "{r}"
    );
}

/// `_cat/aliases` sends `_alias`'s request (Elasticsearch's RestAliasAction), so a key sees the
/// aliases of the indices it may view, and one with no such index privilege is refused alike.
#[tokio::test]
async fn cat_aliases_lists_what_get_aliases_would() {
    let app = app().await;
    let roles =
        json!({"r": {"indices": [{"names": ["a*", "b*"], "privileges": ["view_index_metadata"]}]}});
    let (kv, _) = key(&app, ELASTIC, roles).await;
    let out = ok(&app, &kv, "GET", "/_cat/aliases?format=json", None).await;
    let al = json!({"alias": "al", "index": "b1", "filter": "-", "routing.index": "-", "routing.search": "-", "is_write_index": "-"});
    assert_eq!(out, json!([al]));
    let out = ok(&app, &kv, "GET", "/_cat/aliases/ca?format=json", None).await;
    assert_eq!(out, json!([]));
    let (kr, id) = key(&app, ELASTIC, read_a()).await;
    let expected = denied(
        &id,
        "indices:admin/aliases/get",
        "",
        "view_index_metadata,manage,all",
    );
    assert_denied(&app, &kr, "GET", "/_cat/aliases", expected).await;
}

/// An added alias may not let the key do any action on it that it may not do on the index,
/// compared over every action name the privileges' patterns match, as Elasticsearch compares
/// their automata (`Operations.subsetOf`), not just the actions es-mini knows.
#[tokio::test]
async fn security_authz_gaps_alias_permissions_compare_all_actions() {
    let app = app().await;
    let roles = json!({"r": {"indices": [
        {"names": ["a*"], "privileges": ["manage"]},
        {"names": ["ax*"], "privileges": ["indices:custom/action"]},
        {"names": ["am*"], "privileges": ["monitor", "indices:admin/ilm/explain"]},
    ]}});
    let (k, _) = key(&app, ELASTIC, roles).await;
    let post = |alias: &str| Some(json!({"actions": [{"add": {"index": "a1", "alias": alias}}]}));
    let (status, out) = call(&app, &k, "POST", "/_aliases", post("ax1")).await;
    assert_eq!(status, 403, "{out}");
    assert_eq!(
        out["error"]["reason"],
        "Adding an alias is not allowed when the alias has more permissions than any of the indices"
    );
    ok(&app, &k, "POST", "/_aliases", post("am1")).await;
}
