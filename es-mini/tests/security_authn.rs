//! Opt-in security (`xpack.security.enabled`): every request authenticates with an API key or
//! as the built-in `elastic` user, and failures get Elasticsearch 9.5.5's 401 responses
//! (recorded from a live node).

use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::collections::HashMap;
use tower::ServiceExt;

const BASIC: &str = "Basic realm=\"security\", charset=\"UTF-8\"";

fn secured() -> axum::Router {
    let settings = HashMap::from([
        ("xpack.security.enabled".to_owned(), "true".to_owned()),
        ("bootstrap.password".to_owned(), "changeme".to_owned()),
    ]);
    es_mini::app_with_settings(&settings).unwrap()
}

fn basic(user: &str, password: &str) -> String {
    format!("Basic {}", b64(&format!("{user}:{password}")))
}

fn b64(s: &str) -> String {
    use es_index::es_codec::base64;
    base64::encode(s.as_bytes(), base64::STANDARD, true)
}

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    auth: Option<&str>,
    body: Option<Value>,
) -> (u16, Vec<String>, Value) {
    let mut req = Request::builder().method(method).uri(uri);
    if let Some(a) = auth {
        req = req.header("authorization", a);
    }
    let body = body.map_or(Body::empty(), |b| Body::from(b.to_string()));
    let req = req.header("content-type", "application/json").body(body);
    let res = app.clone().oneshot(req.unwrap()).await.unwrap();
    let status = res.status().as_u16();
    let www = (res.headers().get_all("www-authenticate").iter())
        .map(|v| v.to_str().unwrap().to_owned())
        .collect();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, www, body)
}

/// Elasticsearch's 401: `reason`, then the `extra` fields, then the challenge headers.
fn unauthorized(reason: &str, extra: Value, cause: Option<Value>) -> Value {
    let header = json!({"WWW-Authenticate": [BASIC, "ApiKey"]});
    let mut root = json!({"type": "security_exception", "reason": reason});
    root.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    let mut error = root.clone();
    root["header"] = header.clone();
    if let Some(c) = cause {
        error["caused_by"] = c;
    }
    error["header"] = header;
    let mut out = json!({"error": {"root_cause": [root]}, "status": 401});
    out["error"]
        .as_object_mut()
        .unwrap()
        .extend(error.as_object().unwrap().clone());
    out
}

async fn assert_401(app: &axum::Router, uri: &str, auth: Option<&str>, expected: Value) {
    let (status, www, body) = call(app, "GET", uri, auth, None).await;
    assert_eq!(status, 401, "{auth:?}: {body}");
    assert_eq!(www, [BASIC, "ApiKey"], "{auth:?}");
    // Compared as text so field order counts too.
    assert_eq!(body.to_string(), expected.to_string(), "{auth:?}");
}

#[tokio::test]
async fn security_is_off_by_default() {
    for app in [
        es_mini::app(),
        es_mini::app_with_settings(&HashMap::from([(
            "xpack.security.enabled".to_owned(),
            "false".to_owned(),
        )]))
        .unwrap(),
    ] {
        assert_eq!(call(&app, "GET", "/", None, None).await.0, 200);
        let (status, _, body) = call(&app, "GET", "/_security/_authenticate", None, None).await;
        assert_eq!(status, 400);
        let error = "no handler found for uri [/_security/_authenticate] and method [GET]";
        assert_eq!(body, json!({"error": error}));
    }
}

#[test]
fn rejects_a_non_boolean_enabled_setting() {
    let settings = HashMap::from([("xpack.security.enabled".to_owned(), "yes".to_owned())]);
    let err = es_mini::app_with_settings(&settings).unwrap_err();
    assert_eq!(
        err,
        "Failed to parse value [yes] as only [true] or [false] are allowed."
    );
}

