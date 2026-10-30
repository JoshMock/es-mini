//! Authorization of API keys, as Elasticsearch's `RBACEngine` does it: each REST request is the
//! action Elasticsearch names it, granted by cluster privileges, or by index privileges on the
//! names it targets, where wildcards expand to the indices and aliases the key may use only.

use super::{Authn, Security};
use crate::error::AppError;
use axum::{
    extract::{Query, Request, State},
    http::{Method, StatusCode, Uri},
    middleware::Next,
    response::{IntoResponse, Response},
};
use es_index::aliases::{IndexMeta, IndicesOptions, glob};
use serde_json::Value;
use std::collections::HashMap;
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
    ("indices:data/read/esql", "read_cross_cluster,read,all"),
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
        // ponytail: manage_own_api_key does not let a key query its own keys (Elasticsearch
        // narrows the query to the key itself), so it is refused rather than over-granted
        .filter(|p| !(action == QUERY_KEYS && *p == "manage_own_api_key"))
        .any(|p| named.split(',').any(|n| n == p) || (p.contains(':') && glob(p, action)))
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
// ponytail: bulk, mget, msearch, ES|QL and POST _aliases are checked by action only, not per
// index they name yet
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
        ("POST", ["_aliases"]) => vec![AnyIndex("indices:admin/aliases", "")],
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
        let path = uri.path();
        let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
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
                    let names = self.indices(security, action, expr, expand, params)?;
                    if expand {
                        // An empty expansion matches nothing, as Elasticsearch's no-index marker.
                        let names = if names.is_empty() {
                            "*,-*".to_owned()
                        } else {
                            names
                                .iter()
                                .map(|n| encoded(n))
                                .collect::<Vec<_>>()
                                .join(",")
                        };
                        let mut segs: Vec<&str> = segs.clone();
                        match at {
                            Place::Segment(i) => segs[i] = &names,
                            Place::Prefix => segs.insert(0, &names),
                            Place::Suffix => segs.push(&names),
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
                    let auto = "indices:admin/auto_create";
                    let shared = security.store.read();
                    let exists =
                        shared.contains_key(index) || es_index::aliases::is_alias(&*shared, index);
                    if !exists && !self.index(auto, index) {
                        return Err(self.denied(auto, &[index]));
                    }
                    if !self.index(item, index) {
                        return Err(self.denied("indices:data/write/bulk[s]", &[index]));
                    }
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
        security: &Security,
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
            "indices:admin/delete" => IndicesOptions::DELETE,
            a if a.starts_with("indices:data/") => IndicesOptions::DATA,
            _ => IndicesOptions::READ,
        }
        .with_params(params);
        let shared = security.store.read();
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
            .filter(|n| shared.contains_key(*n) || es_index::aliases::is_alias(&*shared, n))
            .filter(|n| self.field_or_document_level(action, n))
            .map(String::as_str)
            .collect();
        if !restricted.is_empty() {
            return Err(AppError::DlsFlsUnlicensed(restricted.join(",")));
        }
        Ok(out)
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

/// `name` with the bytes a path segment cannot hold percent-encoded (`%` is kept: names taken
/// from the path are still encoded).
fn encoded(name: &str) -> String {
    let mut out = String::new();
    for b in name.bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' => out.push(b.into()),
            _ if b"-._~!$&'()*+,;=:@%".contains(&b) => out.push(b.into()),
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
