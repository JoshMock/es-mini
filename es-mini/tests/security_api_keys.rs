//! The API key lifecycle (create, get, invalidate) with security on. Expected bodies were
//! recorded from a live Elasticsearch 9.5.5 (`xpack.security.enabled=true`).

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
