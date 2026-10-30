//! Opt-in security (`xpack.security.enabled`): every request authenticates with an API key or
//! as the built-in `elastic` superuser, as Elasticsearch's `AuthenticationService` does.

use crate::error::AppError;
use axum::{
    Extension, Json, Router,
    body::Bytes,
    extract::{Query, Request, State},
    http::{HeaderValue, StatusCode, Uri, header},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use es_index::es_codec::{base64, sha256::sha256_base64};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

/// The challenges a 401 sends, in Elasticsearch's order.
const CHALLENGES: [&str; 2] = ["Basic realm=\"security\", charset=\"UTF-8\"", "ApiKey"];

pub struct Security {
    /// The hash of `bootstrap.password`, `elastic`'s password (ES's `ELASTIC_PASSWORD`).
    elastic: Option<String>,
    /// API keys as (id, the document Elasticsearch stores in `.security-7`), in index order: a
    /// write moves a key last, as an update moves a Lucene document.
    // ponytail: in memory until keys are stored in .security-7
    keys: Mutex<Vec<(String, Value)>>,
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
    let keys = Mutex::default();
    Ok(Some(Arc::new(Security { elastic, keys })))
}

/// The security REST API.
pub fn routes<S>(security: Arc<Security>) -> Router<S> {
    Router::new()
        .route("/_security/_authenticate", get(authenticate_response))
        .route(
            "/_security/api_key",
            post(create_key)
                .put(create_key)
                .get(get_keys)
                .delete(invalidate_keys),
        )
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
    fn keys(&self) -> MutexGuard<'_, Vec<(String, Value)>> {
        self.keys.lock().unwrap_or_else(PoisonError::into_inner)
    }

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
        let keys = self.keys();
        let Some((_, doc)) = keys.iter().find(|(k, _)| k == id) else {
            return Err(format!("unable to find apikey with id {id}"));
        };
        if doc["api_key_invalidated"] == true {
            return Err(format!("api key [{id}] has been invalidated"));
        }
        if !verify(secret, doc["api_key_hash"].as_str().unwrap_or("")) {
            return Err(format!("invalid credentials for API key [{id}]"));
        }
        if !active(doc, now()) {
            return Err("api key is expired".into());
        }
        let name = doc["name"].as_str().unwrap_or("");
        let (id, name) = (id.to_owned(), name.to_owned());
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
    Extension(authn): Extension<Authn>,
    body: Bytes,
) -> Result<Json<Value>, AppError> {
    let body = crate::docs::parse_json(&body)?;
    let (mut name, mut expiration, mut metadata, mut roles) = (None, None, None, None);
    for (k, v) in body.as_object().into_iter().flatten() {
        let wrong = || {
            let token = es_index::json_token(v);
            AppError::XContentParse(format!(
                "[api_key_request] {k} doesn't support values of type: {token}"
            ))
        };
        match k.as_str() {
            "name" => name = Some(v.as_str().ok_or_else(wrong)?),
            "expiration" => expiration = Some(v.as_str().ok_or_else(wrong)?),
            "metadata" => metadata = Some(v.as_object().ok_or_else(wrong)?),
            "role_descriptors" => roles = Some(role_descriptors(v)?),
            _ => {
                return Err(AppError::XContentParse(format!(
                    "[api_key_request] unknown field [{k}]"
                )));
            }
        }
    }
    let Some(name) = name else {
        return Err(AppError::IllegalArgument("Required [name]".into()));
    };
    let (id, creation) = es_index::auto_id();
    let expiration = expiration
        .map(|e| {
            // Elasticsearch adds whole seconds only.
            let ms = es_index::index_settings::time_millis(e, "expiration")
                .map_err(failed_expiration)?;
            Ok::<_, AppError>(creation + ms / 1000 * 1000)
        })
        .transpose()?;
    let mut errors = vec![];
    errors.extend(name_error(name));
    if metadata.is_some_and(|m| m.keys().any(|k| k.starts_with('_'))) {
        errors.push("API key metadata keys may not start with [_]".into());
    }
    for role in roles.iter().flat_map(Map::values) {
        errors.extend(privilege_errors(role));
    }
    validate(errors)?;
    let (realm, limited_by) = match authn {
        Authn::Elastic => ("reserved", superuser()),
        Authn::ApiKey { .. } => {
            // Elasticsearch's RoleDescriptor.isEmpty
            let empty = |r: &Value| {
                ["cluster", "indices", "applications", "run_as"]
                    .iter()
                    .all(|f| r[f].as_array().is_none_or(Vec::is_empty))
                    && r["metadata"].as_object().is_none_or(Map::is_empty)
            };
            if !roles
                .as_ref()
                .is_some_and(|r| !r.is_empty() && r.values().all(empty))
            {
                return Err(AppError::IllegalArgument("creating derived api keys requires an explicit role descriptor that is empty (has no privileges)".into()));
            }
            ("_es_api_key", json!({}))
        }
    };
    let io = |e: std::io::Error| AppError::Io(e.to_string());
    let secret = base64::encode(&random::<16>().map_err(io)?, base64::URL, false);
    let hash = hash(secret.as_bytes()).map_err(io)?;
    let doc = json!({
        "doc_type": "api_key",
        "type": "rest",
        "creation_time": creation,
        "expiration_time": expiration,
        "api_key_invalidated": false,
        "api_key_hash": hash,
        "role_descriptors": roles.unwrap_or_default(),
        "limited_by_role_descriptors": limited_by,
        "name": name,
        "version": 8150099,
        "metadata_flattened": metadata,
        "creator": {
            "principal": "elastic",
            "metadata": {"_reserved": true},
            "realm_type": realm,
            "full_name": null,
            "realm": realm,
            "email": null,
        },
    });
    security.keys().push((id.clone(), doc));
    let encoded = base64::encode(format!("{id}:{secret}").as_bytes(), base64::STANDARD, true);
    let mut out = json!({"id": id, "name": name});
    if let Some(e) = expiration {
        out["expiration"] = e.into();
    }
    out["api_key"] = secret.into();
    out["encoded"] = encoded.into();
    Ok(Json(out))
}