#[tokio::test]
async fn requests_without_valid_credentials_get_401() {
    let app = secured();
    let missing = |uri: &str| {
        let reason = format!("missing authentication credentials for REST request [{uri}]");
        unauthorized(&reason, json!({}), None)
    };
    assert_401(&app, "/", None, missing("/")).await;
    assert_401(&app, "/foo/_search?q=x", None, missing("/foo/_search?q=x")).await;
    assert_401(&app, "/nope/_nope", None, missing("/nope/_nope")).await;
    assert_401(&app, "/", Some("Foo abc"), missing("/")).await;
    for user in ["elastic", "bob"] {
        let reason = format!("unable to authenticate user [{user}] for REST request [/]");
        let expected = unauthorized(&reason, json!({}), None);
        assert_401(&app, "/", Some(&basic(user, "wrong")), expected).await;
    }
    let expected = unauthorized("invalid basic authentication header value", json!({}), None);
    assert_401(
        &app,
        "/",
        Some(&format!("Basic {}", b64("elastic"))),
        expected,
    )
    .await;
    let cause =
        json!({"type": "illegal_argument_exception", "reason": "Illegal base64 character 21"});
    let expected = unauthorized(
        "invalid basic authentication header encoding",
        json!({}),
        Some(cause),
    );
    assert_401(&app, "/", Some("Basic !!!"), expected).await;

    let failed = |credentials: &str| {
        let reason = "unable to authenticate with provided credentials and anonymous access is not allowed for this request";
        unauthorized(
            reason,
            json!({"additional_unsuccessful_credentials": credentials}),
            None,
        )
    };
    let cases = [
        (
            format!("ApiKey {}", b64("foo:bar")),
            "API key: unable to find apikey with id foo",
        ),
        ("ApiKey !!!".into(), "API key: Illegal base64 character 21"),
        (
            format!("ApiKey {}", b64("foobar")),
            "API key: invalid ApiKey value",
        ),
        ("Bearer abc".into(), "oauth2 token: invalid token"),
    ];
    for (auth, credentials) in cases {
        assert_401(&app, "/", Some(&auth), failed(credentials)).await;
    }
}

#[tokio::test]
async fn elastic_authenticates_with_the_bootstrap_password() {
    let app = secured();
    let auth = basic("elastic", "changeme");
    let (status, _, body) = call(&app, "GET", "/", Some(&auth), None).await;
    assert_eq!(status, 200, "{body}");
    let (status, _, body) = call(&app, "GET", "/_security/_authenticate", Some(&auth), None).await;
    assert_eq!(status, 200);
    let expected = json!({
        "username": "elastic",
        "roles": ["superuser"],
        "full_name": null,
        "email": null,
        "metadata": {"_reserved": true},
        "enabled": true,
        "authentication_realm": {"name": "reserved", "type": "reserved"},
        "lookup_realm": {"name": "reserved", "type": "reserved"},
        "authentication_type": "realm"
    });
    assert_eq!(body.to_string(), expected.to_string());
    let (status, _, body) = call(&app, "POST", "/_security/_authenticate", Some(&auth), None).await;
    assert_eq!(status, 405, "{body}");

    // Without a bootstrap password, elastic cannot log in.
    let settings = HashMap::from([("xpack.security.enabled".to_owned(), "true".to_owned())]);
    let app = es_mini::app_with_settings(&settings).unwrap();
    let reason = "unable to authenticate user [elastic] for REST request [/]";
    assert_401(
        &app,
        "/",
        Some(&basic("elastic", "")),
        unauthorized(reason, json!({}), None),
    )
    .await;
}

