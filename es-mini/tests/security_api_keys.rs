//! The API key lifecycle (create, get, query, update, invalidate) with security on. Expected
//! bodies were recorded from a live Elasticsearch 9.5.5 (`xpack.security.enabled=true`).

use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::collections::HashMap;
use tower::ServiceExt;

const ELASTIC: &str = "Basic ZWxhc3RpYzpjaGFuZ2VtZQ=="; // elastic:changeme

fn secured() -> axum::Router {
    let settings = HashMap::from([
        ("xpack.security.enabled".to_owned(), "true".to_owned()),
        ("bootstrap.password".to_owned(), "changeme".to_owned()),
    ]);
    es_mini::app_with_settings(&settings).unwrap()
}

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
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// Creates a key as `auth`, returning the response and its `ApiKey` authorization header.
async fn create(app: &axum::Router, auth: &str, body: Value) -> (Value, String) {
    let (status, key) = call(app, auth, "POST", "/_security/api_key", Some(body)).await;
    assert_eq!(status, 200, "{key}");
    let header = format!("ApiKey {}", key["encoded"].as_str().unwrap());
    (key, header)
}

/// Elasticsearch's error body: `chain` is the error and its `caused_by`s, outermost first, and
/// `root` the index in it of the root cause.
fn error(status: u16, chain: &[(&str, &str)], root: usize) -> Value {
    let mut error = Value::Null;
    for (kind, reason) in chain.iter().rev() {
        let mut e = json!({"type": kind, "reason": reason});
        if !error.is_null() {
            e["caused_by"] = error;
        }
        error = e;
    }
    error["root_cause"] = json!([{"type": chain[root].0, "reason": chain[root].1}]);
    json!({"error": error, "status": status})
}

fn validation(reason: &str) -> Value {
    error(400, &[("action_request_validation_exception", reason)], 0)
}

/// The 401 body for an API key that fails with `why`.
fn unauthorized(why: &str) -> Value {
    let reason = "unable to authenticate with provided credentials and anonymous access is not allowed for this request";
    let header =
        json!({"WWW-Authenticate": ["Basic realm=\"security\", charset=\"UTF-8\"", "ApiKey"]});
    let e = json!({
        "type": "security_exception",
        "reason": reason,
        "additional_unsuccessful_credentials": format!("API key: {why}"),
        "header": header,
    });
    let mut error = e.clone();
    error["root_cause"] = json!([e]);
    json!({"error": error, "status": 401})
}

fn superuser() -> Value {
    json!({"superuser": {
        "cluster": ["all"],
        "indices": [
            {"names": ["*"], "privileges": ["all"], "allow_restricted_indices": false},
            {"names": ["*"], "privileges": ["monitor", "read", "view_index_metadata", "read_cross_cluster"], "allow_restricted_indices": true}
        ],
        "applications": [{"application": "*", "privileges": ["*"], "resources": ["*"]}],
        "run_as": ["*"],
        "metadata": {"_reserved": true},
        "transient_metadata": {"enabled": true},
        "remote_indices": [
            {"names": ["*"], "privileges": ["all"], "allow_restricted_indices": false, "clusters": ["*"]},
            {"names": ["*"], "privileges": ["monitor", "read", "view_index_metadata", "read_cross_cluster"], "allow_restricted_indices": true, "clusters": ["*"]}
        ],
        "remote_cluster": [{"privileges": ["monitor_enrich", "monitor_stats"], "clusters": ["*"]}]
    }})
}

#[tokio::test]
async fn keys_are_created_and_listed() {
    let app = secured();
    let (k1, _) = create(&app, ELASTIC, json!({"name": "k1"})).await;
    let body = json!({
        "name": "k2",
        "expiration": "1d",
        "metadata": {"app": "x", "tags": [1, 2]},
        "role_descriptors": {"r1": {"cluster": ["all"], "indices": [{"names": ["i*"], "privileges": ["read"]}]}}
    });
    let (k2, _) = create(&app, ELASTIC, body).await;
    let fields: Vec<&String> = k2.as_object().unwrap().keys().collect();
    assert_eq!(fields, ["id", "name", "expiration", "api_key", "encoded"]);

    let (status, got) = call(&app, ELASTIC, "GET", "/_security/api_key", None).await;
    assert_eq!(status, 200, "{got}");
    let keys = got["api_keys"].as_array().unwrap();
    assert_eq!(keys.len(), 2);
    let creation = keys[1]["creation"].as_i64().unwrap();
    assert_eq!(k2["expiration"].as_i64().unwrap() - creation, 86_400_000);
    let k1_info = json!({
        "id": k1["id"], "name": "k1", "type": "rest", "creation": keys[0]["creation"],
        "invalidated": false, "username": "elastic", "realm": "reserved", "realm_type": "reserved",
        "metadata": {}, "role_descriptors": {}
    });
    let k2_info = json!({
        "id": k2["id"], "name": "k2", "type": "rest", "creation": creation,
        "expiration": k2["expiration"], "invalidated": false, "username": "elastic",
        "realm": "reserved", "realm_type": "reserved", "metadata": {"app": "x", "tags": [1, 2]},
        "role_descriptors": {"r1": {
            "cluster": ["all"],
            "indices": [{"names": ["i*"], "privileges": ["read"], "allow_restricted_indices": false}],
            "applications": [], "run_as": [], "metadata": {}, "transient_metadata": {"enabled": true}
        }}
    });
    let all = json!({"api_keys": [k1_info.clone(), k2_info.clone()]});
    // Compared as text so field order counts too.
    assert_eq!(got.to_string(), all.to_string());

    let id = k2["id"].as_str().unwrap();
    let mut limited = k2_info.clone();
    limited["limited_by"] = json!([superuser()]);
    let cases = [
        (
            format!("?id={id}"),
            200,
            json!({"api_keys": [k2_info.clone()]}),
        ),
        (
            format!("?id={id}&with_limited_by=true"),
            200,
            json!({"api_keys": [limited]}),
        ),
        (
            "?name=k1".into(),
            200,
            json!({"api_keys": [k1_info.clone()]}),
        ),
        ("?name=k*".into(), 200, all.clone()),
        ("?name=*".into(), 200, all.clone()),
        ("?name=*2".into(), 200, json!({"api_keys": []})),
        ("?id=".into(), 200, all.clone()),
        ("?owner=true".into(), 200, all.clone()),
        (
            "?realm_name=reserved&username=elastic".into(),
            200,
            all.clone(),
        ),
        (
            "?realm_name=_es_api_key".into(),
            200,
            json!({"api_keys": []}),
        ),
        ("?id=nope".into(), 404, json!({"api_keys": []})),
        (
            format!("?id={id}&username=elastic"),
            400,
            validation(
                "Validation Failed: 1: username or realm name must not be specified when the api key id or api key name is specified;",
            ),
        ),
        (
            format!("?id={id}&name=k1"),
            400,
            validation(
                "Validation Failed: 1: only one of [api key id, api key name] can be specified;",
            ),
        ),
        (
            "?bogus=1".into(),
            400,
            error(
                400,
                &[(
                    "illegal_argument_exception",
                    "request [/_security/api_key] contains unrecognized parameter: [bogus]",
                )],
                0,
            ),
        ),
    ];
    for (query, status, expected) in cases {
        let uri = format!("/_security/api_key{query}");
        let got = call(&app, ELASTIC, "GET", &uri, None).await;
        assert_eq!(got.0, status, "{query}: {}", got.1);
        assert_eq!(got.1.to_string(), expected.to_string(), "{query}");
    }
}