/// The error for an `expiration` that is not a time value, as Elasticsearch's parser wraps it.
fn failed_expiration(reason: String) -> AppError {
    let mut cause = json!({"type": "illegal_argument_exception", "reason": reason});
    // Java's Long.parseLong failed on the value without its unit.
    if let Some(rest) = reason.strip_prefix("failed to parse [") {
        let v = rest.split(']').next().unwrap_or("").trim().to_lowercase();
        let units = ["nanos", "micros", "ms", "s", "m", "h", "d"];
        let n = units.iter().find_map(|u| v.strip_suffix(u)).unwrap_or(&v);
        let reason = format!("For input string: \"{}\"", n.trim());
        cause["caused_by"] = json!({"type": "number_format_exception", "reason": reason});
    }
    let reason = "Failed to build [api_key_request] after last required field arrived";
    AppError::Caused(Box::new(AppError::XContentParse(reason.into())), cause)
}

/// What is wrong with an API key name, as Elasticsearch's `Validation.ApiKey.validateName`.
fn name_error(name: &str) -> Option<String> {
    Some(match name {
        "" => "api key name is required".into(),
        _ if name.encode_utf16().count() > 256 => {
            "api key name may not be more than 256 characters long".into()
        }
        // Java's String.trim strips every char up to ' '.
        _ if name.trim_matches(|c| c <= ' ') != name => {
            "api key name may not begin or end with whitespace".into()
        }
        _ if name.starts_with('_') => "api key name may not begin with an underscore".into(),
        _ => return None,
    })
}

