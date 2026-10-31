//! Alias endpoints: `_alias`, `_aliases`, and `POST /_aliases` actions.

use crate::{
    AppState,
    error::AppError,
    indices::{check, parse_body},
    security::{Authn, alias_actions},
};
use axum::{
    Extension, Json,
    body::Bytes,
    extract::{Path, Query, State},
    http::{StatusCode, Uri},
    response::{IntoResponse, Response},
};
use es_index::{
    Indices,
    aliases::{self, Alias, glob},
};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};

type Params = Query<HashMap<String, String>>;

const WRITE_PARAMS: [&str; 2] = ["timeout", "master_timeout"];

/// Resolves `<static{math{format|zone}}>` date math names like ES's DateMathExpressionResolver;
/// other names pass through.
pub fn resolve_date_math(name: &str) -> Result<String, AppError> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64);
    resolve_date_math_at(name, now)
}

pub(crate) fn resolve_date_math_at(name: &str, now: i64) -> Result<String, AppError> {
    let bad = |why: &str| {
        AppError::IllegalArgument(format!("invalid dynamic name expression [{name}]. {why}"))
    };
    let Some(inner) = name.strip_prefix('<').and_then(|n| n.strip_suffix('>')) else {
        return Ok(name.to_string());
    };
    let mut out = String::new();
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => out.push(chars.next().ok_or_else(|| bad("trailing escape"))?),
            '{' => {
                let (mut expr, mut depth) = (String::new(), 1);
                for c in chars.by_ref() {
                    depth += i32::from(c == '{') - i32::from(c == '}');
                    if depth == 0 {
                        break;
                    }
                    expr.push(c);
                }
                if depth != 0 {
                    return Err(bad("date math placeholder is open ended"));
                }
                out.push_str(&resolve_expr(&expr, now).map_err(|e| bad(&e))?);
            }
            c => out.push(c),
        }
    }
    Ok(out)
}