#[tokio::test]
async fn invalidated_and_expired_keys_fail_authentication() {
    let app = secured();
    let (k1, auth1) = create(&app, ELASTIC, json!({"name": "k1"})).await;
    let (k2, auth2) = create(&app, ELASTIC, json!({"name": "k2"})).await;
    let (k3, auth3) = create(&app, ELASTIC, json!({"name": "k3", "expiration": "-1d"})).await;
    let id1 = k1["id"].as_str().unwrap();
    assert_eq!(
        call(&app, &auth1, "GET", "/_security/_authenticate", None)
            .await
            .0,
        200
    );
    let got = call(&app, &auth3, "GET", "/_security/_authenticate", None).await;
    assert_eq!(got.1, unauthorized("api key is expired"));

    let invalidate = |body: Value| call(&app, ELASTIC, "DELETE", "/_security/api_key", Some(body));
    let got = invalidate(json!({"ids": [id1, "nope"]})).await;
    let expected = json!({"invalidated_api_keys": [id1], "previously_invalidated_api_keys": [], "error_count": 0});
    assert_eq!((got.0, got.1.to_string()), (200, expected.to_string()));
    let got = invalidate(json!({"ids": [id1]})).await;
    let none = json!({"invalidated_api_keys": [], "previously_invalidated_api_keys": [], "error_count": 0});
    assert_eq!(got.1, none);
    assert_eq!(invalidate(json!({"name": "nothing"})).await.1, none);
    let got = invalidate(json!({"name": "k*"})).await;
    let expected = json!({"invalidated_api_keys": [k2["id"], k3["id"]], "previously_invalidated_api_keys": [], "error_count": 0});
    assert_eq!(got.1, expected);

    let got = call(&app, &auth1, "GET", "/_security/_authenticate", None).await;
    assert_eq!(
        got.1,
        unauthorized(&format!("api key [{id1}] has been invalidated"))
    );
    assert_eq!(call(&app, &auth2, "GET", "/", None).await.0, 401);

    // An invalidation moves a key last, as Elasticsearch's update of its document does.
    let (_, got) = call(&app, ELASTIC, "GET", "/_security/api_key", None).await;
    let keys = got["api_keys"].as_array().unwrap();
    let ids: Vec<&Value> = keys.iter().map(|k| &k["id"]).collect();
    assert_eq!(ids, [&k1["id"], &k2["id"], &k3["id"]]);
    let fields: Vec<&String> = keys[0].as_object().unwrap().keys().collect();
    let expected = [
        "id",
        "name",
        "type",
        "creation",
        "invalidated",
        "invalidation",
        "username",
        "realm",
        "realm_type",
        "metadata",
        "role_descriptors",
    ];
    assert_eq!(fields, expected);
    assert_eq!(keys[0]["invalidated"], true);
    let (_, got) = call(
        &app,
        ELASTIC,
        "GET",
        "/_security/api_key?active_only=true",
        None,
    )
    .await;
    assert_eq!(got, json!({"api_keys": []}));

    let failed_build = |reason: &str| {
        let mut e = error(
            400,
            &[(
                "x_content_parse_exception",
                "Failed to build [invalidate_api_key] after last required field arrived",
            )],
            0,
        );
        e["error"]["caused_by"] =
            json!({"type": "action_request_validation_exception", "reason": reason});
        e
    };
    let cases = [
        (
            json!({}),
            validation(
                "Validation Failed: 1: One of [api key id(s), api key name, username, realm name] must be specified if [owner] flag is false;",
            ),
        ),
        (
            json!({"ids": ["x"], "name": "y"}),
            validation(
                "Validation Failed: 1: only one of [api key id(s), api key name] can be specified;",
            ),
        ),
        (
            json!({"ids": ["x"], "username": "y"}),
            validation(
                "Validation Failed: 1: username or realm name must not be specified when the api key id(s) or api key name are specified;",
            ),
        ),
        (
            json!({"ids": []}),
            failed_build("Validation Failed: 1: Field [ids] cannot be an empty array;"),
        ),
        (
            json!({"ids": ["", "x", ""]}),
            failed_build(
                "Validation Failed: 1: Field [ids] must not contain blank id, but got blank ids at index positions: [0, 2];",
            ),
        ),
    ];
    for (body, expected) in cases {
        let got = invalidate(body.clone()).await;
        assert_eq!((got.0, &got.1), (400, &expected), "{body}");
    }
}

