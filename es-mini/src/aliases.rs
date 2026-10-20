//! Alias endpoints: `_alias`, `_aliases`, and `POST /_aliases` actions.

use crate::{AppState, error::AppError, indices::parse_body};
use axum::{
    Json,
    body::Bytes,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use es_index::{
    Indices,
    aliases::{self, Alias, glob},
};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// Resolves `<static{anchor||/d{yyyy-MM-dd}}>` date math; other names pass through.
// ponytail: only `anchor||/d{fmt}` with yyyy/MM/dd; `now` and other rounding units unsupported.
pub fn resolve_date_math(name: &str) -> String {
    let Some(inner) = name.strip_prefix('<').and_then(|n| n.strip_suffix('>')) else {
        return name.to_string();
    };
    let mut out = String::new();
    let mut rest = inner;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let Some(close) = rest.find("}}") else { break };
        let expr = &rest[open + 1..close + 1];
        rest = &rest[close + 2..];
        let (anchor, fmt) = expr.split_once("||/d{").unwrap_or((expr, "yyyy-MM-dd}"));
        let mut d = anchor.splitn(3, '-');
        let (y, m, day) = (d.next(), d.next(), d.next());
        out.push_str(
            &fmt.trim_end_matches('}')
                .replace("yyyy", y.unwrap_or(""))
                .replace("MM", m.unwrap_or(""))
                .replace("dd", day.unwrap_or("")),
        );
    }
    out + rest
}

/// Rejects values of the wrong scalar type for alias fields, as ES does at index creation.
pub fn reject_bad_scalars(name: &str, v: &Value) -> Result<(), AppError> {
    for (f, ty) in [
        ("is_write_index", "String"),
        ("is_hidden", "String"),
        ("routing", "boolean"),
    ] {
        let bad = match (v.get(f), ty) {
            (Some(Value::String(s)), "String") => Some(s.clone()),
            (Some(Value::Bool(b)), "boolean") => Some(b.to_string()),
            _ => None,
        };
        if let Some(x) = bad {
            return Err(AppError::IllegalArgument(format!(
                "Unsupported {ty} type value [{x}] for field [{f}] in alias [{name}]"
            )));
        }
    }
    Ok(())
}

/// Parses alias metadata, validating its filter as a query.
pub fn parse_alias(v: &Value) -> Result<Alias, AppError> {
    let alias = Alias::parse(v).map_err(AppError::IllegalArgument)?;
    if let Some(f) = &alias.filter {
        es_query::parse_query(f).map_err(|e| {
            AppError::IllegalArgument(format!("failed to parse filter for alias: {e}"))
        })?;
    }
    Ok(alias)
}

/// Concrete index names an expression refers to (aliases expand to their indices).
fn index_names(indices: &Indices, expr: &str) -> Result<Vec<String>, AppError> {
    Ok(aliases::resolve_read(indices, expr)?
        .into_iter()
        .map(|t| t.index)
        .collect())
}

fn check_alias_name(indices: &Indices, alias: &str) -> Result<(), AppError> {
    match indices.contains_key(alias) {
        true => Err(AppError::InvalidAliasName {
            alias: alias.to_owned(),
            reason: "an index or data stream exists with the same name as the alias".into(),
        }),
        false => Ok(()),
    }
}

fn split(names: &str) -> Vec<&str> {
    names
        .split(',')
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .collect()
}

fn alias_glob(pattern: &str, name: &str) -> bool {
    glob(if pattern == "_all" { "*" } else { pattern }, name)
}

pub async fn put(
    State(state): State<AppState>,
    Path((index, alias)): Path<(String, String)>,
    body: Bytes,
) -> Result<Json<Value>, AppError> {
    let meta = parse_alias(&parse_body(&body)?)?;
    let mut indices = state.write().expect("lock poisoned");
    check_alias_name(&indices, &alias)?;
    for name in index_names(&indices, &index)? {
        if let Some(i) = indices.get_mut(&name) {
            i.aliases.insert(alias.clone(), meta.clone());
        }
    }
    Ok(Json(json!({"acknowledged": true})))
}