/// `role_descriptors` parsed into the form Elasticsearch stores in `.security-7`.
fn role_descriptors(v: &Value) -> Result<Map<String, Value>, AppError> {
    let outer = "[api_key_request] failed to parse field [role_descriptors]";
    let roles = v.as_object().ok_or_else(|| {
        let reason =
            "[role_descriptors] doesn't support arrays. Use a single object with multiple fields.";
        let cause = json!({"type": "illegal_argument_exception", "reason": reason});
        AppError::Caused(Box::new(AppError::XContentParse(outer.into())), cause)
    })?;
    roles
        .iter()
        .map(|(name, role)| {
            let parsed = role_descriptor(name, role).map_err(|chain| {
                let mid = format!("[role_descriptors] failed to parse field [{name}]");
                let wrap = [
                    ("x_content_parse_exception", outer.into()),
                    ("x_content_parse_exception", mid),
                ];
                AppError::Chain(wrap.into_iter().chain(chain).collect())
            })?;
            Ok((name.clone(), parsed))
        })
        .collect()
}

/// One role descriptor in `.security-7` form, or the chain of parse errors Elasticsearch
/// reports for it.
// ponytail: applications are kept as given, unvalidated; remote_indices, remote_cluster and
// restriction are refused as unexpected (es-mini has no remote clusters)
fn role_descriptor(name: &str, role: &Value) -> Result<Value, Vec<(&'static str, String)>> {
    let fail = |reason: String| vec![("parse_exception", reason)];
    let array_fail = |reason: String| {
        let mut chain = fail(format!("failed to parse role [{name}]"));
        chain.extend(fail(reason));
        chain
    };
    let mut out = json!({"cluster": [], "indices": [], "applications": [], "run_as": [], "metadata": {}, "type": "role"});
    for (k, v) in role.as_object().into_iter().flatten() {
        match k.as_str() {
            "cluster" | "run_as" => out[k] = strings(k, v).map_err(array_fail)?,
            "applications" => out[k] = v.clone(),
            "metadata" => out[k] = v.clone(),
            "transient_metadata" => {}
            "indices" => {
                let entries = v.as_array().map_or(&[][..], Vec::as_slice);
                let mut indices = vec![];
                for entry in entries {
                    let mut e = Map::new();
                    let mut restricted = false;
                    let what = format!("failed to parse indices privileges for role [{name}].");
                    for (f, v) in entry.as_object().into_iter().flatten() {
                        match f.as_str() {
                            "names" if v.is_string() => {
                                e.insert("names".into(), json!([v]));
                            }
                            "names" | "privileges" => {
                                e.insert(f.clone(), strings(f, v).map_err(array_fail)?);
                            }
                            "field_security" | "query" => {
                                e.insert(f.clone(), v.clone());
                            }
                            "allow_restricted_indices" => restricted = v.as_bool().unwrap_or(false),
                            _ => return Err(fail(format!("{what} unexpected field [{f}]"))),
                        }
                    }
                    for required in ["privileges", "names"] {
                        if !e.contains_key(required) {
                            return Err(fail(format!(
                                "{what} missing required [{required}] field"
                            )));
                        }
                    }
                    // names and privileges lead, in that order.
                    let mut ordered = Map::new();
                    for f in ["names", "privileges", "field_security", "query"] {
                        if let Some(v) = e.remove(f) {
                            ordered.insert(f.into(), v);
                        }
                    }
                    ordered.insert("allow_restricted_indices".into(), restricted.into());
                    indices.push(Value::Object(ordered));
                }
                out[k] = indices.into();
            }
            _ => {
                return Err(fail(format!(
                    "failed to parse role [{name}]. unexpected field [{k}]"
                )));
            }
        }
    }
    Ok(out)
}

/// `v` as a string array, or Elasticsearch's complaint about field `field`.
fn strings(field: &str, v: &Value) -> Result<Value, String> {
    let Some(items) = v.as_array() else {
        let token = es_index::json_token(v);
        return Err(format!(
            "could not parse [{field}] field. expected a string array but found [{token}] value instead"
        ));
    };
    match items.iter().find(|i| !i.is_string()) {
        // Elasticsearch names no field here.
        Some(bad) => Err(format!(
            "could not parse [null] field. expected a string array but one of the value in the array is [{}]",
            es_index::json_token(bad)
        )),
        None => Ok(v.clone()),
    }
}