#[tokio::test]
async fn create_requests_are_validated() {
    let app = secured();
    let parse = |field: &str, inner: &[(&str, &str)]| {
        let outer = format!("[api_key_request] failed to parse field [{field}]");
        let mid = "[role_descriptors] failed to parse field [r]";
        let mut chain = vec![
            ("x_content_parse_exception", outer.as_str()),
            ("x_content_parse_exception", mid),
        ];
        chain.extend(inner);
        error(400, &chain, chain.len() - 1)
    };
    let failed_build = |cause: Value| {
        let mut e = error(
            400,
            &[(
                "x_content_parse_exception",
                "Failed to build [api_key_request] after last required field arrived",
            )],
            0,
        );
        e["error"]["caused_by"] = cause;
        e
    };
    let cases = [
        (
            json!({"name": ""}),
            validation("Validation Failed: 1: api key name is required;"),
        ),
        (
            json!({"name": " lead"}),
            validation("Validation Failed: 1: api key name may not begin or end with whitespace;"),
        ),
        (
            json!({"name": "_x"}),
            validation("Validation Failed: 1: api key name may not begin with an underscore;"),
        ),
        (
            json!({"name": "x", "metadata": {"_reserved": 1}}),
            validation("Validation Failed: 1: API key metadata keys may not start with [_];"),
        ),
        (
            json!({"name": "x", "type": "rest"}),
            error(
                400,
                &[(
                    "x_content_parse_exception",
                    "[api_key_request] unknown field [type]",
                )],
                0,
            ),
        ),
        (
            json!({"name": 5}),
            error(
                400,
                &[(
                    "x_content_parse_exception",
                    "[api_key_request] name doesn't support values of type: VALUE_NUMBER",
                )],
                0,
            ),
        ),
        (
            json!({"name": "x", "metadata": "str"}),
            error(
                400,
                &[(
                    "x_content_parse_exception",
                    "[api_key_request] metadata doesn't support values of type: VALUE_STRING",
                )],
                0,
            ),
        ),
        (
            json!({"name": "x", "expiration": 5}),
            error(
                400,
                &[(
                    "x_content_parse_exception",
                    "[api_key_request] expiration doesn't support values of type: VALUE_NUMBER",
                )],
                0,
            ),
        ),
        (
            json!({"name": "x", "expiration": "1"}),
            failed_build(
                json!({"type": "illegal_argument_exception", "reason": "failed to parse setting [expiration] with value [1] as a time value: unit is missing or unrecognized"}),
            ),
        ),
        (
            json!({"name": "x", "expiration": "bogus"}),
            failed_build(
                json!({"type": "illegal_argument_exception", "reason": "failed to parse [bogus]", "caused_by": {"type": "number_format_exception", "reason": "For input string: \"bogu\""}}),
            ),
        ),
        (json!({"name": "x", "role_descriptors": []}), {
            let mut e = error(
                400,
                &[(
                    "x_content_parse_exception",
                    "[api_key_request] failed to parse field [role_descriptors]",
                )],
                0,
            );
            e["error"]["caused_by"] = json!({"type": "illegal_argument_exception", "reason": "[role_descriptors] doesn't support arrays. Use a single object with multiple fields."});
            e
        }),
        (
            json!({"name": "x", "role_descriptors": {"r": {"unknown": 1}}}),
            parse(
                "role_descriptors",
                &[(
                    "parse_exception",
                    "failed to parse role [r]. unexpected field [unknown]",
                )],
            ),
        ),
        (
            json!({"name": "x", "role_descriptors": {"r": {"indices": [{"names": ["a"]}]}}}),
            parse(
                "role_descriptors",
                &[(
                    "parse_exception",
                    "failed to parse indices privileges for role [r]. missing required [privileges] field",
                )],
            ),
        ),
        (
            json!({"name": "x", "role_descriptors": {"r": {"indices": [{"privileges": ["read"]}]}}}),
            parse(
                "role_descriptors",
                &[(
                    "parse_exception",
                    "failed to parse indices privileges for role [r]. missing required [names] field",
                )],
            ),
        ),
        (
            json!({"name": "x", "role_descriptors": {"r": {"indices": [{"names": ["a"], "privileges": ["read"], "bogus": 1}]}}}),
            parse(
                "role_descriptors",
                &[(
                    "parse_exception",
                    "failed to parse indices privileges for role [r]. unexpected field [bogus]",
                )],
            ),
        ),
        (
            json!({"name": "x", "role_descriptors": {"r": {"cluster": "all"}}}),
            parse(
                "role_descriptors",
                &[
                    ("parse_exception", "failed to parse role [r]"),
                    (
                        "parse_exception",
                        "could not parse [cluster] field. expected a string array but found [VALUE_STRING] value instead",
                    ),
                ],
            ),
        ),
        (
            json!({"name": "x", "role_descriptors": {"r": {"cluster": [1]}}}),
            parse(
                "role_descriptors",
                &[
                    ("parse_exception", "failed to parse role [r]"),
                    (
                        "parse_exception",
                        "could not parse [null] field. expected a string array but one of the value in the array is [VALUE_NUMBER]",
                    ),
                ],
            ),
        ),
    ];
    for (body, expected) in cases {
        let got = call(
            &app,
            ELASTIC,
            "POST",
            "/_security/api_key",
            Some(body.clone()),
        )
        .await;
        assert_eq!(
            (got.0, got.1.to_string()),
            (400, expected.to_string()),
            "{body}"
        );
    }

    // Unknown privileges, every one reported.
    let body = json!({"name": "x", "role_descriptors": {"r": {"cluster": ["bogus", "all"], "indices": [{"names": ["a"], "privileges": ["nope"]}]}}});
    let (status, got) = call(&app, ELASTIC, "POST", "/_security/api_key", Some(body)).await;
    assert_eq!(status, 400);
    let reason = got["error"]["reason"].as_str().unwrap();
    assert!(reason.starts_with("Validation Failed: 1: unknown cluster privilege [bogus]. a privilege must be either one of the predefined cluster privilege names [manage_own_api_key,"), "{reason}");
    assert!(reason.contains(",monitor,manage,all] or a pattern over one of the available cluster actions;2: unknown index privilege [nope]. a privilege must be either one of the predefined fixed indices privileges [all,auto_configure,"), "{reason}");
    assert!(
        reason.ends_with(
            ",view_index_metadata,write] or a pattern over one of the available index actions;"
        ),
        "{reason}"
    );

    // Action patterns are privileges too; field and document security are kept as given.
    let body = json!({"name": "x", "role_descriptors": {"r": {
        "cluster": ["monitor", "cluster:admin/xpack/*"],
        "indices": [{"names": "logs", "privileges": ["read", "indices:data/read/*"], "field_security": {"grant": ["f"]}, "query": "{\"match_all\":{}}", "allow_restricted_indices": true}],
        "run_as": ["x"],
        "metadata": {"m": 1}
    }}});
    let (key, _) = create(&app, ELASTIC, body).await;
    let uri = format!("/_security/api_key?id={}", key["id"].as_str().unwrap());
    let (_, got) = call(&app, ELASTIC, "GET", &uri, None).await;
    let expected = json!({"r": {
        "cluster": ["monitor", "cluster:admin/xpack/*"],
        "indices": [{"names": ["logs"], "privileges": ["read", "indices:data/read/*"], "field_security": {"grant": ["f"]}, "query": "{\"match_all\":{}}", "allow_restricted_indices": true}],
        "applications": [], "run_as": ["x"], "metadata": {"m": 1}, "transient_metadata": {"enabled": true}
    }});
    assert_eq!(
        got["api_keys"][0]["role_descriptors"].to_string(),
        expected.to_string()
    );
}

#[tokio::test]
async fn keys_create_only_empty_derived_keys() {
    let app = secured();
    let (_, auth) = create(&app, ELASTIC, json!({"name": "parent"})).await;
    let reason = "creating derived api keys requires an explicit role descriptor that is empty (has no privileges)";
    let expected = error(400, &[("illegal_argument_exception", reason)], 0);
    for body in [
        json!({"name": "d"}),
        json!({"name": "d", "role_descriptors": {"r": {"cluster": ["all"]}}}),
    ] {
        let got = call(&app, &auth, "POST", "/_security/api_key", Some(body)).await;
        assert_eq!(got, (400, expected.clone()));
    }
    let (key, _) = create(
        &app,
        &auth,
        json!({"name": "derived", "role_descriptors": {"e": {}}}),
    )
    .await;
    let uri = format!(
        "/_security/api_key?id={}&with_limited_by=true",
        key["id"].as_str().unwrap()
    );
    let (_, got) = call(&app, ELASTIC, "GET", &uri, None).await;
    let info = &got["api_keys"][0];
    assert_eq!(
        (&info["realm"], &info["realm_type"]),
        (&json!("_es_api_key"), &json!("_es_api_key"))
    );
    let empty = json!({"cluster": [], "indices": [], "applications": [], "run_as": [], "metadata": {}, "transient_metadata": {"enabled": true}});
    assert_eq!(info["role_descriptors"], json!({"e": empty}));
    assert_eq!(info["limited_by"], json!([{}]));
    // The creating key owns it: owner=true lists keys of the key's realm.
    let (_, got) = call(&app, &auth, "GET", "/_security/api_key?owner=true", None).await;
    assert_eq!(got["api_keys"].as_array().unwrap().len(), 1);
}

/// The key `id` as the get API shows it.
async fn get_key(app: &axum::Router, id: &str) -> Value {
    let uri = format!("/_security/api_key?id={id}");
    let (_, got) = call(app, ELASTIC, "GET", &uri, None).await;
    got["api_keys"][0].clone()
}

#[tokio::test]
async fn keys_are_updated() {
    let app = secured();
    let body = json!({"name": "k1", "metadata": {"a": 1}, "role_descriptors": {"r1": {"cluster": ["monitor"]}}});
    let (k1, _) = create(&app, ELASTIC, body).await;
    let id = k1["id"].as_str().unwrap();
    let uri = format!("/_security/api_key/{id}");
    let update = |body: Option<Value>| call(&app, ELASTIC, "PUT", &uri, body);
    let (updated, unchanged) = (
        (200, json!({"updated": true})),
        (200, json!({"updated": false})),
    );
    for body in [None, Some(json!({})), Some(json!({"metadata": {"a": 1}}))] {
        assert_eq!(update(body).await, unchanged);
    }
    assert_eq!(update(Some(json!({"metadata": {"b": 2}}))).await, updated);
    assert_eq!(get_key(&app, id).await["metadata"], json!({"b": 2}));
    assert_eq!(update(Some(json!({"role_descriptors": {}}))).await, updated);
    assert_eq!(get_key(&app, id).await["role_descriptors"], json!({}));

    let roles = json!({"r2": {"indices": [{"names": ["x"], "privileges": ["read"]}]}});
    let body = json!({"role_descriptors": roles, "expiration": "2d"});
    assert_eq!(update(Some(body)).await, updated);
    let info = get_key(&app, id).await;
    let r2 = json!({"cluster": [], "indices": [{"names": ["x"], "privileges": ["read"], "allow_restricted_indices": false}], "applications": [], "run_as": [], "metadata": {}, "transient_metadata": {"enabled": true}});
    assert_eq!(info["role_descriptors"], json!({"r2": r2}));
    let left = info["expiration"].as_i64().unwrap() - info["creation"].as_i64().unwrap();
    assert!(
        (2 * 86_400_000..2 * 86_400_000 + 60_000).contains(&left),
        "{left}"
    );
    // The same roles in another form change nothing; any expiration does.
    let same = json!({"r2": {"indices": [{"names": "x", "privileges": ["read"]}], "run_as": []}});
    assert_eq!(
        update(Some(json!({"role_descriptors": same}))).await,
        unchanged
    );
    assert_eq!(update(Some(json!({"expiration": "2d"}))).await, updated);
}

