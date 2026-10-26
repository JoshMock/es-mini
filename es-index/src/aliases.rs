//! Index aliases and index-expression resolution (comma lists, `*` wildcards, `_all`, aliases).

use crate::Index;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// Alias metadata as ES returns it. Routing is stored and echoed back, but has no effect with
/// one shard per index.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Alias {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index_routing: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub search_routing: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_write_index: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_hidden: Option<bool>,
}

impl Alias {
    /// Parses an alias definition; `routing` sets both routing fields.
    pub fn parse(v: &Value) -> Result<Self, String> {
        let mut v = match v {
            Value::Null => json!({}),
            v => v.clone(),
        };
        if let Some(r) = v.as_object_mut().and_then(|o| o.remove("routing")) {
            let r = Value::String(match r {
                Value::String(s) => s,
                r => r.to_string(),
            });
            v["index_routing"] = r.clone();
            v["search_routing"] = r;
        }
        Self::deserialize(&v).map_err(|e| e.to_string())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ResolveError {
    IndexNotFound(String),
    /// An alias over several indices without exactly one write index.
    NoWriteIndex(String),
    IndexClosed(String),
    /// A missing literal name that no index could have, e.g. one starting with `_`.
    InvalidIndexName(String),
    /// A literal alias name where only concrete indices are allowed (index deletion).
    AliasNotAllowed(String),
}

/// One concrete index to read, with the filter of the alias it was reached through.
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub index: String,
    pub filter: Option<Value>,
}

pub fn glob(pattern: &str, name: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == name,
        Some((pre, rest)) => {
            let Some(tail) = name.strip_prefix(pre) else {
                return false;
            };
            tail.char_indices()
                .map(|(i, _)| i)
                .chain([tail.len()])
                .any(|i| glob(rest, &tail[i..]))
        }
    }
}

/// Indices keyed by name, held directly ([`crate::Indices`]) or through locks.
pub type Map<I> = BTreeMap<String, I>;

/// What index-expression resolution reads of an index.
pub trait IndexMeta {
    fn aliases(&self) -> &BTreeMap<String, Alias>;
    fn closed(&self) -> bool;
}

impl IndexMeta for Index {
    fn aliases(&self) -> &BTreeMap<String, Alias> {
        &self.aliases
    }
    fn closed(&self) -> bool {
        self.closed
    }
}

pub fn is_alias(indices: &Map<impl IndexMeta>, name: &str) -> bool {
    indices.values().any(|i| i.aliases().contains_key(name))
}

/// How an index expression resolves, like Elasticsearch's `IndicesOptions`. Endpoints start
/// from a preset and apply their request's `ignore_unavailable`, `allow_no_indices` and
/// `expand_wildcards` with [`IndicesOptions::with_params`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IndicesOptions {
    /// Skip literal names that are missing (or closed, where closed indices are refused).
    pub ignore_unavailable: bool,
    /// Let a wildcard, or the whole expression, match nothing; otherwise that is `IndexNotFound`.
    pub allow_no_indices: bool,
    /// Which indices wildcards (and `_all`) expand to. Literal names are never filtered.
    pub expand_open: bool,
    pub expand_closed: bool,
    /// Whether the expression may name aliases (deletion takes concrete indices only).
    pub aliases: bool,
    /// Document reads and searches refuse closed indices named directly or through an alias.
    pub refuse_closed: bool,
}

impl IndicesOptions {
    /// Metadata reads (GET index, mappings, settings, alias management): open and closed.
    pub const READ: Self = Self {
        ignore_unavailable: false,
        allow_no_indices: true,
        expand_open: true,
        expand_closed: true,
        aliases: true,
        refuse_closed: false,
    };
    /// Document reads and searches: wildcards skip closed indices, named ones are refused.
    pub const DATA: Self = Self {
        expand_closed: false,
        refuse_closed: true,
        ..Self::READ
    };
    /// Index deletion: concrete indices only.
    pub const DELETE: Self = Self {
        aliases: false,
        ..Self::READ
    };