/// The unknown privileges of a parsed role, as Elasticsearch's `RoleDescriptorRequestValidator`
/// reports them: the first unknown cluster privilege, then one per index entry.
fn privilege_errors(role: &Value) -> Vec<String> {
    let unknown = |v: &Value, known: &[&str], pattern: &str| {
        (v.as_array().into_iter().flatten())
            .filter_map(Value::as_str)
            .find(|p| !known.contains(p) && !p.starts_with(pattern))
            .map(str::to_owned)
    };
    let mut errors = vec![];
    if let Some(p) = unknown(&role["cluster"], CLUSTER_PRIVILEGES, "cluster:") {
        errors.push(format!("unknown cluster privilege [{p}]. a privilege must be either one of the predefined cluster privilege names [{}] or a pattern over one of the available cluster actions", CLUSTER_PRIVILEGES.join(",")));
    }
    for entry in role["indices"].as_array().into_iter().flatten() {
        if let Some(p) = unknown(&entry["privileges"], INDEX_PRIVILEGES, "indices:") {
            errors.push(format!("unknown index privilege [{p}]. a privilege must be either one of the predefined fixed indices privileges [{}] or a pattern over one of the available index actions", INDEX_PRIVILEGES.join(",")));
        }
    }
    errors
}

/// Elasticsearch 9.5.5's cluster privilege names, in the order its errors list them.
const CLUSTER_PRIVILEGES: &[&str] = &[
    "manage_own_api_key",
    "manage_data_stream_global_retention",
    "monitor_data_stream_global_retention",
    "none",
    "cancel_task",
    "clone_api_key",
    "cross_cluster_replication",
    "cross_cluster_search",
    "delegate_pki",
    "grant_api_key",
    "manage_autoscaling",
    "manage_index_templates",
    "manage_logstash_pipelines",
    "manage_oidc",
    "manage_saml",
    "manage_search_application",
    "manage_search_query_rules",
    "manage_search_synonyms",
    "manage_service_account",
    "manage_token",
    "manage_user_profile",
    "monitor_connector",
    "monitor_enrich",
    "monitor_esql",
    "monitor_inference",
    "monitor_ml",
    "monitor_reindex",
    "monitor_rollup",
    "monitor_snapshot",
    "monitor_stats",
    "monitor_text_structure",
    "monitor_watcher",
    "post_behavioral_analytics_event",
    "read_ccr",
    "read_connector_secrets",
    "read_fleet_secrets",
    "read_ilm",
    "read_pipeline",
    "read_project_routing",
    "read_security",
    "read_slm",
    "transport_client",
    "write_connector_secrets",
    "write_fleet_secrets",
    "create_snapshot",
    "manage_behavioral_analytics",
    "manage_ccr",
    "manage_connector",
    "manage_enrich",
    "manage_ilm",
    "manage_inference",
    "manage_ml",
    "manage_project_routing",
    "manage_reindex",
    "manage_rollup",
    "manage_slm",
    "manage_watcher",
    "monitor_data_frame_transforms",
    "monitor_transform",
    "manage_ingest_pipelines",
    "manage_pipeline",
    "manage_api_key",
    "manage_data_frame_transforms",
    "manage_transform",
    "manage_security",
    "monitor",
    "manage",
    "all",
];

