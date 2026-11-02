//! Authorization of API keys, as Elasticsearch's `RBACEngine` does it: each REST request is the
//! action Elasticsearch names it, granted by cluster privileges, or by index privileges on the
//! names it targets, where wildcards expand to the indices and aliases the key may use only.

use super::{Authn, Security};
use crate::error::AppError;
use crate::store::{Shared, Store};
use axum::{
    Extension, Json,
    body::Bytes,
    extract::{Query, Request, State},
    http::{Method, StatusCode, Uri},
    middleware::Next,
    response::{IntoResponse, Response},
};
use es_index::aliases::{IndexMeta, IndicesOptions, glob};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

/// The privileges that grant each action es-mini authorizes, in the order Elasticsearch's
/// denials list them (recorded from Elasticsearch 9.5.5).
const GRANTED_BY: &[(&str, &str)] = &[
    ("cluster:monitor/main", "monitor,manage,all"),
    ("cluster:monitor/health", "monitor,manage,all"),
    (
        "cluster:monitor/stats",
        "cross_cluster_search,monitor_stats,monitor,manage,all",
    ),
    ("cluster:monitor/settings", "monitor,manage,all"),
    ("cluster:monitor/nodes/info", "monitor,manage,all"),
    ("cluster:monitor/nodes/stats", "monitor,manage,all"),
    (
        "cluster:monitor/state",
        "cross_cluster_replication,read_ccr,transport_client,manage_ccr,monitor,manage,all",
    ),
    (
        "cluster:admin/xpack/security/api_key/create",
        "manage_own_api_key,manage_api_key,manage_security,all",
    ),
    (
        "cluster:admin/xpack/security/api_key/get",
        "read_security,manage_api_key,manage_security,all",
    ),
    (
        "cluster:admin/xpack/security/api_key/query",
        "manage_own_api_key,read_security,manage_api_key,manage_security,all",
    ),
    (
        "cluster:admin/xpack/security/api_key/invalidate",
        "manage_api_key,manage_security,all",
    ),
    (
        "cluster:admin/xpack/security/api_key/update",
        "manage_own_api_key,manage_api_key,manage_security,all",
    ),
    (
        "cluster:admin/xpack/security/api_key/bulk_update",
        "manage_own_api_key,manage_api_key,manage_security,all",
    ),
    ("indices:data/read/search", "read,all"),
    ("indices:data/read/get", "read,all"),
    (
        "indices:data/read/field_caps",
        "view_index_metadata,manage,read,all",
    ),
    ("indices:data/read/open_point_in_time", "read,all"),
    ("indices:data/read/scroll", "read,all"),
    ("indices:data/read/search/template", "read,all"),
    ("indices:data/read/msearch", "read,all"),
    ("indices:data/read/msearch/template", "read,all"),
    ("indices:data/read/mget", "read,all"),
    ("indices:data/read/mget[shard]", "read,all"),
    ("indices:data/read/esql", "read_cross_cluster,read,all"),
    ("indices:data/read/esql/search_shards", "read,all"),
    (
        "indices:data/write/bulk",
        "create_doc,create,delete,index,write,all",
    ),
    (
        "indices:data/write/bulk[s]",
        "create_doc,create,delete,index,write,all",
    ),
    (
        "indices:data/write/index",
        "create_doc,create,index,write,all",
    ),
    // The actions of bulk items, which no denial names.
    (
        "indices:data/write/index:op_type/index",
        "create,index,write,all",
    ),
    (
        "indices:data/write/index:op_type/create",
        "create_doc,create,index,write,all",
    ),
    ("indices:data/write/delete", "delete,write,all"),
    ("indices:data/write/update", "index,write,all"),
    ("indices:data/write/update/byquery", "index,write,all"),
    ("indices:data/write/delete/byquery", "delete,write,all"),
    (
        "indices:admin/auto_create",
        "auto_configure,create_index,manage,all",
    ),
    ("indices:admin/create", "create_index,manage,all"),
    ("indices:admin/get", "view_index_metadata,manage,all"),
    ("indices:admin/delete", "delete_index,manage,all"),
    (
        "indices:admin/mappings/get",
        "view_index_metadata,manage,all",
    ),
    ("indices:admin/mapping/put", "manage,all"),
    (
        "indices:monitor/settings/get",
        "monitor,view_index_metadata,manage,all",
    ),
    ("indices:admin/settings/update", "manage,all"),
    (
        "indices:admin/aliases/get",
        "view_index_metadata,manage,all",
    ),
    ("indices:admin/aliases", "manage,all"),
    (
        "indices:monitor/stats",
        "monitor,cross_cluster_replication,manage,all",
    ),
    ("indices:admin/refresh", "maintenance,manage,all"),
    ("indices:admin/flush", "maintenance,manage,all"),
    ("indices:admin/close", "manage_follow_index,manage,all"),
    ("indices:admin/open", "manage,all"),
];

const QUERY_KEYS: &str = "cluster:admin/xpack/security/api_key/query";

fn granted_by(action: &str) -> &'static str {
    (GRANTED_BY.iter())
        .find(|(a, _)| *a == action)
        .map_or("all", |(_, p)| p)
}

/// Whether the privileges `privileges` (names, or patterns over action names) grant `action`.
fn grants(privileges: &Value, action: &str) -> bool {
    let named = granted_by(action);
    (privileges.as_array().into_iter().flatten())
        .filter_map(Value::as_str)
        .any(|p| named.split(',').any(|n| n == p) || (p.contains(':') && glob(p, action)))
}

/// Whether API key `roles` may query every key, not just itself: Elasticsearch narrows a
/// query that only `manage_own_api_key` grants to the key's own id.
pub(super) fn queries_all_keys(roles: &[Value]) -> bool {
    let wider = |r: &Value| {
        let p = r["cluster"].as_array().into_iter().flatten();
        let p: Vec<&Value> = p.filter(|p| *p != "manage_own_api_key").collect();
        grants(&json!(p), QUERY_KEYS)
    };
    roles.iter().all(|set| self::roles(set).any(wider))
}