    /// Applies a request's `ignore_unavailable`, `allow_no_indices` and `expand_wildcards`.
    // ponytail: `hidden` is accepted and ignored; es-mini has no hidden indices
    pub fn with_params(mut self, params: &std::collections::HashMap<String, String>) -> Self {
        let flag = |k: &str| params.get(k).map(|v| v.is_empty() || v == "true");
        self.ignore_unavailable = flag("ignore_unavailable").unwrap_or(self.ignore_unavailable);
        self.allow_no_indices = flag("allow_no_indices").unwrap_or(self.allow_no_indices);
        if let Some(w) = params.get("expand_wildcards") {
            let has = |s: &str| w.split(',').any(|x| x == s || x == "all");
            (self.expand_open, self.expand_closed) = (has("open"), has("closed"));
        }
        self
    }
}

/// Resolves an index expression (comma list of names, `*` wildcards, `_all`, aliases, and
/// `-exclusions` after the first part) to concrete indices in name order, each with the filter
/// of the alias it was reached through (none if it was also reached directly).
pub fn resolve(
    indices: &Map<impl IndexMeta>,
    expr: &str,
    o: &IndicesOptions,
) -> Result<Vec<Target>, ResolveError> {
    // Per index: None once reached unfiltered, else the filters of the aliases that reached it.
    let mut hits: BTreeMap<&str, Option<Vec<Value>>> = BTreeMap::new();
    fn add<'a>(
        hits: &mut BTreeMap<&'a str, Option<Vec<Value>>>,
        index: &'a str,
        filter: Option<&Value>,
    ) {
        let entry = hits.entry(index).or_insert_with(|| Some(vec![]));
        match (entry.as_mut(), filter) {
            (Some(fs), Some(f)) => fs.push(f.clone()),
            (_, None) => *entry = None,
            (None, Some(_)) => {}
        }
    }
    let expands = |i: &dyn IndexMeta| {
        if i.closed() {
            o.expand_closed && !o.refuse_closed
        } else {
            o.expand_open
        }
    };
    let parts = expr.split(',').map(str::trim).filter(|p| !p.is_empty());
    for (n, part) in parts.enumerate() {
        if let (true, Some(excluded)) = (n > 0, part.strip_prefix('-')) {
            hits.retain(|name, _| {
                !glob(excluded, name)
                    && !(o.aliases && indices[*name].aliases().keys().any(|a| glob(excluded, a)))
            });
            continue;
        }
        let pattern = if part == "_all" { "*" } else { part };
        if pattern.contains('*') {
            let mut found = false;
            for (name, index) in indices.iter().filter(|(_, i)| expands(*i)) {
                if glob(pattern, name) {
                    add(&mut hits, name, None);
                    found = true;
                }
                for (_, meta) in index
                    .aliases()
                    .iter()
                    .filter(|(a, _)| o.aliases && glob(pattern, a))
                {
                    add(&mut hits, name, meta.filter.as_ref());
                    found = true;
                }
            }
            if !found && !o.allow_no_indices {
                return Err(ResolveError::IndexNotFound(part.to_owned()));
            }
            continue;
        }
        // A literal name: an index, or an alias over its indices.
        let mut named: Vec<(&str, Option<&Value>)> = match indices.get_key_value(part) {
            Some((name, _)) => vec![(name.as_str(), None)],
            None => indices
                .iter()
                .filter_map(|(name, i)| {
                    Some((name.as_str(), i.aliases().get(part)?.filter.as_ref()))
                })
                .collect(),
        };
        let missing = match () {
            _ if named.is_empty() && part.starts_with('_') => {
                Some(ResolveError::InvalidIndexName(part.to_owned()))
            }
            _ if named.is_empty() => Some(ResolveError::IndexNotFound(part.to_owned())),
            _ if !o.aliases && !indices.contains_key(part) => {
                Some(ResolveError::AliasNotAllowed(part.to_owned()))
            }
            _ => named
                .iter()
                .find(|(name, _)| o.refuse_closed && indices[*name].closed())
                .map(|(name, _)| ResolveError::IndexClosed((*name).to_owned())),
        };
        match missing {
            Some(_) if o.ignore_unavailable => named.clear(),
            Some(e) => return Err(e),
            None => {}
        }
        for (name, filter) in named {
            add(&mut hits, name, filter);
        }
    }
    if hits.is_empty() && !o.allow_no_indices {
        return Err(ResolveError::IndexNotFound(expr.to_owned()));
    }
    Ok(hits
        .into_iter()
        .map(|(index, filters)| Target {
            index: index.to_owned(),
            filter: filters.and_then(|mut fs| match fs.len() {
                0 => None,
                1 => fs.pop(),
                _ => Some(json!({"bool": {"should": fs}})),
            }),
        })
        .collect())
}

