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
    assert_denied(&app, &km, "GET", "/_cat/shards", expected).await;
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