#[tokio::test]
async fn update_requests_are_validated() {
    let app = secured();
    let (k1, auth) = create(&app, ELASTIC, json!({"name": "k1"})).await;
    let (k2, _) = create(&app, ELASTIC, json!({"name": "k2", "expiration": "-1d"})).await;
    let (k3, _) = create(&app, ELASTIC, json!({"name": "k3"})).await;
    let id = k1["id"].as_str().unwrap();
    let uri = format!("/_security/api_key/{id}");
    let update = |body: Value| call(&app, ELASTIC, "PUT", &uri, Some(body));
    let x_content = |reason: &str| error(400, &[("x_content_parse_exception", reason)], 0);
    let illegal = |reason: &str| error(400, &[("illegal_argument_exception", reason)], 0);
    let cases = [
        (
            json!({"metadata": {"_x": 1}}),
            validation("Validation Failed: 1: API key metadata keys may not start with [_];"),
        ),
        (
            json!({"metadata": "s"}),
            x_content(
                "[update_api_key_request_payload] metadata doesn't support values of type: VALUE_STRING",
            ),
        ),
        (
            json!({"metadata": null}),
            x_content(
                "[update_api_key_request_payload] metadata doesn't support values of type: VALUE_NULL",
            ),
        ),
        (
            json!({"role_descriptors": null}),
            x_content(
                "[update_api_key_request_payload] role_descriptors doesn't support values of type: VALUE_NULL",
            ),
        ),
        (
            json!({"name": "n"}),
            x_content("[update_api_key_request_payload] unknown field [name]"),
        ),
        (
            json!({"expiration": "zz"}),
            error(
                400,
                &[
                    (
                        "x_content_parse_exception",
                        "Failed to build [update_api_key_request_payload] after last required field arrived",
                    ),
                    (
                        "illegal_argument_exception",
                        "failed to parse setting [expiration] with value [zz] as a time value: unit is missing or unrecognized",
                    ),
                ],
                0,
            ),
        ),
    ];
    for (body, expected) in cases {
        assert_eq!(update(body.clone()).await, (400, expected), "{body}");
    }
    let got = call(&app, ELASTIC, "PUT", &format!("{uri}?refresh=true"), None).await;
    let reason = format!("request [{uri}] contains unrecognized parameter: [refresh]");
    assert_eq!(got, (400, illegal(&reason)));

    let got = call(&app, ELASTIC, "PUT", "/_security/api_key/nope", None).await;
    let reason = "no API key owned by requesting user found for ID [nope]";
    assert_eq!(
        got,
        (
            404,
            error(404, &[("resource_not_found_exception", reason)], 0)
        )
    );
    let k2 = k2["id"].as_str().unwrap();
    let got = call(
        &app,
        ELASTIC,
        "PUT",
        &format!("/_security/api_key/{k2}"),
        None,
    )
    .await;
    assert_eq!(
        got,
        (
            400,
            illegal(&format!("cannot update expired API key [{k2}]"))
        )
    );
    let k3 = k3["id"].as_str().unwrap();
    call(
        &app,
        ELASTIC,
        "DELETE",
        "/_security/api_key",
        Some(json!({"ids": [k3]})),
    )
    .await;
    let got = call(
        &app,
        ELASTIC,
        "PUT",
        &format!("/_security/api_key/{k3}"),
        None,
    )
    .await;
    assert_eq!(
        got,
        (
            400,
            illegal(&format!("cannot update invalidated API key [{k3}]"))
        )
    );

    let reason =
        "authentication via API key not supported: only the owner user can update an API key";
    let got = call(&app, &auth, "PUT", &uri, None).await;
    assert_eq!(got, (400, illegal(reason)));
    let body = json!({"ids": [id]});
    let got = call(
        &app,
        &auth,
        "POST",
        "/_security/api_key/_bulk_update",
        Some(body),
    )
    .await;
    assert_eq!(got, (400, illegal(reason)));
}

#[tokio::test]
async fn keys_are_bulk_updated() {
    let app = secured();
    let mut ids = vec![];
    for body in [
        json!({"name": "k1"}),
        json!({"name": "k2", "expiration": "1d"}),
        json!({"name": "k3", "expiration": "-1d"}),
        json!({"name": "k4"}),
    ] {
        ids.push(create(&app, ELASTIC, body).await.0["id"].clone());
    }
    let (a, b, c, d) = (&ids[0], &ids[1], &ids[2], &ids[3]);
    call(
        &app,
        ELASTIC,
        "DELETE",
        "/_security/api_key",
        Some(json!({"ids": [d]})),
    )
    .await;
    let bulk = |body: Value| {
        call(
            &app,
            ELASTIC,
            "POST",
            "/_security/api_key/_bulk_update",
            Some(body),
        )
    };

    let got = bulk(json!({"ids": [a, b, c, d, "nope"], "metadata": {"z": 1}})).await;
    let illegal = |reason: String| json!({"type": "illegal_argument_exception", "reason": reason});
    let details = json!({
        c.as_str().unwrap(): illegal(format!("cannot update expired API key [{}]", c.as_str().unwrap())),
        "nope": {"type": "resource_not_found_exception", "reason": "no API key owned by requesting user found for ID [nope]"},
        d.as_str().unwrap(): illegal(format!("cannot update invalidated API key [{}]", d.as_str().unwrap())),
    });
    let expected =
        json!({"updated": [a, b], "noops": [], "errors": {"count": 3, "details": details}});
    assert_eq!(got, (200, expected));
    assert_eq!(
        get_key(&app, b.as_str().unwrap()).await["metadata"],
        json!({"z": 1})
    );

    let got = bulk(json!({"ids": [a, b], "metadata": {"z": 1}})).await;
    assert_eq!(got, (200, json!({"updated": [], "noops": [a, b]})));
    // Duplicates count once, and a single id may be a string.
    let got = bulk(json!({"ids": [a, a]})).await;
    assert_eq!(got, (200, json!({"updated": [], "noops": [a]})));
    let got = bulk(json!({"ids": a})).await;
    assert_eq!(got, (200, json!({"updated": [], "noops": [a]})));
    let got = bulk(json!({"ids": [b], "role_descriptors": {"q": {}}, "expiration": "3d"})).await;
    assert_eq!(got, (200, json!({"updated": [b], "noops": []})));

    let got = bulk(json!({"ids": []})).await;
    let expected = validation("Validation Failed: 1: Field [ids] cannot be empty;");
    assert_eq!(got, (400, expected));
    let got = bulk(json!({})).await;
    let expected = error(400, &[("illegal_argument_exception", "Required [ids]")], 0);
    assert_eq!(got, (400, expected));
    let got = bulk(json!({"ids": [b], "bogus": 1})).await;
    let reason = "[bulk_update_api_key_request] unknown field [bogus]";
    assert_eq!(
        got,
        (400, error(400, &[("x_content_parse_exception", reason)], 0))
    );
    let got = bulk(json!({"ids": 5})).await;
    let reason = "[bulk_update_api_key_request] ids doesn't support values of type: VALUE_NUMBER";
    assert_eq!(
        got,
        (400, error(400, &[("x_content_parse_exception", reason)], 0))
    );
}