/// [`resolve`] with [`IndicesOptions::READ`]: metadata reads.
pub fn resolve_read(
    indices: &Map<impl IndexMeta>,
    expr: &str,
) -> Result<Vec<Target>, ResolveError> {
    resolve(indices, expr, &IndicesOptions::READ)
}

/// [`resolve`] with [`IndicesOptions::DATA`]: document reads and searches.
pub fn resolve_data(
    indices: &Map<impl IndexMeta>,
    expr: &str,
) -> Result<Vec<Target>, ResolveError> {
    resolve(indices, expr, &IndicesOptions::DATA)
}

/// Resolves a write target: an index (existing or to be auto-created) or an alias's write index.
pub fn resolve_write(indices: &Map<impl IndexMeta>, name: &str) -> Result<String, ResolveError> {
    let target = resolve_write_target(indices, name)?;
    match indices.get(&target) {
        Some(i) if i.closed() => Err(ResolveError::IndexClosed(target)),
        _ => Ok(target),
    }
}

fn resolve_write_target(indices: &Map<impl IndexMeta>, name: &str) -> Result<String, ResolveError> {
    if indices.contains_key(name) {
        return Ok(name.to_owned());
    }
    let members: Vec<(&String, &Alias)> = indices
        .iter()
        .filter_map(|(n, i)| Some((n, i.aliases().get(name)?)))
        .collect();
    let explicit: Vec<&String> = members
        .iter()
        .filter(|(_, a)| a.is_write_index == Some(true))
        .map(|(n, _)| *n)
        .collect();
    match (members.as_slice(), explicit.as_slice()) {
        ([], _) => Ok(name.to_owned()),
        (_, [w]) => Ok((*w).clone()),
        ([(only, a)], []) if a.is_write_index != Some(false) => Ok((*only).clone()),
        _ => Err(ResolveError::NoWriteIndex(name.to_owned())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Index, Indices};
    use serde_json::json;

    fn indices() -> Indices {
        let mut all = Indices::new();
        for name in ["logs-1", "logs-2", "other"] {
            all.insert(name.into(), Index::default());
        }
        let alias = |v| Alias::parse(&v).unwrap();
        all.get_mut("logs-1")
            .unwrap()
            .aliases
            .insert("logs".into(), alias(json!({})));
        all.get_mut("logs-2")
            .unwrap()
            .aliases
            .insert("logs".into(), alias(json!({"is_write_index": true})));
        let f = json!({"filter": {"term": {"tag": "x"}}});
        all.get_mut("other")
            .unwrap()
            .aliases
            .insert("filtered".into(), alias(f));
        all.get_mut("logs-1").unwrap().aliases.insert(
            "filtered".into(),
            alias(json!({"filter": {"term": {"n": 1}}})),
        );
        all
    }

    fn names(t: &[Target]) -> Vec<&str> {
        t.iter().map(|t| t.index.as_str()).collect()
    }

    #[test]
    fn exclusion_removes_matching_indices() {
        let all = indices();
        let names = |e| -> Vec<String> {
            resolve_read(&all, e)
                .unwrap()
                .into_iter()
                .map(|t| t.index)
                .collect()
        };
        assert_eq!(names("logs-*,-logs-1"), ["logs-2"]);
        assert_eq!(names("*,-logs"), ["other"]);
    }

    #[test]
    fn parse_expands_routing_and_rejects_unknown_keys() {
        let a = Alias::parse(&json!({"routing": "r"})).unwrap();
        assert_eq!(
            (a.index_routing.as_deref(), a.search_routing.as_deref()),
            (Some("r"), Some("r"))
        );
        assert_eq!(
            serde_json::to_value(&a).unwrap(),
            json!({"index_routing": "r", "search_routing": "r"})
        );
        assert!(Alias::parse(&json!({"nope": 1})).is_err());
    }

    #[test]
    fn read_resolution() {
        let all = indices();
        assert_eq!(names(&resolve_read(&all, "other").unwrap()), ["other"]);
        assert_eq!(
            names(&resolve_read(&all, "logs").unwrap()),
            ["logs-1", "logs-2"]
        );
        assert_eq!(
            names(&resolve_read(&all, "logs-*,other").unwrap()),
            ["logs-1", "logs-2", "other"]
        );
        assert_eq!(resolve_read(&all, "_all").unwrap().len(), 3);
        assert_eq!(
            names(&resolve_read(&all, "filt*").unwrap()),
            ["logs-1", "other"]
        );
        assert!(resolve_read(&all, "nomatch*").unwrap().is_empty());
        assert_eq!(
            resolve_read(&all, "other,nope"),
            Err(ResolveError::IndexNotFound("nope".into()))
        );
    }

    #[test]
    fn read_filters() {
        let all = indices();
        let t = resolve_read(&all, "filtered").unwrap();
        assert_eq!(t[1].filter, Some(json!({"term": {"tag": "x"}})));
        // Reaching an index directly as well drops the alias filter.
        let t = resolve_read(&all, "filtered,other").unwrap();
        assert_eq!(t[1].filter, None);
    }

    #[test]
    fn write_resolution() {
        let mut all = indices();
        assert_eq!(resolve_write(&all, "other"), Ok("other".into()));
        assert_eq!(resolve_write(&all, "new-index"), Ok("new-index".into()));
        assert_eq!(resolve_write(&all, "logs"), Ok("logs-2".into()));
        assert_eq!(
            resolve_write(&all, "filtered"),
            Err(ResolveError::NoWriteIndex("filtered".into()))
        );
        all.remove("other");
        // A single index behind an alias is its write index unless explicitly disabled.
        assert_eq!(resolve_write(&all, "filtered"), Ok("logs-1".into()));
        all.get_mut("logs-1")
            .unwrap()
            .aliases
            .get_mut("filtered")
            .unwrap()
            .is_write_index = Some(false);
        assert_eq!(
            resolve_write(&all, "filtered"),
            Err(ResolveError::NoWriteIndex("filtered".into()))
        );
    }

    #[test]
    fn data_resolution_rejects_named_closed_indices_and_skips_wildcard_ones() {
        let mut all = indices();
        all.get_mut("logs-1").unwrap().closed = true;
        let closed = || Err(ResolveError::IndexClosed("logs-1".into()));
        assert_eq!(resolve_data(&all, "logs-1"), closed());
        assert_eq!(
            resolve_data(&all, "logs"),
            closed(),
            "an alias names its indices"
        );
        assert_eq!(names(&resolve_data(&all, "logs-*").unwrap()), ["logs-2"]);
        assert_eq!(resolve_data(&all, "_all").unwrap().len(), 2);
        let excluded = resolve_data(&all, "*,-other").unwrap();
        assert_eq!(names(&excluded), ["logs-2"], "an exclusion names nothing");
        // Metadata reads still see closed indices.
        assert_eq!(names(&resolve_read(&all, "logs-1").unwrap()), ["logs-1"]);
        assert_eq!(
            resolve_write(&all, "logs-1"),
            Err(ResolveError::IndexClosed("logs-1".into()))
        );
        all.get_mut("logs-2").unwrap().closed = true;
        assert_eq!(
            resolve_write(&all, "logs"),
            Err(ResolveError::IndexClosed("logs-2".into()))
        );
    }

    fn opts(base: IndicesOptions, params: &[(&str, &str)]) -> IndicesOptions {
        let params: std::collections::HashMap<String, String> = params
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        base.with_params(&params)
    }

    #[test]
    fn options_ignore_unavailable_drops_missing_and_closed_names() {
        let mut all = indices();
        all.get_mut("other").unwrap().closed = true;
        let o = opts(IndicesOptions::DATA, &[("ignore_unavailable", "true")]);
        assert_eq!(
            names(&resolve(&all, "logs-1,nope,other", &o).unwrap()),
            ["logs-1"]
        );
        let o = IndicesOptions::DATA;
        assert_eq!(
            resolve(&all, "logs-1,nope", &o),
            Err(ResolveError::IndexNotFound("nope".into()))
        );
    }

    #[test]
    fn options_wildcards_skip_closed_even_next_to_names() {
        let mut all = indices();
        all.get_mut("other").unwrap().closed = true;
        // A wildcard matching a closed index next to a literal name no longer errors.
        assert_eq!(
            names(&resolve(&all, "logs-1,o*", &IndicesOptions::DATA).unwrap()),
            ["logs-1"]
        );
        assert_eq!(
            resolve(&all, "other", &IndicesOptions::DATA),
            Err(ResolveError::IndexClosed("other".into()))
        );
        let o = opts(IndicesOptions::READ, &[("expand_wildcards", "closed")]);
        assert_eq!(names(&resolve(&all, "*", &o).unwrap()), ["other"]);
        // expand_wildcards never filters literal names.
        assert_eq!(names(&resolve(&all, "logs-1", &o).unwrap()), ["logs-1"]);
    }

    #[test]
    fn options_allow_no_indices_is_per_wildcard() {
        let all = indices();
        let o = opts(IndicesOptions::READ, &[("allow_no_indices", "false")]);
        assert_eq!(
            resolve(&all, "other,nomatch*", &o),
            Err(ResolveError::IndexNotFound("nomatch*".into()))
        );
        assert_eq!(
            names(&resolve(&all, "other,nomatch*", &IndicesOptions::READ).unwrap()),
            ["other"]
        );
        // ...and to the whole result, e.g. when ignore_unavailable dropped every name.
        let o = opts(
            IndicesOptions::READ,
            &[
                ("allow_no_indices", "false"),
                ("ignore_unavailable", "true"),
            ],
        );
        assert_eq!(
            resolve(&all, "nope", &o),
            Err(ResolveError::IndexNotFound("nope".into()))
        );
    }

    #[test]
    fn options_delete_matches_concrete_indices_only() {
        let all = indices();
        assert_eq!(
            resolve(&all, "logs", &IndicesOptions::DELETE),
            Err(ResolveError::AliasNotAllowed("logs".into()))
        );
        assert_eq!(
            names(&resolve(&all, "filt*", &IndicesOptions::DELETE).unwrap()),
            Vec::<&str>::new()
        );
        assert_eq!(
            names(&resolve(&all, "logs-*", &IndicesOptions::DELETE).unwrap()),
            ["logs-1", "logs-2"]
        );
        let o = opts(IndicesOptions::DELETE, &[("ignore_unavailable", "true")]);
        assert!(resolve(&all, "logs", &o).unwrap().is_empty());
    }

    #[test]
    fn glob_patterns() {
        assert!(glob("a*c", "abbc") && glob("*", "") && !glob("a*c", "abd") && glob("é*", "éa"));
    }
}