/// One placeholder `math`, `math{format}` or `math{format|zone}`; format defaults to `yyyy.MM.dd`.
fn resolve_expr(expr: &str, now: i64) -> Result<String, String> {
    let (math, spec) = match expr.split_once('{') {
        Some((m, f)) => (m, f.strip_suffix('}').ok_or("missing closing brace")?),
        None => (expr, "yyyy.MM.dd"),
    };
    let (fmt, zone) = spec.split_once('|').unwrap_or((spec, "UTC"));
    let tz = es_index::tz::TimeZone::parse(zone).ok_or(format!("unknown time zone [{zone}]"))?;
    let t = es_index::date_math::parse(math, None, None, &tz, false, now)?;
    es_index::format_date(fmt, t, &tz).ok_or(format!("invalid date format [{fmt}]"))
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
fn resolved_names(
    indices: &aliases::Map<impl aliases::IndexMeta>,
    expr: &str,
) -> Result<Vec<String>, AppError> {
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
    uri: Uri,
    Query(params): Params,
    body: Bytes,
) -> Result<Json<Value>, AppError> {
    check(&uri, &params, &WRITE_PARAMS)?;
    let meta = parse_alias(&parse_body(&body)?)?;
    let mut indices = state.write();
    check_alias_name(&indices, &alias)?;
    for name in resolved_names(&indices, &index)? {
        if let Some(i) = indices.get_mut(&name) {
            i.aliases.insert(alias.clone(), meta.clone());
        }
    }
    Ok(Json(json!({"acknowledged": true})))
}

pub async fn delete(
    State(state): State<AppState>,
    Path((index, names)): Path<(String, String)>,
    uri: Uri,
    Query(params): Params,
) -> Result<Json<Value>, AppError> {
    check(&uri, &params, &WRITE_PARAMS)?;
    let mut indices = state.write();
    let mut removed = false;
    for name in resolved_names(&indices, &index)? {
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
    indices: &aliases::Map<impl aliases::IndexMeta>,
    index: Option<&str>,
    names: Option<&str>,
) -> Result<(Value, Vec<String>), AppError> {
    let selected = match index {
        Some(expr) => resolved_names(indices, expr)?,
        None => indices.keys().cloned().collect(),
    };
    let patterns = names.map(split);
    let mut out = Map::new();
    let mut seen = BTreeSet::new();
    for name in selected {
        let aliases: Map<String, Value> = indices[&name]
            .aliases()
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
    // An exclusion before any positive pattern is a literal name, so it is missing.
    let mut has_positive = false;
    let mut missing: Vec<String> = patterns
        .into_iter()
        .flatten()
        .filter(|p| {
            if p.starts_with('-') {
                return !has_positive;
            }
            has_positive = true;
            !p.contains('*') && *p != "_all" && !seen.contains(*p)
        })
        .map(str::to_owned)
        .collect();
    // With only exclusions, just the first one acts as a literal name.
    if !has_positive {
        missing.truncate(1);
    }
    missing.sort();
    Ok((Value::Object(out), missing))
}

fn get_response(
    state: &AppState,
    index: Option<&str>,
    names: Option<&str>,
) -> Result<Response, AppError> {
    let (mut body, missing) = lookup(&*state.read(), index, names)?;
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

pub async fn get_all(
    State(state): State<AppState>,
    uri: Uri,
    Query(params): Params,
) -> Result<Response, AppError> {
    check(&uri, &params, &["local"])?;
    get_response(&state, None, None)
}

pub async fn get_named(
    State(state): State<AppState>,
    Path(names): Path<String>,
    uri: Uri,
    Query(params): Params,
) -> Result<Response, AppError> {
    check(&uri, &params, &["local"])?;
    get_response(&state, None, Some(&names))
}

pub async fn get_for_index(
    State(state): State<AppState>,
    Path(index): Path<String>,
    uri: Uri,
    Query(params): Params,
) -> Result<Response, AppError> {
    check(&uri, &params, &["local"])?;
    get_response(&state, Some(&index), None)
}

pub async fn get_for_index_named(
    State(state): State<AppState>,
    Path((index, names)): Path<(String, String)>,
    uri: Uri,
    Query(params): Params,
) -> Result<Response, AppError> {
    check(&uri, &params, &["local"])?;
    get_response(&state, Some(&index), Some(&names))
}

fn exists(state: &AppState, index: Option<&str>, names: &str) -> StatusCode {
    match lookup(&*state.read(), index, Some(names)) {
        Ok((body, missing))
            if missing.is_empty() && body.as_object().is_some_and(|o| !o.is_empty()) =>
        {
            StatusCode::OK
        }
        _ => StatusCode::NOT_FOUND,
    }
}

pub async fn head_named(
    State(state): State<AppState>,
    Path(names): Path<String>,
    uri: Uri,
    Query(params): Params,
) -> Result<StatusCode, AppError> {
    check(&uri, &params, &["local"])?;
    Ok(exists(&state, None, &names))
}

pub async fn head_for_index_named(
    State(state): State<AppState>,
    Path((index, names)): Path<(String, String)>,
    uri: Uri,
    Query(params): Params,
) -> Result<StatusCode, AppError> {
    check(&uri, &params, &["local"])?;
    Ok(exists(&state, Some(&index), &names))
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
pub async fn update(
    State(state): State<AppState>,
    uri: Uri,
    Query(params): Params,
    authn: Option<Extension<Authn>>,
    body: Bytes,
) -> Result<Json<Value>, AppError> {
    check(&uri, &params, &WRITE_PARAMS)?;
    let mut body = parse_body(&body)?;
    let actions = body["actions"]
        .as_array_mut()
        .ok_or_else(|| AppError::ActionRequestValidation("no actions specified".into()))?;
    alias_actions(authn.as_deref(), &state.read(), actions)?;
    let mut indices = state.write();
    let mut tables: BTreeMap<String, BTreeMap<String, Alias>> = indices
        .iter()
        .map(|(n, i)| (n.clone(), i.aliases.clone()))
        .collect();
    let mut drop = BTreeSet::new();
    let mut added = Vec::new();
    let mut results = Vec::new();
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
            .map(|e| resolved_names(&indices, e))
            .collect::<Result<Vec<_>, _>>()?
            .concat();
        if targets.is_empty() {
            return Err(AppError::ActionRequestValidation(
                "One of [index] or [indices] is required".into(),
            ));
        }
        if kind != "remove" && a.get("must_exist").is_some() {
            return Err(AppError::IllegalArgument(format!(
                "[must_exist] is only valid for [remove] actions, not [{kind}]"
            )));
        }
        let mut failure = None;
        let mut result_action = json!({"type": kind, "indices": targets});
        if kind != "remove_index" {
            result_action["aliases"] = json!(names_of(a, "alias", "aliases"));
        }
        match kind.as_str() {
            "add" => {
                let mut meta = a.clone();
                for k in ["index", "indices", "alias", "aliases", "must_exist"] {
                    meta.as_object_mut().map(|o| o.remove(k));
                }
                let meta = parse_alias(&meta)?;
                for alias in names_of(a, "alias", "aliases") {
                    for t in &targets {
                        tables
                            .get_mut(t)
                            .map(|m| m.insert(alias.clone(), meta.clone()));
                    }
                    added.push(alias);
                }
            }
            "remove" => {
                let must_exist = a["must_exist"].as_bool() == Some(true);
                for pattern in names_of(a, "alias", "aliases") {
                    let mut hit = false;
                    for t in &targets {
                        let Some(m) = tables.get_mut(t) else { continue };
                        let before = m.len();
                        m.retain(|name, _| !alias_glob(&pattern, name));
                        hit |= m.len() != before;
                    }
                    if !hit {
                        let e = AppError::AliasesNotFound(vec![pattern]);
                        if must_exist {
                            return Err(e);
                        }
                        failure.get_or_insert(e);
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
        results.push(match failure {
            None => json!({"action": result_action, "status": 200, "error": null}),
            Some(e) => {
                let (status, error) = e.parts();
                json!({"action": result_action, "status": status.as_u16(), "error": error})
            }
        });
    }
    // Checked after the loop so a later remove_index may free the name.
    for alias in &added {
        if !drop.contains(alias) {
            check_alias_name(&indices, alias)?;
        }
    }
    for (name, table) in tables {
        if let Some(i) = indices.get_mut(&name) {
            i.aliases = table;
        }
    }
    indices.retain(|n, _| !drop.contains(n));
    let errors = results.iter().any(|r| r["status"] != 200);
    let mut body = json!({"acknowledged": true, "errors": errors});
    if errors {
        body["action_results"] = json!(results);
    }
    Ok(Json(body))
}
