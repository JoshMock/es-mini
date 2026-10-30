//! Opt-in security (`xpack.security.enabled`): every request authenticates with an API key or
//! as the built-in `elastic` superuser, as Elasticsearch's `AuthenticationService` does.

use crate::error::AppError;
use axum::{
    Extension, Json, Router,
    body::Bytes,
    extract::{Request, State},
    http::{HeaderValue, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use es_index::es_codec::{base64, sha256::sha256_base64};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// The challenges a 401 sends, in Elasticsearch's order.
const CHALLENGES: [&str; 2] = ["Basic realm=\"security\", charset=\"UTF-8\"", "ApiKey"];

pub struct Security {
    /// The hash of `bootstrap.password`, `elastic`'s password (ES's `ELASTIC_PASSWORD`).
    elastic: Option<String>,
    /// API keys by id: name and secret hash.
    // ponytail: in memory until the security-api-keys goal stores them in .security-7
    keys: RwLock<HashMap<String, (String, String)>>,
}

/// Who a request authenticated as.
#[derive(Clone)]
enum Authn {
    Elastic,
    ApiKey { id: String, name: String },
}

/// Security as the node settings `xpack.security.enabled` (default false) and
/// `bootstrap.password` configure it; `None` when disabled.
pub fn from_settings(settings: &HashMap<String, String>) -> Result<Option<Arc<Security>>, String> {
    match settings.get("xpack.security.enabled").map(String::as_str) {
        None | Some("false") => return Ok(None),
        Some("true") => {}
        Some(v) => {
            return Err(format!(
                "Failed to parse value [{v}] as only [true] or [false] are allowed."
            ));
        }
    }
    let elastic = (settings.get("bootstrap.password"))
        .map(|p| hash(p.as_bytes()))
        .transpose()
        .map_err(|e| format!("hashing bootstrap.password: {e}"))?;
    let keys = RwLock::default();
    Ok(Some(Arc::new(Security { elastic, keys })))
}

/// The security REST API.
pub fn routes<S>(security: Arc<Security>) -> Router<S> {
    Router::new()
        .route("/_security/_authenticate", get(authenticate_response))
        .route("/_security/api_key", post(create_key).put(create_key))
        .with_state(security)
}

/// Middleware: answers 401 unless the request authenticates, as Elasticsearch's
/// `AuthenticatorChain` (oauth2 token, API key, then the reserved realm).
pub async fn authenticate(
    State(security): State<Arc<Security>>,
    mut req: Request,
    next: Next,
) -> Response {
    let uri = req.uri().path_and_query().map_or("", |p| p.as_str());
    let auth = req.headers().get(header::AUTHORIZATION);
    match security.authn(auth.map_or(&[][..], HeaderValue::as_bytes), uri) {
        Ok(authn) => {
            req.extensions_mut().insert(authn);
            next.run(req).await
        }
        Err(body) => {
            let mut res = (StatusCode::UNAUTHORIZED, Json(body)).into_response();
            for c in CHALLENGES {
                let c = HeaderValue::from_static(c);
                res.headers_mut().append(header::WWW_AUTHENTICATE, c);
            }
            res
        }
    }
}

impl Security {
    fn authn(&self, auth: &[u8], uri: &str) -> Result<Authn, Value> {
        let scheme = |s: &str| {
            (auth.len() > s.len() && auth[..s.len()].eq_ignore_ascii_case(s.as_bytes()))
                .then(|| auth[s.len()..].trim_ascii())
        };
        let failed = |credentials: String| {
            let reason = "unable to authenticate with provided credentials and anonymous access is not allowed for this request";
            unauthorized(reason, Some(credentials), None)
        };
        if scheme("Bearer ").is_some() {
            return Err(failed("oauth2 token: invalid token".into()));
        }
        if let Some(cred) = scheme("ApiKey ") {
            return self
                .api_key(cred)
                .map_err(|e| failed(format!("API key: {e}")));
        }
        let Some(cred) = scheme("Basic ") else {
            let reason = format!("missing authentication credentials for REST request [{uri}]");
            return Err(unauthorized(&reason, None, None));
        };
        let decoded = base64::decode(cred, base64::STANDARD).map_err(|e| {
            let cause = json!({"type": "illegal_argument_exception", "reason": e});
            let reason = "invalid basic authentication header encoding";
            unauthorized(reason, None, Some(cause))
        })?;
        let decoded = String::from_utf8_lossy(&decoded);
        let Some((user, password)) = decoded.split_once(':') else {
            let reason = "invalid basic authentication header value";
            return Err(unauthorized(reason, None, None));
        };
        if user == "elastic" && (self.elastic.as_ref()).is_some_and(|h| verify(password, h)) {
            return Ok(Authn::Elastic);
        }
        let reason = format!("unable to authenticate user [{user}] for REST request [{uri}]");
        Err(unauthorized(&reason, None, None))
    }

    /// The key that `id:secret`, base64-encoded in `cred`, names, or why it fails.
    fn api_key(&self, cred: &[u8]) -> Result<Authn, String> {
        let decoded =
            String::from_utf8_lossy(&base64::decode(cred, base64::STANDARD)?).into_owned();
        let (id, secret) = match decoded.split_once(':') {
            Some((id, secret)) if !id.is_empty() => (id, secret),
            _ => return Err("invalid ApiKey value".into()),
        };
        let keys = self
            .keys
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some((name, hash)) = keys.get(id) else {
            return Err(format!("unable to find apikey with id {id}"));
        };
        if !verify(secret, hash) {
            return Err(format!("invalid credentials for API key [{id}]"));
        }
        let (id, name) = (id.to_owned(), name.clone());
        Ok(Authn::ApiKey { id, name })
    }
}

/// The body of Elasticsearch's 401 `security_exception`, with its challenges:
/// `credentials` says why the credentials given failed, and `cause` is the `caused_by`.
fn unauthorized(reason: &str, credentials: Option<String>, cause: Option<Value>) -> Value {
    let mut fields = vec![
        ("type", json!("security_exception")),
        ("reason", json!(reason)),
    ];
    fields.extend(credentials.map(|c| ("additional_unsuccessful_credentials", json!(c))));
    let header = ("header", json!({"WWW-Authenticate": CHALLENGES}));
    let object = |f: Vec<(&str, Value)>| {
        Value::Object(f.into_iter().map(|(k, v)| (k.to_owned(), v)).collect())
    };
    let root = object([&fields[..], std::slice::from_ref(&header)].concat());
    let mut error = vec![("root_cause", json!([root]))];
    error.extend(fields);
    error.extend(cause.map(|c| ("caused_by", c)));
    error.push(header);
    json!({"error": object(error), "status": 401})
}

/// `GET /_security/_authenticate`: who the request authenticated as.
async fn authenticate_response(Extension(authn): Extension<Authn>) -> Json<Value> {
    let (roles, realm, kind) = match &authn {
        Authn::Elastic => (json!(["superuser"]), "reserved", "realm"),
        Authn::ApiKey { .. } => (json!([]), "_es_api_key", "api_key"),
    };
    let realm = json!({"name": realm, "type": realm});
    let mut body = json!({
        "username": "elastic",
        "roles": roles,
        "full_name": null,
        "email": null,
        "metadata": {"_reserved": true},
        "enabled": true,
        "authentication_realm": realm,
        "lookup_realm": realm,
        "authentication_type": kind,
    });
    if let Authn::ApiKey { id, name } = authn {
        body["api_key"] = json!({"managed_by": "elasticsearch", "name": name, "id": id});
    }
    Json(body)
}

/// `POST|PUT /_security/api_key`: a new key, whose secret is returned once and stored hashed.
async fn create_key(
    State(security): State<Arc<Security>>,
    body: Bytes,
) -> Result<Json<Value>, AppError> {
    let body = crate::docs::parse_json(&body)?;
    // ponytail: expiration, metadata and role_descriptors arrive with the security-api-keys
    // goal; until then they are refused rather than ignored
    if let Some(k) =
        (body.as_object().into_iter().flatten()).find_map(|(k, _)| (k != "name").then_some(k))
    {
        return Err(AppError::XContentParse(format!(
            "[api_key_request] unknown field [{k}]"
        )));
    }
    let Some(name) = body["name"].as_str() else {
        return Err(AppError::IllegalArgument("Required [name]".into()));
    };
    let io = |e: std::io::Error| AppError::Io(e.to_string());
    let secret = base64::encode(&random::<16>().map_err(io)?, base64::URL, false);
    let hash = hash(secret.as_bytes()).map_err(io)?;
    let (id, _) = es_index::auto_id();
    let mut keys = (security.keys.write()).unwrap_or_else(std::sync::PoisonError::into_inner);
    keys.insert(id.clone(), (name.to_owned(), hash));
    let encoded = base64::encode(format!("{id}:{secret}").as_bytes(), base64::STANDARD, true);
    Ok(Json(
        json!({"id": id, "name": name, "api_key": secret, "encoded": encoded}),
    ))
}

/// Elasticsearch's `SSHA256` hash (its default for API keys): `{SSHA256}`, then base64 of an
/// 8-byte random salt, then base64 of SHA-256 over the secret followed by the salt.
// ponytail: ES hashes the bootstrap password with bcrypt; it never leaves memory, so SSHA256
fn hash(secret: &[u8]) -> std::io::Result<String> {
    Ok(salted(secret, &random::<8>()?))
}

fn salted(secret: &[u8], salt: &[u8]) -> String {
    let salt64 = base64::encode(salt, base64::STANDARD, true);
    format!(
        "{{SSHA256}}{salt64}{}",
        sha256_base64(&[secret, salt].concat())
    )
}

/// Whether `secret` has the `SSHA256` hash `hash`, compared in constant time.
fn verify(secret: &str, hash: &str) -> bool {
    let salt = hash
        .get(9..21)
        .map(|s| base64::decode(s.as_bytes(), base64::STANDARD));
    let Some(Ok(salt)) = salt else { return false };
    let mine = salted(secret.as_bytes(), &salt);
    mine.len() == hash.len()
        && mine
            .bytes()
            .zip(hash.bytes())
            .fold(0, |d, (a, b)| d | (a ^ b))
            == 0
}

fn random<const N: usize>() -> std::io::Result<[u8; N]> {
    let mut out = [0; N];
    std::io::Read::read_exact(&mut std::fs::File::open("/dev/urandom")?, &mut out)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A key and its hash from a live Elasticsearch 9.5.5's `.security-7` index.
    #[test]
    fn verifies_elasticsearch_hashes() {
        let hash = "{SSHA256}Jpbc7W3/Wkk=yNxEER013AMJkhB3d3CaTDDw5Gy0RE04VyIKQJ3wQww=";
        assert!(verify("cm2qKDlKe3OpZrgKZs_eqw", hash));
        assert!(!verify("cm2qKDlKe3OpZrgKZs_eqx", hash));
        let mine = super::hash(b"secret").unwrap();
        assert_eq!((&mine[..9], mine.len()), ("{SSHA256}", hash.len()));
        assert!(verify("secret", &mine) && !verify("secreT", &mine));
        assert_ne!(mine, super::hash(b"secret").unwrap(), "salted");
    }
}