/// The role descriptors an API key is authorized by: its own (if it has any), then those it is
/// limited by (its creator's), each of which must grant what the key does.
pub(super) fn role_sets(doc: &Value) -> Vec<Value> {
    let own = &doc["role_descriptors"];
    let limited_by = doc["limited_by_role_descriptors"].clone();
    match own.as_object() {
        Some(o) if !o.is_empty() => vec![own.clone(), limited_by],
        _ => vec![limited_by],
    }
}

/// An authenticated API key.
struct Key<'a> {
    id: &'a str,
    roles: &'a [Value],
}

/// Where in the path an index expression is, to put its expansion there.
#[derive(Clone, Copy)]
enum Place {
    Segment(usize),
    /// The request names no index, so it targets every one; the expansion goes first.
    Prefix,
    /// As [`Place::Prefix`], with the expansion going last.
    Suffix,
}

/// One check of a request, in the order Elasticsearch makes them.
enum Step<'a> {
    Cluster(&'static str),
    /// An index action that some index privilege must grant, whatever the index; `names`, the
    /// indices the request names, only go in the denial.
    AnyIndex(&'static str, &'a str),
    /// An index action on the expression at `at`; `expand` expands its wildcards to authorized
    /// names only (otherwise every part is a literal name).
    Indices {
        action: &'static str,
        at: Place,
        expand: bool,
    },
    /// A document write to `index` (single-item bulks in Elasticsearch): `action` on any index,
    /// auto-creation if `index` is missing, then the bulk item's `item` action on `index`.
    Write {
        action: &'static str,
        item: &'static str,
        index: &'a str,
    },
    /// The alias names of an alias change, which need `indices:admin/aliases` too.
    AliasNames(&'a str),
}

/// The checks Elasticsearch makes for `method` on the path `segs`, by the actions it names
/// them.
// (Bulk, mget, msearch, ES|QL and POST _aliases are checked per item by their handlers.)
fn steps<'a>(method: &Method, segs: &[&'a str], params: &HashMap<String, String>) -> Vec<Step<'a>> {
    use Step::*;
    let at = |action, i| Indices {
        action,
        at: Place::Segment(i),
        expand: true,
    };
    let every = |action| Indices {
        action,
        at: Place::Prefix,
        expand: true,
    };
    let literal = |action| Indices {
        action,
        at: Place::Segment(0),
        expand: false,
    };
    let get = *method == Method::GET || *method == Method::HEAD;
    let m = if get { "GET" } else { method.as_str() };
    match (m, segs) {
        (_, ["_security", "_authenticate"] | ["_security", "user", "_has_privileges"])
        | ("DELETE", ["_pit"] | ["_search", "scroll", ..]) => vec![],
        ("GET", []) => vec![Cluster("cluster:monitor/main")],
        (_, ["_cluster", "health", ..]) => vec![Cluster("cluster:monitor/health")],
        (_, ["_cluster", "stats"]) => vec![Cluster("cluster:monitor/stats")],
        (_, ["_cluster", "settings"]) => vec![Cluster("cluster:monitor/settings")],
        (_, ["_nodes", "stats"]) => vec![Cluster("cluster:monitor/nodes/stats")],
        (_, ["_nodes", ..]) => vec![Cluster("cluster:monitor/nodes/info")],
        ("GET", ["_security", "api_key"]) => {
            vec![Cluster("cluster:admin/xpack/security/api_key/get")]
        }
        ("DELETE", ["_security", "api_key"]) => {
            vec![Cluster("cluster:admin/xpack/security/api_key/invalidate")]
        }
        (_, ["_security", "api_key"]) => {
            vec![Cluster("cluster:admin/xpack/security/api_key/create")]
        }
        (_, ["_security", "api_key", "_bulk_update"]) => {
            vec![Cluster("cluster:admin/xpack/security/api_key/bulk_update")]
        }
        (_, ["_security", "api_key", _]) => {
            vec![Cluster("cluster:admin/xpack/security/api_key/update")]
        }
        (_, ["_security", "_query", "api_key"]) => vec![Cluster(QUERY_KEYS)],
        ("PUT", ["_settings"]) => vec![every("indices:admin/settings/update")],
        (_, ["_settings", ..]) => vec![every("indices:monitor/settings/get")],
        ("POST", ["_aliases"]) => vec![],
        (_, ["_alias" | "_aliases", ..]) => vec![every("indices:admin/aliases/get")],
        (_, ["_refresh"]) => vec![every("indices:admin/refresh")],
        (_, ["_flush"]) => vec![every("indices:admin/flush")],
        (_, ["_mapping"]) => vec![every("indices:admin/mappings/get")],
        (_, ["_stats", ..]) => vec![every("indices:monitor/stats")],
        (_, ["_cat", "shards"]) => vec![
            Indices {
                action: "indices:monitor/stats",
                at: Place::Suffix,
                expand: true,
            },
            Cluster("cluster:monitor/state"),
        ],
        (_, ["_cat", "shards", _]) => {
            vec![
                at("indices:monitor/stats", 2),
                Cluster("cluster:monitor/state"),
            ]
        }
        (_, ["_query"]) => vec![AnyIndex("indices:data/read/esql", "")],
        (_, ["_bulk"] | [_, "_bulk"]) => vec![AnyIndex("indices:data/write/bulk", "")],
        (_, ["_search", "scroll", ..]) => vec![AnyIndex("indices:data/read/scroll", "")],
        (_, ["_search", "template"]) => vec![
            AnyIndex("indices:data/read/search/template", ""),
            every("indices:data/read/search"),
        ],
        (_, [_, "_search", "template"]) => vec![
            AnyIndex("indices:data/read/search/template", ""),
            at("indices:data/read/search", 0),
        ],
        (_, ["_msearch", "template"] | [_, "_msearch", "template"]) => {
            vec![AnyIndex("indices:data/read/msearch/template", "")]
        }
        (_, ["_msearch"] | [_, "_msearch"]) => vec![AnyIndex("indices:data/read/msearch", "")],
        (_, ["_mget"] | [_, "_mget"]) => vec![AnyIndex("indices:data/read/mget", "")],
        (_, ["_search" | "_count"]) => vec![every("indices:data/read/search")],
        (_, ["_field_caps"]) => vec![every("indices:data/read/field_caps")],
        ("PUT", [_]) => vec![literal("indices:admin/create")],
        ("DELETE", [_]) => vec![at("indices:admin/delete", 0)],
        ("GET", [_]) => vec![at("indices:admin/get", 0)],
        ("PUT", [_, "_settings"]) => vec![at("indices:admin/settings/update", 0)],
        (_, [_, "_settings", ..]) => vec![at("indices:monitor/settings/get", 0)],
        (_, [_, "_close"]) => vec![at("indices:admin/close", 0)],
        (_, [_, "_open"]) => vec![at("indices:admin/open", 0)],
        ("GET", [_, "_alias" | "_aliases", ..]) => vec![at("indices:admin/aliases/get", 0)],
        (_, [_, "_alias" | "_aliases", alias]) => {
            vec![at("indices:admin/aliases", 0), AliasNames(alias)]
        }
        (_, [_, "_refresh"]) => vec![at("indices:admin/refresh", 0)],
        (_, [_, "_flush"]) => vec![at("indices:admin/flush", 0)],
        ("GET", [_, "_mapping"]) => vec![at("indices:admin/mappings/get", 0)],
        (_, [_, "_mapping"]) => vec![at("indices:admin/mapping/put", 0)],
        (_, [_, "_stats", ..]) => vec![at("indices:monitor/stats", 0)],
        (_, [_, "_search" | "_count"]) => vec![at("indices:data/read/search", 0)],
        (_, [_, "_field_caps"]) => vec![at("indices:data/read/field_caps", 0)],
        (_, [_, "_pit"]) => vec![at("indices:data/read/open_point_in_time", 0)],
        ("GET", [_, "_doc" | "_source", _]) => vec![at("indices:data/read/get", 0)],
        ("DELETE", [index, "_doc", _]) => vec![Write {
            action: "indices:data/write/delete",
            item: "indices:data/write/delete",
            index,
        }],
        (_, [index, "_doc", _]) if params.get("op_type").is_none_or(|o| o != "create") => {
            vec![Write {
                action: "indices:data/write/index",
                item: "indices:data/write/index:op_type/index",
                index,
            }]
        }
        // Documents with generated ids are created.
        (_, [index, "_doc" | "_create", ..]) => vec![Write {
            action: "indices:data/write/index",
            item: "indices:data/write/index:op_type/create",
            index,
        }],
        (_, [_, "_update", _]) => vec![at("indices:data/write/update", 0)],
        (_, [_, "_update_by_query"]) => vec![
            at("indices:data/write/update/byquery", 0),
            at("indices:data/read/search", 0),
        ],
        (_, [_, "_delete_by_query"]) => vec![
            at("indices:data/write/delete/byquery", 0),
            at("indices:data/read/search", 0),
        ],
        _ => vec![],
    }
}