/// Elasticsearch 9.5.5's index privilege names, in the order its errors list them.
const INDEX_PRIVILEGES: &[&str] = &[
    "all",
    "auto_configure",
    "create",
    "create_dataset",
    "create_doc",
    "create_index",
    "create_view",
    "cross_cluster_replication",
    "cross_cluster_replication_internal",
    "delete",
    "delete_dataset",
    "delete_index",
    "delete_view",
    "index",
    "maintenance",
    "manage",
    "manage_data_stream_lifecycle",
    "manage_dataset",
    "manage_failure_store",
    "manage_follow_index",
    "manage_ilm",
    "manage_leader_index",
    "manage_view",
    "monitor",
    "none",
    "read",
    "read_cross_cluster",
    "read_dataset_metadata",
    "read_failure_store",
    "read_view_metadata",
    "view_index_metadata",
    "write",
];

/// The `superuser` role, as Elasticsearch stores it as the limit of the keys `elastic` creates.
fn superuser() -> Value {
    let restricted = [
        "monitor",
        "read",
        "view_index_metadata",
        "read_cross_cluster",
    ];
    json!({"superuser": {
        "cluster": ["all"],
        "indices": [
            {"names": ["*"], "privileges": ["all"], "allow_restricted_indices": false},
            {"names": ["*"], "privileges": restricted, "allow_restricted_indices": true},
        ],
        "applications": [{"application": "*", "privileges": ["*"], "resources": ["*"]}],
        "run_as": ["*"],
        "metadata": {"_reserved": true},
        "type": "role",
        "remote_indices": [
            {"names": ["*"], "privileges": ["all"], "allow_restricted_indices": false, "clusters": ["*"]},
            {"names": ["*"], "privileges": restricted, "allow_restricted_indices": true, "clusters": ["*"]},
        ],
        "remote_cluster": [{"privileges": ["monitor_enrich", "monitor_stats"], "clusters": ["*"]}],
    }})
}

/// Which keys a get or invalidate request selects.
#[derive(Default)]
struct Filter<'a> {
    ids: Option<Vec<&'a str>>,
    name: Option<&'a str>,
    username: Option<&'a str>,
    realm: Option<&'a str>,
    active_only: bool,
}

impl Filter<'_> {
    fn matches(&self, id: &str, doc: &Value, now: i64) -> bool {
        let name = doc["name"].as_str().unwrap_or("");
        (self.ids.as_ref()).is_none_or(|ids| ids.contains(&id))
            && self.name.is_none_or(|n| match n.strip_suffix('*') {
                Some(prefix) => name.starts_with(prefix),
                None => name == n,
            })
            && (self.username).is_none_or(|u| doc["creator"]["principal"] == u)
            && self.realm.is_none_or(|r| doc["creator"]["realm"] == r)
            && (!self.active_only || active(doc, now))
    }
}

/// Whether a key is neither invalidated nor expired at `now`.
fn active(doc: &Value, now: i64) -> bool {
    doc["api_key_invalidated"] != true && doc["expiration_time"].as_i64().is_none_or(|e| e > now)
}

/// The realm whose keys `owner=true` selects: the authenticated user's, or for an API key, its
/// owner's.
fn owner_realm(security: &Security, authn: &Authn) -> String {
    match authn {
        Authn::Elastic => "reserved".into(),
        Authn::ApiKey { id, .. } => (security.keys().iter())
            .find(|(k, _)| k == id)
            .and_then(|(_, d)| d["creator"]["realm"].as_str())
            .unwrap_or("")
            .to_owned(),
    }
}