/// Deletes a data directory when dropped.
struct Cleanup(std::path::PathBuf);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn persisted(dir: &std::path::Path) -> axum::Router {
    let settings = HashMap::from([
        ("xpack.security.enabled".to_owned(), "true".to_owned()),
        ("bootstrap.password".to_owned(), "changeme".to_owned()),
    ]);
    es_mini::app_persisted_with_settings(dir, &settings).unwrap()
}

#[tokio::test]
async fn keys_survive_restart_in_a_hidden_index() {
    let dir = std::env::temp_dir().join(format!("esm-api-keys-{}", std::process::id()));
    let _cleanup = Cleanup(dir.clone());
    let _ = std::fs::remove_dir_all(&dir);
    let app = persisted(&dir);
    let (k1, a1) = create(&app, ELASTIC, json!({"name": "k1", "metadata": {"m": 1}})).await;
    let (k2, a2) = create(&app, ELASTIC, json!({"name": "k2"})).await;
    let ids = json!({"ids": [k2["id"]]});
    let got = call(&app, ELASTIC, "DELETE", "/_security/api_key", Some(ids)).await;
    assert_eq!(got.0, 200, "{}", got.1);
    let (_, before) = call(&app, ELASTIC, "GET", "/_security/api_key", None).await;
    assert_eq!(before["api_keys"].as_array().unwrap().len(), 2, "{before}");
    drop(app);

    let app = persisted(&dir);
    let (status, after) = call(&app, ELASTIC, "GET", "/_security/api_key", None).await;
    assert_eq!(status, 200, "{after}");
    assert_eq!(after, before);
    assert_eq!(
        call(&app, &a1, "GET", "/_security/_authenticate", None)
            .await
            .0,
        200
    );
    let why = format!(
        "api key [{}] has been invalidated",
        k2["id"].as_str().unwrap()
    );
    let got = call(&app, &a2, "GET", "/_security/_authenticate", None).await;
    assert_eq!(got, (401, unauthorized(&why)));

    // Searches of every index skip the hidden .security-7; elastic may read it but not write it.
    let id = k1["id"].as_str().unwrap();
    let got = call(&app, ELASTIC, "GET", "/_search", None).await;
    assert_eq!(got.1["hits"]["total"]["value"], 0, "{}", got.1);
    let got = call(&app, ELASTIC, "GET", "/_cat/shards?format=json", None).await;
    assert_eq!(got, (200, json!([])));
    let got = call(
        &app,
        ELASTIC,
        "GET",
        &format!("/.security-7/_doc/{id}"),
        None,
    )
    .await;
    assert_eq!(
        (got.0, &got.1["_source"]["name"]),
        (200, &json!("k1")),
        "{}",
        got.1
    );
    let got = call(&app, ELASTIC, "GET", "/.security/_search", None).await;
    assert_eq!(got.1["hits"]["total"]["value"], 2, "{}", got.1);
    for index in [".security-7", ".security"] {
        let uri = format!("/{index}/_doc/x");
        let got = call(&app, ELASTIC, "PUT", &uri, Some(json!({"a": 1}))).await;
        let reason = format!(
            "action [indices:data/write/bulk[s]] is unauthorized for user [elastic] with effective roles [superuser] on restricted indices [{index}], this action is granted by the index privileges [create_doc,create,delete,index,write,all]"
        );
        assert_eq!(
            got,
            (403, error(403, &[("security_exception", &reason)], 0))
        );
    }
    let got = call(&app, ELASTIC, "PUT", "/.security-7", None).await;
    let reason = "action [indices:admin/create] is unauthorized for user [elastic] with effective roles [superuser] on restricted indices [.security-7], this action is granted by the index privileges [create_index,manage,all]";
    assert_eq!(got, (403, error(403, &[("security_exception", reason)], 0)));
}

/// `POST /_security/_query/api_key{params}` with `body` as `auth`.
async fn query(app: &axum::Router, auth: &str, params: &str, body: Option<Value>) -> (u16, Value) {
    let uri = format!("/_security/_query/api_key{params}");
    call(app, auth, "POST", &uri, body).await
}

/// The keys k1, k2 (expiring), other-3 and Gone (invalidated), in that order, as Elasticsearch
/// 9.5.5's query recordings were made.
async fn query_fixture(app: &axum::Router) -> Vec<Value> {
    let bodies = [
        json!({"name": "k1", "metadata": {"env": "prod", "n": 5}}),
        json!({"name": "k2", "expiration": "1d", "metadata": {"env": "dev", "tags": ["x", "y"]}}),
        json!({"name": "other-3"}),
        json!({"name": "Gone"}),
    ];
    let mut keys = vec![];
    for body in bodies {
        let (key, _) = create(app, ELASTIC, body).await;
        // Distinct creation times, so sorts by creation are stable.
        std::thread::sleep(std::time::Duration::from_millis(2));
        keys.push(key);
    }
    let ids = json!({"ids": [keys[3]["id"]]});
    let got = call(app, ELASTIC, "DELETE", "/_security/api_key", Some(ids)).await;
    assert_eq!(got.0, 200, "{}", got.1);
    let (_, all) = call(app, ELASTIC, "GET", "/_security/api_key", None).await;
    all["api_keys"].as_array().unwrap().clone()
}

