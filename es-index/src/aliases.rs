//! Index aliases and index-expression resolution (comma lists, `*` wildcards, `_all`, aliases).

use crate::Indices;
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

pub fn is_alias(indices: &Indices, name: &str) -> bool {
    indices.values().any(|i| i.aliases.contains_key(name))
}

/// Resolves a read expression to concrete indices, in name order. Missing literal names fail;
/// wildcards may match nothing.
// ponytail: no `-exclusion` patterns, hidden indices, or expand_wildcards options
pub fn resolve_read<'a>(indices: &'a Indices, expr: &str) -> Result<Vec<Target>, ResolveError> {
    // Per index: None once reached unfiltered, else the filters of the aliases that reached it.
    let mut hits: BTreeMap<&str, Option<Vec<Value>>> = BTreeMap::new();
    let mut add = |index: &'a str, filter: Option<&Value>| {
        let entry = hits.entry(index).or_insert_with(|| Some(vec![]));
        match (entry.as_mut(), filter) {
            (Some(fs), Some(f)) => fs.push(f.clone()),
            (_, None) => *entry = None,
            (None, Some(_)) => {}
        }
    };
    for part in expr.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let pattern = if part == "_all" { "*" } else { part };
        let wildcard = pattern.contains('*');
        let mut found = false;
        for (name, index) in indices {
            if glob(pattern, name) {
                add(name, None);
                found = true;
            }
            for (alias, meta) in &index.aliases {
                if glob(pattern, alias) {
                    add(name, meta.filter.as_ref());
                    found = true;
                }
            }
        }
        if !found && !wildcard {
            return Err(ResolveError::IndexNotFound(part.to_owned()));
        }
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

/// Resolves a write target: an index (existing or to be auto-created) or an alias's write index.
pub fn resolve_write(indices: &Indices, name: &str) -> Result<String, ResolveError> {
    if indices.contains_key(name) {
        return Ok(name.to_owned());
    }
    let members: Vec<(&String, &Alias)> = indices
        .iter()
        .filter_map(|(n, i)| Some((n, i.aliases.get(name)?)))
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
    use crate::Index;
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
    fn glob_patterns() {
        assert!(glob("a*c", "abbc") && glob("*", "") && !glob("a*c", "abd") && glob("é*", "éa"));
    }
}