/// Middleware: refuses an API key's request that its role descriptors do not grant, with
/// Elasticsearch's 403, and narrows the wildcards (or the absent index expression) of the
/// requests it lets through to the names it may use.
pub async fn authorize(
    State(security): State<Arc<Security>>,
    mut req: Request,
    next: Next,
) -> Response {
    let Some(Authn::ApiKey { id, roles, .. }) = req.extensions().get::<Authn>().cloned() else {
        return next.run(req).await;
    };
    let key = Key {
        id: &id,
        roles: &roles,
    };
    let params = Query::<HashMap<String, String>>::try_from_uri(req.uri())
        .map(|q| q.0)
        .unwrap_or_default();
    match key.check(&security, req.method(), req.uri(), &params) {
        Ok(Some(uri)) => *req.uri_mut() = uri,
        Ok(None) => {}
        Err(denied) => return denied.into_response(),
    }
    next.run(req).await
}

impl Key<'_> {
    /// Checks `method uri`, returning the URI with its index expression narrowed, if it has
    /// one, or the response refusing it.
    fn check(
        &self,
        security: &Security,
        method: &Method,
        uri: &Uri,
        params: &HashMap<String, String>,
    ) -> Result<Option<Uri>, AppError> {
        // Checked decoded, as the handlers' `Path` extractors see them: `a1%2Cb1` names b1 too.
        let decoded: Vec<String> = (uri.path().split('/').filter(|s| !s.is_empty()))
            .map(decode)
            .collect::<Result<_, _>>()?;
        let segs: Vec<&str> = decoded.iter().map(String::as_str).collect();
        let mut rewritten = None;
        for step in steps(method, &segs, params) {
            match step {
                Step::Cluster(action) => {
                    if !self.cluster(action) {
                        return Err(self.denied(action, &[]));
                    }
                }
                Step::AnyIndex(action, names) => self.any_index(action, &[names])?,
                Step::Indices { action, at, expand } => {
                    let expr = match at {
                        Place::Segment(i) => segs[i],
                        Place::Prefix | Place::Suffix => "_all",
                    };
                    let names =
                        self.indices(&security.store.read(), action, expr, expand, params)?;
                    if expand {
                        let names = expression(&names, encoded);
                        let mut segs: Vec<String> = segs.iter().map(|s| encoded(s)).collect();
                        match at {
                            Place::Segment(i) => segs[i] = names,
                            Place::Prefix => segs.insert(0, names),
                            Place::Suffix => segs.push(names),
                        }
                        rewritten = Some(format!("/{}", segs.join("/")));
                    }
                }
                Step::Write {
                    action,
                    item,
                    index,
                } => {
                    self.any_index(action, &[index])?;
                    self.write(&security.store, item, index)?;
                }
                Step::AliasNames(aliases) => {
                    let action = "indices:admin/aliases";
                    // ponytail: alias wildcards are not checked
                    let literal = aliases.split(',').filter(|a| !a.contains('*'));
                    if !literal.into_iter().all(|a| self.index(action, a)) {
                        return Err(self.denied(action, &[]));
                    }
                }
            }
        }
        let query = uri.query().map_or(String::new(), |q| format!("?{q}"));
        (rewritten
            .map(|path| Uri::try_from(format!("{path}{query}")))
            .transpose())
        .map_err(|e| AppError::IllegalArgument(e.to_string()))
    }

    /// The names `expr` targets for `action` that the key may use, expanding wildcards (if
    /// `expand`) to those only; a literal name it may not use refuses the request.
    fn indices(
        &self,
        shared: &Shared,
        action: &'static str,
        expr: &str,
        expand: bool,
        params: &HashMap<String, String>,
    ) -> Result<Vec<String>, AppError> {
        let parts: Vec<&str> = (expr.split(',').map(str::trim))
            .filter(|p| !p.is_empty())
            .collect();
        let literal = |p: &&str| !expand || !(p.contains('*') || *p == "_all");
        let named: Vec<&str> = (parts.iter().enumerate())
            .filter(|(n, p)| literal(p) && !(*n > 0 && p.starts_with('-')))
            .map(|(_, p)| *p)
            .collect();
        self.any_index(action, &named)?;
        let o = match action {
            // Alias actions, like deletion, expand wildcards to indices only.
            "indices:admin/delete" | "indices:admin/aliases" => IndicesOptions::DELETE,
            a if a.starts_with("indices:data/") => IndicesOptions::DATA,
            _ => IndicesOptions::READ,
        }
        .with_params(params);
        let (mut out, mut denied) = (Vec::<String>::new(), vec![]);
        for (n, part) in parts.iter().enumerate() {
            if let (true, true, Some(excluded)) = (expand, n > 0, part.strip_prefix('-')) {
                out.retain(|name| !glob(excluded, name));
                continue;
            }
            if literal(part) {
                if self.index(action, part) {
                    out.push((*part).to_owned());
                } else if !o.ignore_unavailable {
                    denied.push(*part);
                }
                continue;
            }
            let pattern = if *part == "_all" { "*" } else { part };
            let mut found = false;
            for (name, index) in shared.iter() {
                let expands = match index.closed() {
                    true => o.expand_closed && !o.refuse_closed,
                    false => o.expand_open,
                };
                let aliases = index.aliases().keys().filter(|_| o.aliases);
                for name in std::iter::once(name).filter(|_| expands).chain(aliases) {
                    if glob(pattern, name) && self.index(action, name) {
                        found = true;
                        if !out.contains(name) {
                            out.push(name.clone());
                        }
                    }
                }
            }
            if !found && !o.allow_no_indices {
                return Err(AppError::IndexNotFound((*part).to_owned()));
            }
        }
        if !denied.is_empty() {
            return Err(self.denied(action, &java_order(&denied)));
        }
        let restricted: Vec<&str> = (out.iter())
            .filter(|n| shared.contains_key(*n) || es_index::aliases::is_alias(&**shared, n))
            .filter(|n| self.field_or_document_level(action, n))
            .map(String::as_str)
            .collect();
        if !restricted.is_empty() {
            return Err(AppError::DlsFlsUnlicensed(restricted.join(",")));
        }
        Ok(out)
    }

    /// Authorizes the bulk item action `item` on `index`: auto-creation first if `index` is
    /// missing (deletes create nothing), then `item` itself.
    fn write(&self, store: &Store, item: &str, index: &str) -> Result<(), AppError> {
        let auto = "indices:admin/auto_create";
        let creates = item != "indices:data/write/delete" && {
            let shared = store.read();
            !(shared.contains_key(index) || es_index::aliases::is_alias(&*shared, index))
        };
        if creates && !self.index(auto, index) {
            return Err(self.denied(auto, &[index]));
        }
        match self.index(item, index) {
            true => Ok(()),
            false => Err(self.denied("indices:data/write/bulk[s]", &[index])),
        }
    }

    fn cluster(&self, action: &str) -> bool {
        (self.roles.iter()).all(|set| roles(set).any(|r| grants(&r["cluster"], action)))
    }

    /// Fails unless some index privilege grants `action`; the denial names `names`.
    fn any_index(&self, action: &'static str, names: &[&str]) -> Result<(), AppError> {
        let names: Vec<&str> = names.iter().copied().filter(|n| !n.is_empty()).collect();
        let granted =
            (self.roles.iter()).all(|set| entries(set).any(|e| grants(&e["privileges"], action)));
        match granted {
            true => Ok(()),
            false => Err(self.denied(action, &java_order(&names))),
        }
    }

    /// Whether the key may do `action` on the index, alias or pattern `name`.
    fn index(&self, action: &str, name: &str) -> bool {
        (self.roles.iter()).all(|set| entries(set).any(|e| covers(e, action, name)))
    }

    /// Whether a role entry that grants `action` on `name` restricts fields or documents.
    fn field_or_document_level(&self, action: &str, name: &str) -> bool {
        (self.roles.iter()).any(|set| {
            entries(set).any(|e| {
                covers(e, action, name) && (!e["field_security"].is_null() || !e["query"].is_null())
            })
        })
    }

    /// Elasticsearch's 403 for `action`, naming the indices `names` if any.
    fn denied(&self, action: &str, names: &[&str]) -> AppError {
        let on = match names {
            [] => String::new(),
            _ => format!(" on indices [{}]", names.join(",")),
        };
        let kind = match action.starts_with("cluster:") {
            true => "cluster",
            false => "index",
        };
        let reason = format!(
            "action [{action}] is unauthorized for API key id [{}] of user [elastic]{on}, this action is granted by the {kind} privileges [{}]",
            self.id,
            granted_by(action)
        );
        AppError::Other(StatusCode::FORBIDDEN, "security_exception", reason)
    }
}