/// `GET /_security/api_key`: the keys the parameters select, in index order.
async fn get_keys(
    State(security): State<Arc<Security>>,
    Extension(authn): Extension<Authn>,
    uri: Uri,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Response, AppError> {
    let allowed = [
        "id",
        "name",
        "username",
        "realm_name",
        "owner",
        "with_limited_by",
        "active_only",
        "with_profile_uid",
    ];
    crate::params::check(&uri, &params, &allowed)?;
    let text = |k: &str| {
        params
            .get(k)
            .map(String::as_str)
            .filter(|v| !v.trim().is_empty())
    };
    let flag = |k: &str| params.get(k).is_some_and(|v| v.is_empty() || v == "true");
    let (id, name, username, realm) = (
        text("id"),
        text("name"),
        text("username"),
        text("realm_name"),
    );
    let mut errors = vec![];
    if (id.is_some() || name.is_some()) && (username.is_some() || realm.is_some()) {
        errors.push("username or realm name must not be specified when the api key id or api key name is specified".into());
    }
    if flag("owner") && (username.is_some() || realm.is_some()) {
        errors.push(
            "neither username nor realm-name may be specified when retrieving owned API keys"
                .into(),
        );
    }
    if id.is_some() && name.is_some() {
        errors.push("only one of [api key id, api key name] can be specified".into());
    }
    validate(errors)?;
    let owner = flag("owner").then(|| owner_realm(&security, &authn));
    let filter = Filter {
        ids: id.map(|i| vec![i]),
        name,
        username: if owner.is_some() {
            Some("elastic")
        } else {
            username
        },
        realm: owner.as_deref().or(realm),
        active_only: flag("active_only"),
    };
    let now = now();
    let keys = security.keys();
    let infos: Vec<Value> = (keys.iter())
        .filter(|(k, d)| filter.matches(k, d, now))
        .map(|(k, d)| info(k, d, flag("with_limited_by")))
        .collect();
    // Elasticsearch answers 404 when the id it was given matches nothing.
    let status = if id.is_some() && infos.is_empty() {
        StatusCode::NOT_FOUND
    } else {
        StatusCode::OK
    };
    Ok((status, Json(json!({"api_keys": infos}))).into_response())
}

/// `errors` as Elasticsearch's action_request_validation_exception, numbered.
fn validate(errors: Vec<String>) -> Result<(), AppError> {
    if errors.is_empty() {
        return Ok(());
    }
    // ActionRequestValidation numbers the first.
    let numbered = (errors.into_iter().enumerate())
        .map(|(i, e)| if i == 0 { e } else { format!("{}: {e}", i + 1) });
    Err(AppError::ActionRequestValidation(
        numbered.collect::<Vec<_>>().join(";"),
    ))
}

/// A key as the get API shows the document `doc`.
fn info(id: &str, doc: &Value, with_limited_by: bool) -> Value {
    let mut out = json!({"id": id, "name": doc["name"], "type": doc["type"], "creation": doc["creation_time"]});
    if !doc["expiration_time"].is_null() {
        out["expiration"] = doc["expiration_time"].clone();
    }
    out["invalidated"] = doc["api_key_invalidated"].clone();
    if !doc["invalidation_time"].is_null() {
        out["invalidation"] = doc["invalidation_time"].clone();
    }
    out["username"] = doc["creator"]["principal"].clone();
    out["realm"] = doc["creator"]["realm"].clone();
    out["realm_type"] = doc["creator"]["realm_type"].clone();
    out["metadata"] = match &doc["metadata_flattened"] {
        Value::Null => json!({}),
        m => m.clone(),
    };
    out["role_descriptors"] = shown(&doc["role_descriptors"]);
    if with_limited_by {
        out["limited_by"] = json!([shown(&doc["limited_by_role_descriptors"])]);
    }
    out
}

/// Stored role descriptors as responses show them: `transient_metadata` in place of `type`.
fn shown(roles: &Value) -> Value {
    let roles = roles.as_object().into_iter().flatten().map(|(name, role)| {
        let role = (role.as_object().into_iter().flatten()).map(|(k, v)| match k.as_str() {
            "type" => ("transient_metadata".to_owned(), json!({"enabled": true})),
            _ => (k.clone(), v.clone()),
        });
        (name.clone(), Value::Object(role.collect()))
    });
    Value::Object(roles.collect())
}

/// `DELETE /_security/api_key`: invalidates the keys the body selects.
async fn invalidate_keys(
    State(security): State<Arc<Security>>,
    Extension(authn): Extension<Authn>,
    body: Bytes,
) -> Result<Json<Value>, AppError> {
    let body = crate::docs::parse_json(&body)?;
    let mut filter = Filter {
        active_only: false,
        ..Filter::default()
    };
    let mut owner = false;
    for (k, v) in body.as_object().into_iter().flatten() {
        let wrong = || {
            let token = es_index::json_token(v);
            AppError::XContentParse(format!(
                "[invalidate_api_key] {k} doesn't support values of type: {token}"
            ))
        };
        let text = || v.as_str().filter(|s| !s.trim().is_empty());
        match k.as_str() {
            "realm_name" => filter.realm = text(),
            "username" => filter.username = text(),
            "name" => filter.name = text(),
            "owner" => owner = v.as_bool().ok_or_else(wrong)?,
            "ids" => {
                let ids: Option<Vec<&str>> = v
                    .as_array()
                    .ok_or_else(wrong)?
                    .iter()
                    .map(Value::as_str)
                    .collect();
                filter.ids = Some(ids.ok_or_else(wrong)?);
            }
            _ => {
                return Err(AppError::XContentParse(format!(
                    "[invalidate_api_key] unknown field [{k}]"
                )));
            }
        }
    }
    if let Some(ids) = &filter.ids {
        let blank: Vec<String> = (ids.iter().enumerate())
            .filter(|(_, i)| i.trim().is_empty())
            .map(|(n, _)| n.to_string())
            .collect();
        let reason = match blank.len() {
            _ if ids.is_empty() => Some("Field [ids] cannot be an empty array".to_owned()),
            0 => None,
            1 => Some(format!(
                "Field [ids] must not contain blank id, but got blank id at index position: [{}]",
                blank[0]
            )),
            _ => Some(format!(
                "Field [ids] must not contain blank id, but got blank ids at index positions: [{}]",
                blank.join(", ")
            )),
        };
        if let Some(r) = reason {
            let cause = json!({"type": "action_request_validation_exception", "reason": format!("Validation Failed: 1: {r};")});
            let reason = "Failed to build [invalidate_api_key] after last required field arrived";
            return Err(AppError::Caused(
                Box::new(AppError::XContentParse(reason.into())),
                cause,
            ));
        }
    }
    let user_or_realm = filter.username.is_some() || filter.realm.is_some();
    let mut errors = vec![];
    if !user_or_realm && filter.ids.is_none() && filter.name.is_none() && !owner {
        errors.push("One of [api key id(s), api key name, username, realm name] must be specified if [owner] flag is false".into());
    }
    if (filter.ids.is_some() || filter.name.is_some()) && user_or_realm {
        errors.push("username or realm name must not be specified when the api key id(s) or api key name are specified".into());
    }
    if owner && user_or_realm {
        errors.push(
            "neither username nor realm-name may be specified when invalidating owned API keys"
                .into(),
        );
    }
    if filter.ids.is_some() && filter.name.is_some() {
        errors.push("only one of [api key id(s), api key name] can be specified".into());
    }
    validate(errors)?;
    let realm = owner.then(|| owner_realm(&security, &authn));
    if realm.is_some() {
        filter.username = Some("elastic");
        filter.realm = realm.as_deref();
    }
    let now = now();
    let mut keys = security.keys();
    // Elasticsearch searches for valid keys only, so none is ever reported as previously
    // invalidated.
    let (mut hit, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut *keys)
        .into_iter()
        .partition(|(k, d)| d["api_key_invalidated"] != true && filter.matches(k, d, now));
    for (_, doc) in &mut hit {
        doc["api_key_invalidated"] = true.into();
        doc["invalidation_time"] = now.into();
    }
    let ids: Vec<String> = hit.iter().map(|(k, _)| k.clone()).collect();
    // Updated documents come last, as in a Lucene index.
    *keys = kept.into_iter().chain(hit).collect();
    Ok(Json(json!({
        "invalidated_api_keys": ids,
        "previously_invalidated_api_keys": [],
        "error_count": 0,
    })))
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
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