#[tokio::test]
async fn keys_are_queried() {
    let app = secured();
    let empty = json!({"total": 0, "count": 0, "api_keys": []});
    assert_eq!(query(&app, ELASTIC, "", None).await, (200, empty.clone()));
    let term = json!({"query": {"term": {"name": "k1"}}});
    assert_eq!(query(&app, ELASTIC, "", Some(term)).await, (200, empty));

    let keys = query_fixture(&app).await;
    let all = json!({"total": 4, "count": 4, "api_keys": keys});
    assert_eq!(query(&app, ELASTIC, "", None).await, (200, all.clone()));
    assert_eq!(query(&app, ELASTIC, "", Some(json!({}))).await, (200, all));
    let got = call(&app, ELASTIC, "GET", "/_security/_query/api_key", None).await;
    assert_eq!(got.1["count"], 4, "{}", got.1);

    let names = |v: &Value| -> Vec<Value> {
        let keys = v["api_keys"].as_array().unwrap();
        keys.iter().map(|k| k["name"].clone()).collect()
    };
    let creation = |i: usize| keys[i]["creation"].clone();
    let max = json!(i64::MAX);
    // (body, total, names, each key's _sort when sorted)
    let cases = [
        (
            json!({"query": {"term": {"name": "k1"}}}),
            1,
            vec!["k1"],
            None,
        ),
        (
            json!({"query": {"prefix": {"name": "k"}}, "sort": ["name"]}),
            2,
            vec!["k1", "k2"],
            Some(json!([["k1"], ["k2"]])),
        ),
        (
            json!({"query": {"term": {"metadata.env": "prod"}}}),
            1,
            vec!["k1"],
            None,
        ),
        (
            json!({"query": {"term": {"metadata.n": 5}}}),
            1,
            vec!["k1"],
            None,
        ),
        (
            json!({"query": {"range": {"expiration": {"gte": "now"}}}}),
            1,
            vec!["k2"],
            None,
        ),
        (
            json!({"query": {"exists": {"field": "expiration"}}}),
            1,
            vec!["k2"],
            None,
        ),
        (
            json!({"query": {"wildcard": {"name": "*-3"}}}),
            1,
            vec!["other-3"],
            None,
        ),
        (
            json!({"query": {"match": {"name": "k2"}}}),
            1,
            vec!["k2"],
            None,
        ),
        (
            json!({"query": {"term": {"name": "gone"}}}),
            0,
            vec![],
            None,
        ),
        (
            json!({"query": {"term": {"name": {"value": "gone", "case_insensitive": true}}}}),
            1,
            vec!["Gone"],
            None,
        ),
        (
            json!({"query": {"terms": {"username": ["elastic"]}}, "sort": ["name"], "search_after": ["k1"]}),
            4,
            vec!["k2", "other-3"],
            Some(json!([["k2"], ["other-3"]])),
        ),
        (
            json!({"query": {"ids": {"values": [keys[0]["id"]]}}}),
            1,
            vec!["k1"],
            None,
        ),
        (
            json!({"query": {"simple_query_string": {"query": "k1"}}}),
            1,
            vec!["k1"],
            None,
        ),
        (
            json!({"query": {"simple_query_string": {"query": "k2", "fields": ["name", "nope"]}}}),
            1,
            vec!["k2"],
            None,
        ),
        (
            json!({"query": {"simple_query_string": {"query": "k1", "fields": ["nope"]}}}),
            0,
            vec![],
            None,
        ),
        (
            json!({"query": {"bool": {"filter": [{"match_all": {}}], "should": [{"term": {"name": "k1"}}, {"term": {"name": "k2"}}], "minimum_should_match": 1}}}),
            2,
            vec!["k1", "k2"],
            None,
        ),
        (
            json!({"query": {"bool": {"must": [{"term": {"invalidated": false}}], "must_not": [{"term": {"name": "k1"}}]}}, "sort": [{"creation": {"order": "desc"}}], "size": 1}),
            2,
            vec!["other-3"],
            Some(json!([[creation(2)]])),
        ),
        (
            json!({"sort": [{"name": {"order": "desc"}}, "creation"]}),
            4,
            vec!["other-3", "k2", "k1", "Gone"],
            Some(json!([
                ["other-3", creation(2)],
                ["k2", creation(1)],
                ["k1", creation(0)],
                ["Gone", creation(3)]
            ])),
        ),
        (
            json!({"sort": ["type", "invalidated", "username", "realm_name", "expiration", "invalidation"]}),
            4,
            vec!["k2", "k1", "other-3", "Gone"],
            Some(json!([
                ["rest", 0, "elastic", "reserved", keys[1]["expiration"], max],
                ["rest", 0, "elastic", "reserved", max, max],
                ["rest", 0, "elastic", "reserved", max, max],
                [
                    "rest",
                    1,
                    "elastic",
                    "reserved",
                    max,
                    keys[3]["invalidation"]
                ],
            ])),
        ),
        (
            json!({"sort": ["metadata.env"]}),
            4,
            vec!["k2", "k1", "other-3", "Gone"],
            Some(json!([["dev"], ["prod"], [null], [null]])),
        ),
        (
            json!({"query": {"term": {"type": "rest"}}, "size": 1}),
            4,
            vec!["k1"],
            None,
        ),
    ];
    for (body, total, expected, sorts) in cases {
        let (status, got) = query(&app, ELASTIC, "", Some(body.clone())).await;
        assert_eq!(status, 200, "{body}: {got}");
        assert_eq!(
            (got["total"].clone(), names(&got)),
            (json!(total), json!(expected).as_array().unwrap().clone()),
            "{body}"
        );
        assert_eq!(got["count"], expected.len(), "{body}");
        let got_sorts: Vec<Value> = (got["api_keys"].as_array().unwrap().iter())
            .map(|k| k["_sort"].clone())
            .collect();
        match sorts {
            Some(sorts) => assert_eq!(json!(got_sorts), sorts, "{body}"),
            None => assert!(got_sorts.iter().all(Value::is_null), "{body}: {got}"),
        }
        // Each key is shown as the get API shows it, then its _sort.
        for k in got["api_keys"].as_array().unwrap() {
            let mut k = k.clone();
            k.as_object_mut().unwrap().remove("_sort");
            assert!(keys.contains(&k), "{body}: {k}");
        }
    }

    let mut limited = keys[0].clone();
    limited["limited_by"] = json!([superuser()]);
    let body = json!({"query": {"term": {"name": "k1"}}});
    let got = query(&app, ELASTIC, "?with_limited_by=true", Some(body)).await;
    let expected = json!({"total": 1, "count": 1, "api_keys": [limited]});
    assert_eq!(got.1.to_string(), expected.to_string());

    // A key with its owner's privileges sees every key too.
    let (_, auth) = create(&app, ELASTIC, json!({"name": "k5"})).await;
    let got = query(&app, &auth, "", None).await;
    assert_eq!(
        (got.0, got.1["total"].clone()),
        (200, json!(5)),
        "{}",
        got.1
    );
}

#[tokio::test]
async fn query_requests_are_validated() {
    let app = secured();
    query_fixture(&app).await;
    let illegal = |reason: &str| error(400, &[("illegal_argument_exception", reason)], 0);
    let x_content = |reason: &str| error(400, &[("x_content_parse_exception", reason)], 0);
    let cases = [
        (
            json!({"query": {"term": {"api_key_hash": "x"}}}),
            illegal("Field [api_key_hash] is not allowed for querying or aggregation"),
        ),
        (
            json!({"query": {"term": {"_id": "x"}}}),
            illegal("Field [_id] is not allowed for querying or aggregation"),
        ),
        (
            json!({"query": {"fuzzy": {"name": "k1"}}}),
            illegal("Query type [fuzzy] is not currently supported in this context"),
        ),
        (
            json!({"query": {"match_none": {}}}),
            illegal("Query type [match_none] is not currently supported in this context"),
        ),
        (
            json!({"query": {"bool": {"must": {"fuzzy": {"name": "k1"}}}}}),
            illegal("Query type [fuzzy] is not currently supported in this context"),
        ),
        (
            json!({"query": {"term": {"na*": "k1"}}}),
            illegal("Field name pattern [na*] is not allowed for querying or aggregation"),
        ),
        (
            json!({"query": {"range": {"creation": {"gte": "now-1d", "relation": "within"}}}}),
            illegal("range query with relation is not currently supported in this context"),
        ),
        (
            json!({"from": -1, "size": -2}),
            validation(
                "Validation Failed: 1: [from] parameter cannot be negative but was [-1];2: [size] parameter cannot be negative but was [-2];",
            ),
        ),
        (
            json!({"sort": ["api_key_hash"]}),
            illegal("Field [api_key_hash] is not allowed for querying or aggregation"),
        ),
        (
            json!({"sort": [{"name": {"nested": {"path": "x"}}}]}),
            illegal("nested sorting is not currently supported in this context"),
        ),
        (
            json!({"sort": "name"}),
            x_content(
                "[query_api_key_request_payload] sort doesn't support values of type: VALUE_STRING",
            ),
        ),
        (
            json!({"sort": [{"creation": "desc"}]}),
            error(
                400,
                &[
                    (
                        "x_content_parse_exception",
                        "[query_api_key_request_payload] failed to parse field [sort]",
                    ),
                    (
                        "x_content_parse_exception",
                        "[field_sort] Expected START_OBJECT but was: VALUE_STRING",
                    ),
                ],
                1,
            ),
        ),
        (
            json!({"foo": 1}),
            x_content("[query_api_key_request_payload] unknown field [foo]"),
        ),
        (
            // ponytail: ES also gives the parsing_exception's line and col
            json!({"query": {"term": {"name": {"nope": 1}}}}),
            error(
                400,
                &[
                    (
                        "x_content_parse_exception",
                        "[query_api_key_request_payload] failed to parse field [query]",
                    ),
                    ("parsing_exception", "[term] query does not support [nope]"),
                ],
                1,
            ),
        ),
    ];
    for (body, expected) in cases {
        let got = query(&app, ELASTIC, "", Some(body.clone())).await;
        assert_eq!(got, (400, expected), "{body}");
    }
    let got = query(&app, ELASTIC, "?foo=1", None).await;
    let reason = "request [/_security/_query/api_key] contains unrecognized parameter: [foo]";
    assert_eq!(got, (400, illegal(reason)));
}