pub async fn delete(
    State(state): State<AppState>,
    Path((index, names)): Path<(String, String)>,
) -> Result<Json<Value>, AppError> {
    let mut indices = state.write().expect("lock poisoned");
    let mut removed = false;
    for name in index_names(&indices, &index)? {
        if let Some(i) = indices.get_mut(&name) {
            let before = i.aliases.len();
            i.aliases
                .retain(|a, _| !split(&names).iter().any(|p| alias_glob(p, a)));
            removed |= i.aliases.len() != before;
        }
    }
    match removed {
        true => Ok(Json(json!({"acknowledged": true}))),
        false => Err(AppError::AliasesNotFound(
            split(&names).into_iter().map(str::to_owned).collect(),
        )),
    }
}

/// `{index: {aliases: {...}}}` for the selected indices and alias names, plus the literal
/// names that matched nothing.
fn lookup(
    indices: &Indices,
    index: Option<&str>,
    names: Option<&str>,
) -> Result<(Value, Vec<String>), AppError> {
    let selected = match index {
        Some(expr) => index_names(indices, expr)?,
        None => indices.keys().cloned().collect(),
    };
    let patterns = names.map(split);
    let mut out = Map::new();
    let mut seen = BTreeSet::new();
    for name in selected {
        let aliases: Map<String, Value> = indices[&name]
            .aliases
            .iter()
            .inspect(|(a, _)| {
                if let Some(ps) = &patterns {
                    let hit = |p: &&str| !p.starts_with('-') && alias_glob(p, a);
                    if ps.iter().any(hit) {
                        seen.insert((*a).clone());
                    }
                }
            })
            .filter(|(a, _)| {
                patterns.as_ref().is_none_or(|ps| {
                    // Later `-x` patterns remove what earlier patterns matched.
                    ps.iter().fold(false, |acc, p| match p.strip_prefix('-') {
                        Some(r) => acc && !alias_glob(r, a),
                        None => acc || alias_glob(p, a),
                    })
                })
            })
            .map(|(a, meta)| (a.clone(), json!(meta)))
            .collect();
        if patterns.is_none() || !aliases.is_empty() {
            out.insert(name, json!({"aliases": aliases}));
        }
    }
    // Exclusions only count as missing when no positive pattern was given.
    let has_positive = patterns.iter().flatten().any(|p| !p.starts_with('-'));
    let missing = patterns
        .into_iter()
        .flatten()
        .filter(|p| {
            let name = p.strip_prefix('-').unwrap_or(p);
            !(has_positive && p.starts_with('-'))
                && !p.contains('*')
                && *p != "_all"
                && !seen.contains(name)
        })
        .map(str::to_owned)
        .collect();
    Ok((Value::Object(out), missing))
}

fn get_response(
    state: &AppState,
    index: Option<&str>,
    names: Option<&str>,
) -> Result<Response, AppError> {
    let (mut body, missing) = lookup(&state.read().expect("lock poisoned"), index, names)?;
    if missing.is_empty() {
        return Ok(Json(body).into_response());
    }
    let what = if missing.len() == 1 {
        "alias"
    } else {
        "aliases"
    };
    body["error"] = format!("{what} [{}] missing", missing.join(",")).into();
    body["status"] = 404.into();
    Ok((StatusCode::NOT_FOUND, Json(body)).into_response())
}

pub async fn get_all(State(state): State<AppState>) -> Result<Response, AppError> {
    get_response(&state, None, None)
}

pub async fn get_named(
    State(state): State<AppState>,
    Path(names): Path<String>,
) -> Result<Response, AppError> {
    get_response(&state, None, Some(&names))
}

pub async fn get_for_index(
    State(state): State<AppState>,
    Path(index): Path<String>,
) -> Result<Response, AppError> {
    get_response(&state, Some(&index), None)
}