/// Authorizes a bulk item, `op` (`index`, `create`, `update` or `delete`) on `index`, for
/// whoever `authn` is; Elasticsearch fails the item alone when its key may not.
pub(crate) fn bulk_item(
    authn: Option<&Authn>,
    store: &Store,
    op: &str,
    index: &str,
) -> Result<(), AppError> {
    let Some(Authn::ApiKey { id, roles, .. }) = authn else {
        return Ok(());
    };
    let item = match op {
        "index" => "indices:data/write/index:op_type/index",
        "create" => "indices:data/write/index:op_type/create",
        "update" => "indices:data/write/update",
        _ => "indices:data/write/delete",
    };
    Key { id, roles }.write(store, item, index)
}

/// Authorizes an mget shard request, which reads its items through `names`, for whoever
/// `authn` is; Elasticsearch fails all the shard's items when its key may not read one name.
pub(crate) fn mget_shard(authn: Option<&Authn>, names: &[&str]) -> Result<(), AppError> {
    let Some(Authn::ApiKey { id, roles, .. }) = authn else {
        return Ok(());
    };
    let key = Key { id, roles };
    let action = "indices:data/read/mget[shard]";
    let denied: Vec<&str> = (names.iter().copied())
        .filter(|n| !key.index(action, n))
        .collect();
    match denied.is_empty() {
        true => Ok(()),
        false => Err(key.denied(action, &java_order(&denied))),
    }
}