#[tokio::test]
async fn keys_are_aggregated() {
    let app = secured();
    query_fixture(&app).await;
    let terms = |buckets: Value| json!({"doc_count_error_upper_bound": 0, "sum_other_doc_count": 0, "buckets": buckets});
    let one = |k: &str| json!({"key": k, "doc_count": 1});
    // (params, body, aggregations), recorded from Elasticsearch 9.5.5
    // ponytail: aggregations on metadata are left out: es-mini has no flattened type, so they
    // see an unmapped field (ES gives buckets dev and prod for terms on metadata.env)
    let cases = [
        (
            "",
            json!({"size": 0, "aggs": {"n": {"terms": {"field": "name"}}}}),
            json!({"n": terms(json!([one("Gone"), one("k1"), one("k2"), one("other-3")]))}),
        ),
        (
            "?typed_keys=true",
            json!({"size": 0, "aggs": {"n": {"terms": {"field": "username"}, "aggs": {"c": {"cardinality": {"field": "name"}}, "v": {"value_count": {"field": "name"}}}}}}),
            json!({"sterms#n": terms(json!([{"key": "elastic", "doc_count": 4, "cardinality#c": {"value": 4}, "value_count#v": {"value": 4}}]))}),
        ),
        (
            "",
            json!({"size": 0, "aggregations": {"inv": {"terms": {"field": "invalidated"}}, "m": {"missing": {"field": "expiration"}}, "t": {"terms": {"field": "type"}}, "r": {"terms": {"field": "realm_name"}}}}),
            json!({
                "inv": terms(json!([{"key": 0, "key_as_string": "false", "doc_count": 3}, {"key": 1, "key_as_string": "true", "doc_count": 1}])),
                "m": {"doc_count": 3},
                "t": terms(json!([{"key": "rest", "doc_count": 4}])),
                "r": terms(json!([{"key": "reserved", "doc_count": 4}])),
            }),
        ),
        (
            "",
            json!({"size": 0, "aggs": {"rr": {"range": {"field": "creation", "ranges": [{"to": 1}, {"from": 1}]}}}}),
            json!({"rr": {"buckets": [{"key": "*-1", "to": 1.0, "to_as_string": "1", "doc_count": 0}, {"key": "1-*", "from": 1.0, "from_as_string": "1", "doc_count": 4}]}}),
        ),
        (
            "",
            json!({"size": 0, "aggs": {"f": {"filter": {"term": {"name": "k1"}}, "aggs": {"n": {"terms": {"field": "name"}}}}}}),
            json!({"f": {"doc_count": 1, "n": terms(json!([one("k1")]))}}),
        ),
        (
            "",
            json!({"size": 0, "aggs": {"fs": {"filters": {"filters": {"a": {"prefix": {"name": "k"}}, "b": {"term": {"invalidated": true}}}, "other_bucket_key": "rest"}}, "fa": {"filters": {"filters": [{"match": {"name": "k1"}}, {"exists": {"field": "expiration"}}]}}}}),
            json!({
                "fs": {"buckets": {"a": {"doc_count": 2}, "b": {"doc_count": 1}, "rest": {"doc_count": 1}}},
                "fa": {"buckets": [{"doc_count": 1}, {"doc_count": 1}]},
            }),
        ),
        (
            "",
            json!({"size": 0, "aggs": {"x": {"terms": {"field": "name", "missing": "none", "size": 2, "order": {"_key": "asc"}}, "meta": {"a": 1}}}}),
            json!({"x": {"meta": {"a": 1}, "doc_count_error_upper_bound": 0, "sum_other_doc_count": 2, "buckets": [one("Gone"), one("k1")]}}),
        ),
    ];
    for (params, body, aggs) in cases {
        let got = query(&app, ELASTIC, params, Some(body.clone())).await;
        let expected = json!({"total": 4, "count": 0, "api_keys": [], "aggregations": aggs});
        assert_eq!(got, (200, expected), "{body}");
    }
    // Aggregations count the keys the query matches.
    let body =
        json!({"query": {"term": {"name": "k1"}}, "aggs": {"n": {"terms": {"field": "name"}}}});
    let got = query(&app, ELASTIC, "", Some(body)).await;
    assert_eq!(
        (got.1["count"].clone(), got.1["aggregations"].clone()),
        (json!(1), json!({"n": terms(json!([one("k1")]))}))
    );
}

#[tokio::test]
async fn aggregation_requests_are_validated() {
    let app = secured();
    query_fixture(&app).await;
    let illegal = |reason: &str| error(400, &[("illegal_argument_exception", reason)], 0);
    let field = |f: &str| {
        illegal(&format!(
            "Field [{f}] is not allowed for querying or aggregation"
        ))
    };
    let unsupported =
        |name: &str, t: &str| illegal(&format!("Unsupported API Keys agg [{name}] of type [{t}]"));
    let pipeline = |t: &str| illegal(&format!("Unsupported aggregation of type [{t}]"));
    let terms = json!({"terms": {"field": "name"}});
    let cases = [
        (
            json!({"x": {"avg": {"field": "creation"}}}),
            unsupported("x", "avg"),
        ),
        (json!({"x": {"global": {}}}), unsupported("x", "global")),
        (json!({"x": {"terms": {"field": "foo"}}}), field("foo")),
        (
            json!({"x": {"terms": {"field": "api_key_hash"}}}),
            field("api_key_hash"),
        ),
        (
            json!({"x": {"terms": {"field": "na*"}}}),
            illegal("Field name pattern [na*] is not allowed for querying or aggregation"),
        ),
        (
            json!({"x": {"terms": {"script": "doc['name'].value"}}}),
            illegal("Unsupported script value source for [x] agg"),
        ),
        (
            json!({"x": terms, "mb": {"max_bucket": {"buckets_path": "x._count"}}}),
            pipeline("max_bucket"),
        ),
        (
            json!({"x": {"terms": {"field": "name"}, "aggs": {"bs": {"bucket_sort": {"size": 1}}}}}),
            pipeline("bucket_sort"),
        ),
        (
            json!({"x": {"terms": {"field": "name"}, "aggs": {"d": {"derivative": {"buckets_path": "_count"}}}}}),
            pipeline("derivative"),
        ),
        (
            json!({"x": {"terms": {"field": "name"}, "aggs": {"y": {"avg": {"field": "creation"}}}}}),
            unsupported("y", "avg"),
        ),
        // Sub-aggregations are checked first.
        (
            json!({"x": {"global": {}, "aggs": {"y": {"terms": {"field": "foo"}}}}}),
            field("foo"),
        ),
        (
            json!({"x": {"filter": {"term": {"foo": "bar"}}}}),
            field("foo"),
        ),
        (
            // ponytail: recorded with a script query, which es-query cannot parse
            json!({"x": {"filter": {"constant_score": {"filter": {"term": {"name": "k1"}}}}}}),
            illegal("Query type [constant_score] is not currently supported in this context"),
        ),
        (
            json!({"x": terms, "y": {"terms": {"field": "foo"}}}),
            field("foo"),
        ),
    ];
    for (aggs, expected) in cases {
        let body = json!({"aggs": aggs});
        let got = query(&app, ELASTIC, "", Some(body.clone())).await;
        assert_eq!(got, (400, expected), "{body}");
    }

    let body = json!({"aggs": {"x": terms}, "aggregations": {"y": terms}});
    let reason =
        "Failed to build [query_api_key_request_payload] after last required field arrived";
    let chain = [
        ("x_content_parse_exception", reason),
        (
            "illegal_argument_exception",
            "Duplicate 'aggs' or 'aggregations' field",
        ),
    ];
    assert_eq!(
        query(&app, ELASTIC, "", Some(body)).await,
        (400, error(400, &chain, 0))
    );
    // ponytail: ES also gives the line and col
    let body = json!({"aggs": 5});
    let reason =
        "[query_api_key_request_payload] aggs doesn't support values of type: VALUE_NUMBER";
    let got = query(&app, ELASTIC, "", Some(body)).await;
    assert_eq!(
        got,
        (400, error(400, &[("x_content_parse_exception", reason)], 0))
    );
    let body = json!({"aggs": {"x": {"terms": {}}}});
    let chain = [
        (
            "x_content_parse_exception",
            "[query_api_key_request_payload] failed to parse field [aggs]",
        ),
        (
            "illegal_argument_exception",
            "Required one of fields [field, script], but none were specified.",
        ),
    ];
    let got = query(&app, ELASTIC, "", Some(body)).await;
    assert_eq!(got, (400, error(400, &chain, 1)));
}