pub async fn get_for_index_named(
    State(state): State<AppState>,
    Path((index, names)): Path<(String, String)>,
) -> Result<Response, AppError> {
    get_response(&state, Some(&index), Some(&names))
}

fn exists(state: &AppState, index: Option<&str>, names: &str) -> StatusCode {
    match lookup(&state.read().expect("lock poisoned"), index, Some(names)) {
        Ok((body, missing))
            if missing.is_empty() && body.as_object().is_some_and(|o| !o.is_empty()) =>
        {
            StatusCode::OK
        }
        _ => StatusCode::NOT_FOUND,
    }
}

pub async fn head_named(State(state): State<AppState>, Path(names): Path<String>) -> StatusCode {
    exists(&state, None, &names)
}

pub async fn head_for_index_named(
    State(state): State<AppState>,
    Path((index, names)): Path<(String, String)>,
) -> StatusCode {
    exists(&state, Some(&index), &names)
}

/// String-or-array parameter of an alias action, under its singular or plural key.
fn names_of(action: &Value, one: &str, many: &str) -> Vec<String> {
    match action.get(one).or_else(|| action.get(many)) {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect(),
        Some(Value::String(s)) => vec![s.clone()],
        _ => vec![],
    }
}

/// Applies `POST /_aliases` actions all-or-nothing: they run against a copy of the alias
/// tables, which replaces the real ones only if every action succeeds.
pub async fn update(State(state): State<AppState>, body: Bytes) -> Result<Json<Value>, AppError> {
    let body = parse_body(&body)?;
    let actions = body["actions"]
        .as_array()
        .ok_or_else(|| AppError::ActionRequestValidation("no actions specified".into()))?;
    let mut indices = state.write().expect("lock poisoned");
    let mut tables: BTreeMap<String, BTreeMap<String, Alias>> = indices
        .iter()
        .map(|(n, i)| (n.clone(), i.aliases.clone()))
        .collect();
    let mut drop = BTreeSet::new();
    for action in actions {
        let Some((kind, a)) = action
            .as_object()
            .filter(|o| o.len() == 1)
            .and_then(|o| o.iter().next())
        else {
            return Err(AppError::IllegalArgument(format!(
                "malformed alias action [{action}]"
            )));
        };
        let targets: Vec<String> = names_of(a, "index", "indices")
            .iter()
            .map(|e| index_names(&indices, e))
            .collect::<Result<Vec<_>, _>>()?
            .concat();
        if targets.is_empty() {
            return Err(AppError::ActionRequestValidation(
                "One of [index] or [indices] is required".into(),
            ));
        }
        match kind.as_str() {
            "add" => {
                let mut meta = a.clone();
                for k in ["index", "indices", "alias", "aliases", "must_exist"] {
                    meta.as_object_mut().map(|o| o.remove(k));
                }
                let meta = parse_alias(&meta)?;
                for alias in names_of(a, "alias", "aliases") {
                    check_alias_name(&indices, &alias)?;
                    for t in &targets {
                        tables
                            .get_mut(t)
                            .map(|m| m.insert(alias.clone(), meta.clone()));
                    }
                }
            }
            "remove" => {
                let must_exist = a["must_exist"].as_bool() != Some(false);
                for pattern in names_of(a, "alias", "aliases") {
                    let mut hit = false;
                    for t in &targets {
                        let Some(m) = tables.get_mut(t) else { continue };
                        let before = m.len();
                        m.retain(|name, _| !alias_glob(&pattern, name));
                        hit |= m.len() != before;
                    }
                    if !hit && must_exist {
                        return Err(AppError::AliasesNotFound(vec![pattern]));
                    }
                }
            }
            "remove_index" => drop.extend(targets),
            other => {
                return Err(AppError::IllegalArgument(format!(
                    "unknown alias action [{other}]"
                )));
            }
        }
    }
    for (name, table) in tables {
        if let Some(i) = indices.get_mut(&name) {
            i.aliases = table;
        }
    }
    indices.retain(|n, _| !drop.contains(n));
    Ok(Json(json!({"acknowledged": true, "errors": false})))
}