/// Authorizes one search of an msearch on the index expression `expr` for whoever `authn` is,
/// returning `expr` with its wildcards narrowed to the names the key may read; Elasticsearch
/// checks each search alone.
pub(crate) fn msearch_item(
    authn: Option<&Authn>,
    store: &Store,
    expr: String,
    params: &HashMap<String, String>,
) -> Result<String, AppError> {
    let Some(Authn::ApiKey { id, roles, .. }) = authn else {
        return Ok(expr);
    };
    let action = "indices:data/read/search";
    let names = Key { id, roles }.indices(&store.read(), action, &expr, true, params)?;
    Ok(expression(&names, |n| n.to_owned()))
}

/// Authorizes a search of a point in time opened on `indices` for whoever `authn` is: the key
/// must read all of them, whoever opened it, and a refusal names none (as Elasticsearch's).
pub(crate) fn pit_indices(authn: Option<&Authn>, indices: &[&str]) -> Result<(), AppError> {
    let Some(Authn::ApiKey { id, roles, .. }) = authn else {
        return Ok(());
    };
    let key = Key { id, roles };
    let action = "indices:data/read/search";
    match indices.iter().all(|i| key.index(action, i)) {
        true => Ok(()),
        false => Err(key.denied(action, &[])),
    }
}

/// Narrows the index expression `expr` of an ES|QL `FROM` to the names whoever `authn` is may
/// read. As in Elasticsearch, a literal name the key may not read is unknown when nothing else
/// matches, and otherwise refused.
pub(crate) fn esql_from(
    authn: Option<&Authn>,
    shared: &Shared,
    expr: &str,
) -> Result<String, AppError> {
    let Some(Authn::ApiKey { id, roles, .. }) = authn else {
        return Ok(expr.to_owned());
    };
    let key = Key { id, roles };
    let action = "indices:data/read/esql";
    let lenient = HashMap::from(
        ["ignore_unavailable", "allow_no_indices"].map(|o| (o.to_owned(), "true".to_owned())),
    );
    let narrowed = expression(&key.indices(shared, action, expr, true, &lenient)?, |n| {
        n.to_owned()
    });
    let denied: Vec<&str> = (expr.split(',').map(str::trim))
        .filter(|p| !(p.is_empty() || p.contains('*') || p.starts_with('-')))
        .filter(|p| !key.index(action, p))
        .collect();
    if denied.is_empty() {
        return Ok(narrowed);
    }
    match es_index::aliases::resolve_data(&**shared, &narrowed).is_ok_and(|t| !t.is_empty()) {
        true => Err(key.denied("indices:data/read/esql/search_shards", &java_order(&denied))),
        false => Err(AppError::Verification(format!("Unknown index [{expr}]"))),
    }
}

/// Authorizes `POST /_aliases` `actions` for whoever `authn` is, as Elasticsearch does: every
/// action's indices need `indices:admin/aliases` (wildcards narrowed to those the key may
/// use), then its alias names do (removed wildcards narrowed likewise), and an added alias may
/// not grant more than the indices it is added to.
pub(crate) fn alias_actions(
    authn: Option<&Authn>,
    shared: &Shared,
    actions: &mut [Value],
) -> Result<(), AppError> {
    let Some(Authn::ApiKey { id, roles, .. }) = authn else {
        return Ok(());
    };
    let key = Key { id, roles };
    let action = "indices:admin/aliases";
    let strict = HashMap::from([("allow_no_indices".to_owned(), "false".to_owned())]);
    for (_, a) in bodies(actions) {
        for k in ["index", "indices"] {
            if let Some(names) = a.get(k) {
                let expr = strs_or_str(names).collect::<Vec<_>>().join(",");
                a[k] = json!(key.indices(shared, action, &expr, true, &strict)?);
            }
        }
    }
    for (kind, a) in bodies(actions) {
        for k in ["alias", "aliases"] {
            let Some(names) = a.get(k) else { continue };
            let mut out: Vec<String> = vec![];
            for name in strs_or_str(names) {
                if kind == "remove" && (name.contains('*') || name == "_all") {
                    let pattern = if name == "_all" { "*" } else { name };
                    // ponytail: a pattern matching no alias the key may manage removes nothing;
                    // Elasticsearch's response to that is not recorded
                    for n in (shared.values()).flat_map(|i| i.aliases().keys()) {
                        if glob(pattern, n) && key.index(action, n) && !out.contains(n) {
                            out.push(n.clone());
                        }
                    }
                } else if key.index(action, name) {
                    out.push(name.to_owned());
                } else {
                    return Err(key.denied(action, &[]));
                }
            }
            if kind == "add" {
                more_than_indices(&key, &out, &a["index"], &a["indices"])?;
            }
            a[k] = json!(out);
        }
    }
    Ok(())
}

/// Each alias action's kind and body.
fn bodies(actions: &mut [Value]) -> impl Iterator<Item = (&String, &mut Value)> {
    (actions.iter_mut()).filter_map(|a| {
        a.as_object_mut()
            .filter(|o| o.len() == 1)?
            .iter_mut()
            .next()
    })
}

/// Refuses adding `aliases` to the indices `index`/`indices` if the key may do an index action
/// on an alias that it may not on an index.
// ponytail: compares the actions es-mini knows, not Elasticsearch's action automata
fn more_than_indices(
    key: &Key,
    aliases: &[String],
    index: &Value,
    indices: &Value,
) -> Result<(), AppError> {
    let mut actions = (GRANTED_BY.iter()).filter(|(a, _)| a.starts_with("indices:"));
    let names: Vec<&str> = strs_or_str(index).chain(strs_or_str(indices)).collect();
    let more = actions.any(|(a, _)| {
        (aliases.iter()).any(|alias| key.index(a, alias) && names.iter().any(|n| !key.index(a, n)))
    });
    match more {
        true => Err(AppError::Other(
            StatusCode::FORBIDDEN,
            "security_exception",
            "Adding an alias is not allowed when the alias has more permissions than any of the indices".into(),
        )),
        false => Ok(()),
    }
}