/// Recorded from Elasticsearch 9.5.5: a key whose only API key privilege is `manage_own_api_key`
/// finds only itself, whatever it queries; `manage_api_key` sees every key.
#[tokio::test]
async fn query_with_manage_own_api_key_finds_only_the_key_itself() {
    let app = secured();
    create(&app, ELASTIC, json!({"name": "other1"})).await;
    let own = json!({"r": {"cluster": ["manage_own_api_key"]}});
    let (mine, auth) = create(
        &app,
        ELASTIC,
        json!({"name": "mine", "role_descriptors": own}),
    )
    .await;
    let all = json!({"r": {"cluster": ["manage_api_key"]}});
    let (_, admin) = create(
        &app,
        ELASTIC,
        json!({"name": "admin", "role_descriptors": all}),
    )
    .await;
    let names = |r: &Value| -> Vec<String> {
        let keys = r["api_keys"].as_array().unwrap_or_else(|| panic!("{r}"));
        keys.iter()
            .map(|k| k["name"].as_str().unwrap().to_owned())
            .collect()
    };
    let (status, r) = call(&app, &auth, "POST", "/_security/_query/api_key", None).await;
    assert_eq!(
        (status, names(&r), &r["total"]),
        (200, vec!["mine".into()], &json!(1)),
        "{r}"
    );
    assert_eq!(r["api_keys"][0]["id"], mine["id"]);
    let other = json!({"query": {"term": {"name": "other1"}}});
    let (_, r) = call(
        &app,
        &auth,
        "POST",
        "/_security/_query/api_key",
        Some(other),
    )
    .await;
    assert_eq!(r, json!({"total": 0, "count": 0, "api_keys": []}));
    let (_, r) = call(&app, &admin, "GET", "/_security/_query/api_key", None).await;
    assert_eq!(r["total"], 3, "{r}");
}

/// simple_query_string over all fields is lenient, as ES turns it on for `*`: date and boolean
/// fields match the values they can hold and skip the others; `lenient` is also taken as given.
/// Results recorded from Elasticsearch 9.5.5 with keys `k1` and `true`.
#[tokio::test]
async fn security_validation_query_all_fields_is_lenient() {
    let app = secured();
    create(&app, ELASTIC, json!({"name": "k1"})).await;
    // Distinct creation times: the first case queries k1's alone.
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    create(&app, ELASTIC, json!({"name": "true"})).await;
    let (_, all) = call(&app, ELASTIC, "GET", "/_security/api_key?name=k1", None).await;
    let creation = all["api_keys"][0]["creation"].as_i64().unwrap();
    let names = |v: &Value| -> Vec<String> {
        let keys = v["api_keys"].as_array().unwrap();
        keys.iter()
            .map(|k| k["name"].as_str().unwrap().to_owned())
            .collect()
    };
    let sqs = |q: Value| json!({"query": {"simple_query_string": q}});
    let cases = [
        (sqs(json!({"query": creation.to_string()})), vec!["k1"]),
        (sqs(json!({"query": "true"})), vec!["true"]),
        (sqs(json!({"query": "false"})), vec!["k1", "true"]),
        (
            sqs(json!({"query": format!("k1 {creation}"), "default_operator": "and"})),
            vec!["k1"],
        ),
        (
            sqs(json!({"query": "k1", "fields": ["name", "creation"], "lenient": true})),
            vec!["k1"],
        ),
    ];
    for (body, expected) in cases {
        let (status, out) = query(&app, ELASTIC, "", Some(body.clone())).await;
        assert_eq!(status, 200, "{body}: {out}");
        let mut got = names(&out);
        got.sort();
        assert_eq!(got, expected, "{body}");
    }
    let strict = sqs(json!({"query": "k1", "fields": ["name", "creation"]}));
    assert_eq!(query(&app, ELASTIC, "", Some(strict)).await.0, 400);
}

/// The query is parsed before it is translated, as ES parses the request body first: an
/// unknown query type is a parsing_exception, while a type ES knows but the API refuses is
/// still refused. Recorded from Elasticsearch 9.5.5.
#[tokio::test]
async fn security_validation_query_is_parsed_before_translation() {
    let app = secured();
    let outer = "[query_api_key_request_payload] failed to parse field [query]";
    let cases = [
        (json!({"bogus": {}}), "unknown query [bogus]"),
        (
            json!({"bool": {"must": [{"term": {"bogusfield": "x"}}, {"bogus": {}}]}}),
            "unknown query [bogus]",
        ),
    ];
    for (q, reason) in cases {
        let (status, out) = query(&app, ELASTIC, "", Some(json!({"query": q}))).await;
        assert_eq!(status, 400, "{q}: {out}");
        assert_eq!(
            out["error"]["type"], "x_content_parse_exception",
            "{q}: {out}"
        );
        assert_eq!(out["error"]["reason"], outer, "{q}: {out}");
        assert_eq!(
            out["error"]["root_cause"][0]["type"], "parsing_exception",
            "{q}: {out}"
        );
        assert_eq!(
            out["error"]["root_cause"][0]["reason"], reason,
            "{q}: {out}"
        );
    }
    let fuzzy = json!({"query": {"bool": {"must": [{"fuzzy": {"name": "k1"}}]}}});
    let (status, out) = query(&app, ELASTIC, "", Some(fuzzy)).await;
    assert_eq!(status, 400, "{out}");
    let reason = "Query type [fuzzy] is not currently supported in this context";
    assert_eq!(out["error"]["reason"], reason, "{out}");
}