#[tokio::test]
async fn api_keys_authenticate() {
    let app = secured();
    let elastic = basic("elastic", "changeme");
    let (status, _, key) = call(
        &app,
        "POST",
        "/_security/api_key",
        Some(&elastic),
        Some(json!({"name": "k1"})),
    )
    .await;
    assert_eq!(status, 200, "{key}");
    let fields: Vec<&String> = key.as_object().unwrap().keys().collect();
    assert_eq!(fields, ["id", "name", "api_key", "encoded"]);
    let (id, secret) = (
        key["id"].as_str().unwrap(),
        key["api_key"].as_str().unwrap(),
    );
    assert_eq!((id.len(), secret.len()), (20, 22));
    assert_eq!(key["encoded"], json!(b64(&format!("{id}:{secret}"))));

    let auth = format!("ApiKey {}", key["encoded"].as_str().unwrap());
    let (status, _, body) = call(&app, "GET", "/_security/_authenticate", Some(&auth), None).await;
    assert_eq!(status, 200, "{body}");
    let expected = json!({
        "username": "elastic",
        "roles": [],
        "full_name": null,
        "email": null,
        "metadata": {"_reserved": true},
        "enabled": true,
        "authentication_realm": {"name": "_es_api_key", "type": "_es_api_key"},
        "lookup_realm": {"name": "_es_api_key", "type": "_es_api_key"},
        "authentication_type": "api_key",
        "api_key": {"managed_by": "elasticsearch", "name": "k1", "id": id}
    });
    assert_eq!(body.to_string(), expected.to_string());

    // PUT creates too, and a key can create keys.
    let (status, _, other) = call(
        &app,
        "PUT",
        "/_security/api_key",
        Some(&auth),
        Some(json!({"name": "k2"})),
    )
    .await;
    assert_eq!(status, 200, "{other}");
    assert_ne!(other["id"], key["id"]);

    let wrong = format!("ApiKey {}", b64(&format!("{id}:wrong")));
    let reason = "unable to authenticate with provided credentials and anonymous access is not allowed for this request";
    let credentials = format!("API key: invalid credentials for API key [{id}]");
    let extra = json!({"additional_unsuccessful_credentials": credentials});
    assert_401(&app, "/", Some(&wrong), unauthorized(reason, extra, None)).await;
}

#[tokio::test]
async fn api_key_requests_are_validated() {
    let app = secured();
    let elastic = basic("elastic", "changeme");
    let cases = [
        (
            Some(json!({})),
            json!({"type": "illegal_argument_exception", "reason": "Required [name]"}),
        ),
        (
            None,
            json!({"type": "parse_exception", "reason": "request body is required"}),
        ),
    ];
    for (body, error) in cases {
        let (status, _, got) = call(&app, "POST", "/_security/api_key", Some(&elastic), body).await;
        assert_eq!(status, 400, "{got}");
        let mut expected = json!({"error": {"root_cause": [error.clone()]}, "status": 400});
        expected["error"]
            .as_object_mut()
            .unwrap()
            .extend(error.as_object().unwrap().clone());
        assert_eq!(got, expected);
    }
    // ponytail: the other create fields (expiration, metadata, role_descriptors) arrive with the
    // security-api-keys goal; until then they are refused, not ignored.
    let body = Some(json!({"name": "k", "role_descriptors": {}}));
    let (status, _, got) = call(&app, "POST", "/_security/api_key", Some(&elastic), body).await;
    assert_eq!(status, 400, "{got}");
    assert_eq!(got["error"]["type"], "x_content_parse_exception");
}

#[tokio::test]
async fn persisted_servers_enforce_security() {
    let dir = std::env::temp_dir().join(format!("esm-security-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let settings = HashMap::from([("xpack.security.enabled".to_owned(), "true".to_owned())]);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn({
        let dir = dir.clone();
        async move {
            es_mini::serve_persisted(listener, &dir, &settings, async {
                let _ = stopped.await;
            })
            .await
        }
    });
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let req = "PUT /books HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    tokio::io::AsyncWriteExt::write_all(&mut stream, req.as_bytes())
        .await
        .unwrap();
    let mut resp = String::new();
    tokio::io::AsyncReadExt::read_to_string(&mut stream, &mut resp)
        .await
        .unwrap();
    assert!(resp.starts_with("HTTP/1.1 401"), "{resp}");
    stop.send(()).unwrap();
    server.await.unwrap().unwrap();
    assert!(!dir.join("indices").exists());
    let _ = std::fs::remove_dir_all(&dir);
}