/// The names of a string-or-array parameter.
fn strs_or_str(v: &Value) -> impl Iterator<Item = &str> + Clone {
    v.as_str().into_iter().chain(strs(v))
}

/// `names` as one index expression, each through `name`; none is Elasticsearch's no-index
/// marker, which matches nothing.
fn expression(names: &[String], name: impl Fn(&str) -> String) -> String {
    match names.is_empty() {
        true => "*,-*".to_owned(),
        false => names.iter().map(|n| name(n)).collect::<Vec<_>>().join(","),
    }
}

/// The privileges of one kind (cluster or index) that `_has_privileges` compares.
struct Kind {
    /// The named privileges that a named privilege implies, where they are not just itself and
    /// `none`, as Elasticsearch 9.5.5's `_has_privileges` reports them (recorded).
    implies: &'static [(&'static str, &'static str)],
    /// The action patterns of named privileges as (name, included, excluded), from
    /// Elasticsearch's `ClusterPrivilegeResolver` and `IndexPrivilege`, to compare named
    /// privileges with privileges that are action patterns.
    // ponytail: only these privileges; any other named one is never implied by action patterns
    actions: &'static [(
        &'static str,
        &'static [&'static str],
        &'static [&'static str],
    )],
}

const ALL_CLUSTER: &[&str] = &[
    "cluster:*",
    "indices:admin/template/*",
    "indices:admin/index_template/*",
];
const API_KEYS: &[&str] = &["cluster:admin/xpack/security/api_key/*"];

const CLUSTER: Kind = Kind {
    implies: &[
        (
            "all",
            "all,cancel_task,cross_cluster_search,manage,manage_api_key,manage_ccr,manage_ilm,manage_index_templates,manage_ml,manage_own_api_key,manage_security,manage_token,monitor,monitor_ml,monitor_stats,none,read_ccr,read_ilm,read_security,transport_client",
        ),
        (
            "monitor",
            "monitor,monitor_ml,monitor_stats,none,transport_client",
        ),
        (
            "manage",
            "cancel_task,cross_cluster_search,manage,manage_ilm,manage_index_templates,manage_ml,monitor,monitor_ml,monitor_stats,none,read_ilm,transport_client",
        ),
        (
            "manage_security",
            "manage_api_key,manage_own_api_key,manage_security,manage_token,none,read_security",
        ),
        ("manage_api_key", "manage_api_key,manage_own_api_key,none"),
        ("manage_own_api_key", "manage_own_api_key"),
        ("manage_ml", "manage_ml,monitor_ml,none"),
        ("manage_ilm", "manage_ilm,none,read_ilm"),
        ("manage_ccr", "manage_ccr,none,read_ccr"),
    ],
    actions: &[
        ("all", ALL_CLUSTER, &[]),
        ("manage", ALL_CLUSTER, &["cluster:admin/xpack/security/*"]),
        (
            "monitor",
            &[
                "cluster:monitor/*",
                "indices:admin/template/get",
                "indices:admin/index_template/get",
                "cluster:admin/component_template/get",
            ],
            &[],
        ),
        ("monitor_stats", &["cluster:monitor/stats*"], &[]),
        (
            "manage_security",
            &["cluster:admin/xpack/security/*"],
            &["cluster:admin/xpack/security/delegate_pki"],
        ),
        ("manage_api_key", API_KEYS, &[]),
        ("manage_own_api_key", API_KEYS, &[]),
    ],
};

const INDEX: Kind = Kind {
    implies: &[
        (
            "all",
            "all,auto_configure,create,create_doc,create_index,cross_cluster_replication,cross_cluster_replication_internal,delete,delete_index,index,maintenance,manage,manage_data_stream_lifecycle,manage_failure_store,manage_follow_index,manage_ilm,manage_leader_index,monitor,none,read,read_cross_cluster,read_failure_store,view_index_metadata,write",
        ),
        ("create", "create,create_doc,none"),
        ("index", "create,create_doc,index,none"),
        (
            "manage",
            "auto_configure,create_index,delete_index,maintenance,manage,manage_data_stream_lifecycle,manage_failure_store,manage_follow_index,manage_ilm,manage_leader_index,monitor,none,view_index_metadata",
        ),
        ("none", ""),
        ("read", "none,read,read_cross_cluster"),
        ("write", "create,create_doc,delete,index,none,write"),
        ("read_failure_store", "read_failure_store"),
        (
            "manage_failure_store",
            "manage,manage_data_stream_lifecycle,manage_failure_store",
        ),
    ],
    actions: &[
        (
            "all",
            &["indices:*", "internal:transport/proxy/indices:*"],
            &[],
        ),
        (
            "read",
            &[
                "indices:data/read/*",
                "internal:transport/proxy/indices:data/read/*",
                "indices:admin/resolve/index",
            ],
            &[],
        ),
        (
            "write",
            &["indices:data/write/*", "indices:admin/mapping/auto_put"],
            &[],
        ),
        (
            "index",
            &[
                "indices:data/write/index*",
                "indices:data/write/bulk*",
                "indices:data/write/update*",
            ],
            &[],
        ),
        ("monitor", &["indices:monitor/*"], &[]),
        (
            "manage",
            &[
                "indices:monitor/*",
                "indices:admin/*",
                "indices:data/read/field_caps*",
            ],
            &[],
        ),
    ],
};

impl Kind {
    /// Whether the privileges `granted` (names, or patterns over action names) imply the
    /// privilege `requested`.
    fn implies(&self, granted: &[&str], requested: &str) -> bool {
        let named = |p: &str| !p.contains(':');
        let by_name = |g: &&str| match self.implies.iter().find(|(n, _)| n == g) {
            Some((_, implied)) => implied.split(',').any(|i| i == requested),
            None => *g == requested || requested == "none",
        };
        if granted.iter().filter(|g| named(g)).any(by_name) {
            return true;
        }
        // manage_own_api_key grants actions on the user's own keys only, so no action outright.
        let acting = || granted.iter().filter(|g| **g != "manage_own_api_key");
        if requested == "none" {
            return granted.iter().any(|g| !named(g));
        }
        if !named(requested) && !requested.contains('*') {
            let named_by = granted_by(requested).split(',');
            if acting().any(|g| named_by.clone().any(|n| n == *g)) {
                return true;
            }
        }
        let Some((wanted, _)) = self.actions(requested) else {
            return false;
        };
        let covered = |w: &&str| {
            acting().filter_map(|g| self.actions(g)).any(|(inc, exc)| {
                inc.iter().any(|i| glob(i, w)) && !exc.iter().any(|e| glob(e, w) || glob(w, e))
            })
        };
        wanted.iter().all(covered)
    }

    /// The action patterns of `privilege` as (included, excluded), if known.
    fn actions<'a>(&self, privilege: &'a str) -> Option<(Vec<&'a str>, &'static [&'static str])> {
        if privilege.contains(':') {
            return Some((vec![privilege], &[]));
        }
        (self.actions.iter())
            .find(|(n, ..)| *n == privilege)
            .map(|(_, inc, exc)| (inc.to_vec(), *exc))
    }
}

/// The strings of a JSON string array.
fn strs(v: &Value) -> impl Iterator<Item = &str> + Clone {
    v.as_array().into_iter().flatten().filter_map(Value::as_str)
}

/// Whether the index privilege entry `e` covers the index name or pattern `name`; restricted
/// indices (es-mini's `.security-7` and `.security`) only if the entry allows them, and a
/// pattern checked with `allow_restricted` only if it cannot match them or the entry allows them.
// ponytail: Elasticsearch restricts every system index; es-mini has only these
fn names_cover(e: &Value, name: &str, allow_restricted: bool) -> bool {
    let restricted = [super::INDEX, super::ALIAS];
    let reaches_restricted = restricted.contains(&name)
        || (allow_restricted && name.contains('*') && restricted.iter().any(|r| glob(name, r)));
    strs(&e["names"]).any(|n| glob(n, name))
        && (e["allow_restricted_indices"] == true || !reaches_restricted)
}

impl Key<'_> {
    fn has_cluster(&self, privilege: &str) -> bool {
        (self.roles.iter()).all(|set| {
            let granted: Vec<&str> = roles(set).flat_map(|r| strs(&r["cluster"])).collect();
            CLUSTER.implies(&granted, privilege)
        })
    }

    fn has_index(&self, name: &str, allow_restricted: bool, privilege: &str) -> bool {
        (self.roles.iter()).all(|set| {
            let covering = entries(set).filter(|e| names_cover(e, name, allow_restricted));
            let granted: Vec<&str> = covering.flat_map(|e| strs(&e["privileges"])).collect();
            // A role whose index privileges are all `none` has no index privileges at all.
            let some = || {
                entries(set)
                    .flat_map(|e| strs(&e["privileges"]))
                    .any(|p| p != "none")
            };
            match privilege {
                "none" if granted.contains(&"none") => some(),
                _ => !granted.is_empty() && INDEX.implies(&granted, privilege),
            }
        })
    }

    fn has_application(&self, app: &str, resource: &str, privilege: &str) -> bool {
        (self.roles.iter()).all(|set| {
            roles(set)
                .flat_map(|r| r["applications"].as_array().into_iter().flatten())
                .any(|a| {
                    a["application"].as_str().is_some_and(|n| glob(n, app))
                        && strs(&a["privileges"]).any(|p| p == "*" || p == privilege)
                        && strs(&a["resources"]).any(|r| glob(r, resource))
                })
        })
    }
}

/// `GET|POST /_security/user/_has_privileges`: which of the privileges the request names the
/// user (`elastic`, or an API key, limited by its creator) has.
pub(super) async fn has_privileges(
    Extension(authn): Extension<Authn>,
    body: Bytes,
) -> Result<Json<Value>, AppError> {
    if body.is_empty() {
        let reason = "request body or source parameter is required";
        return Err(AppError::Parse(reason.into()));
    }
    let body = crate::docs::parse_json(&body)?;
    let who = "elastic/has_privileges";
    let fields = ["cluster", "index", "application"];
    let check = format!("failed to parse privileges check [{who}].");
    let given = body.as_object().into_iter().flatten();
    if let Some((k, _)) = given.clone().find(|(k, _)| !fields.contains(&k.as_str())) {
        return Err(AppError::Parse(format!("{check} unexpected field [{k}]")));
    }
    if given.count() == 0 {
        let reason =
            format!("{check} All privilege fields [cluster,index,applications] are missing");
        return Err(AppError::Parse(reason));
    }
    let entries_of = format!("failed to parse indices privileges for role [{who}].");
    let index = match body.get("index") {
        None => &vec![],
        Some(Value::Array(index)) => index,
        Some(v) => {
            let token = es_index::json_token(v);
            return Err(AppError::Parse(format!(
                "{entries_of} expected field [index] value to be an array, but found [{token}] instead"
            )));
        }
    };
    for entry in index {
        for f in ["field_security", "query"] {
            if entry.get(f).is_some() {
                let reason = format!("Field [{f}] is not supported in a has_privileges request");
                return Err(AppError::Parse(reason));
            }
        }
        if let Some(v) = entry
            .get("allow_restricted_indices")
            .filter(|v| !v.is_boolean())
        {
            let token = es_index::json_token(v);
            return Err(AppError::Parse(format!(
                "{entries_of} expected field [allow_restricted_indices] value to be a boolean, but found [{token}] instead"
            )));
        }
    }
    let cluster = body.get("cluster").cloned().unwrap_or(json!([]));
    let role = json!({"cluster": cluster, "indices": index});
    let role = super::role_descriptor(who, &role).map_err(AppError::Chain)?;
    let index = role["indices"].as_array().map_or(&[][..], Vec::as_slice);
    for entry in index {
        if strs(&entry["names"]).next().is_none() {
            let reason = format!("{entries_of} [names] cannot be an empty array");
            return Err(AppError::Parse(reason));
        }
        if strs(&entry["privileges"]).next().is_none() {
            let reason = "indices privileges must define at least one privilege";
            return Err(AppError::IllegalArgument(reason.into()));
        }
    }
    let application = body["application"]
        .as_array()
        .map_or(&[][..], Vec::as_slice);
    let cluster: Vec<&str> = strs(&role["cluster"]).collect();
    if cluster.is_empty() && index.is_empty() && application.is_empty() {
        let reason = "must specify at least one privilege";
        return Err(AppError::ActionRequestValidation(reason.into()));
    }
    if let Some(e) = super::privilege_errors(&role).into_iter().next() {
        return Err(AppError::IllegalArgument(e));
    }
    // ponytail: of the validations of application privileges, only the name's length
    for a in application {
        let name = a["application"].as_str().unwrap_or("");
        if name.chars().count() < 3 {
            return Err(AppError::ActionRequestValidation(format!(
                "An application name prefix must be at least 3 characters long (found '{name}')"
            )));
        }
    }
    let elastic;
    let (id, roles) = match &authn {
        Authn::Elastic => {
            elastic = [super::superuser()];
            ("", &elastic[..])
        }
        Authn::ApiKey { id, roles, .. } => (id.as_str(), &roles[..]),
    };
    let key = Key { id, roles };
    let mut all = true;
    let mut has = |granted: bool| {
        all &= granted;
        granted
    };
    let cluster: serde_json::Map<String, Value> = (java_order(&cluster).into_iter())
        .map(|p| (p.to_owned(), has(key.has_cluster(p)).into()))
        .collect();
    let mut indices = BTreeMap::<&str, BTreeMap<&str, bool>>::new();
    for entry in index {
        let allow = entry["allow_restricted_indices"] == true;
        for name in strs(&entry["names"]) {
            for p in strs(&entry["privileges"]) {
                let granted = has(key.has_index(name, allow, p));
                *indices.entry(name).or_default().entry(p).or_insert(true) &= granted;
            }
        }
    }
    let mut applications = BTreeMap::<&str, BTreeMap<&str, BTreeMap<&str, bool>>>::new();
    for a in application {
        let name = a["application"].as_str().unwrap_or("");
        for resource in strs(&a["resources"]) {
            for p in strs(&a["privileges"]) {
                let granted = has(key.has_application(name, resource, p));
                let resources = applications.entry(name).or_default();
                *resources
                    .entry(resource)
                    .or_default()
                    .entry(p)
                    .or_insert(true) &= granted;
            }
        }
    }
    Ok(Json(json!({
        "username": "elastic",
        "has_all_requested": all,
        "cluster": cluster,
        "index": indices,
        "application": applications,
    })))
}

/// The roles of one set of role descriptors.
fn roles(set: &Value) -> impl Iterator<Item = &Value> {
    set.as_object().into_iter().flat_map(|m| m.values())
}

/// The index privilege entries of one set of role descriptors.
fn entries(set: &Value) -> impl Iterator<Item = &Value> {
    roles(set).flat_map(|r| r["indices"].as_array().into_iter().flatten())
}

/// Whether the index privilege entry `e` grants `action` on `name`.
// ponytail: names match with `*` wildcards only (Elasticsearch also takes `?` and /regex/), and
// restricted indices are left to the handlers' own refusal
fn covers(e: &Value, action: &str, name: &str) -> bool {
    let names = e["names"].as_array().into_iter().flatten();
    names.filter_map(Value::as_str).any(|n| glob(n, name)) && grants(&e["privileges"], action)
}

/// The path segment `seg` percent-decoded, as axum's `Path` extractor decodes it.
fn decode(seg: &str) -> Result<String, AppError> {
    let bytes = seg.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes
            .get(i + 1..i + 3)
            .filter(|h| h.iter().all(u8::is_ascii_hexdigit));
        match hex
            .filter(|_| bytes[i] == b'%')
            .and_then(|h| std::str::from_utf8(h).ok())
        {
            Some(h) => (
                out.push(u8::from_str_radix(h, 16).unwrap_or_default()),
                i += 3,
            ),
            _ => (out.push(bytes[i]), i += 1),
        };
    }
    String::from_utf8(out).map_err(|e| AppError::IllegalArgument(e.to_string()))
}

/// `name` with the bytes a path segment cannot hold percent-encoded.
fn encoded(name: &str) -> String {
    let mut out = String::new();
    for b in name.bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' => out.push(b.into()),
            _ if b"-._~!$&'()*+,;=:@".contains(&b) => out.push(b.into()),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// `names` without repeats in Java `HashSet` iteration order (by `String.hashCode` bucket), as
/// Elasticsearch lists the indices it denies.
fn java_order<'a>(names: &[&'a str]) -> Vec<&'a str> {
    let mut unique: Vec<&str> = vec![];
    for n in names {
        if !unique.contains(n) {
            unique.push(n);
        }
    }
    let mut capacity = 16;
    while unique.len() > capacity * 3 / 4 {
        capacity *= 2;
    }
    let bucket = |s: &str| {
        let h = (s.encode_utf16()).fold(0u32, |h, c| h.wrapping_mul(31).wrapping_add(c.into()));
        ((h ^ (h >> 16)) as usize) & (capacity - 1)
    };
    unique.sort_by_key(|n| bucket(n));
    unique
}

#[cfg(test)]
mod tests {
    #[test]
    fn names_are_encoded_for_paths() {
        assert_eq!(super::encoded("logs-é 1"), "logs-%C3%A9%201");
        assert_eq!(
            super::decode("a%2Cb%2c%C3%A9%zz%+1%").unwrap(),
            "a,b,é%zz%+1%"
        );
        assert_eq!(super::encoded("50%"), "50%25");
    }

    #[test]
    fn java_hash_set_order() {
        assert_eq!(super::java_order(&["b1", "c1", "b1"]), ["c1", "b1"]);
        let keys = [
            "monitor",
            "manage",
            "cluster:monitor/health",
            "cluster:monitor/*",
            "manage_own_api_key",
        ];
        let expected = [
            "monitor",
            "cluster:monitor/health",
            "manage_own_api_key",
            "manage",
            "cluster:monitor/*",
        ];
        assert_eq!(super::java_order(&keys), expected);
    }
}
