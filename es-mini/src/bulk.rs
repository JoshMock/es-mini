use crate::{
    AppState,
    docs::{auto_id, delete_doc, parse_json, put_or_create, update_doc},
    error::AppError,
};
use axum::{
    Json,
    body::Bytes,
    extract::{Path, State},
};
use serde_json::{Value, json};
use std::time::Instant;

struct Action {
    op: String,
    index: String,
    id: Option<String>,
    body: Option<Value>,
}

/// Parses the whole NDJSON body up front, so a malformed request executes nothing.
fn parse(body: &[u8], default_index: Option<&str>) -> Result<Vec<Action>, AppError> {
    let mut lines = body
        .split(|b| *b == b'\n')
        .filter(|l| !l.trim_ascii().is_empty());
    let mut actions = Vec::new();
    while let Some(line) = lines.next() {
        let meta = parse_json(line)?;
        let Some((op, meta)) = meta
            .as_object()
            .filter(|m| m.len() == 1)
            .and_then(|m| m.iter().next())
        else {
            return Err(AppError::IllegalArgument(format!(
                "Malformed action/metadata line, found [{meta}]"
            )));
        };
        if !["index", "create", "update", "delete"].contains(&op.as_str()) {
            return Err(AppError::IllegalArgument(format!(
                "Malformed action/metadata line, expected one of [create, delete, index, update] but found [{op}]"
            )));
        }
        let index = meta["_index"]
            .as_str()
            .or(default_index)
            .ok_or_else(|| AppError::ActionRequestValidation("index is missing".into()))?;
        let id = meta["_id"].as_str().map(str::to_owned);
        if id.is_none() && (op == "update" || op == "delete") {
            return Err(AppError::ActionRequestValidation("id is missing".into()));
        }
        let op = if op == "index" && meta["op_type"] == "create" {
            &"create".to_owned()
        } else {
            op
        };
        let body = match op.as_str() {
            "delete" => None,
            _ => {
                let line = lines.next().ok_or_else(|| {
                    AppError::IllegalArgument(
                        "The bulk request must be terminated by a newline [\\n]".into(),
                    )
                })?;
                Some(parse_json(line)?)
            }
        };
        actions.push(Action {
            op: op.clone(),
            index: index.to_owned(),
            id,
            body,
        });
    }
    Ok(actions)
}

/// Resolves a write target that names an alias (and no index) to its single index.
// ponytail: no is_write_index support; multi-index aliases always fail
fn resolve_alias(state: &AppState, name: &str) -> Result<String, AppError> {
    let indices = state.read().expect("lock poisoned");
    if indices.contains_key(name) {
        return Ok(name.to_owned());
    }
    let mut targets = indices.iter().filter(|(_, i)| i.aliases.contains(name));
    match (targets.next(), targets.next()) {
        (Some((n, _)), None) => Ok(n.clone()),
        (Some(_), Some(_)) => Err(AppError::IllegalArgument(format!(
            "no write index is defined for alias [{name}]. The write index may be explicitly disabled using is_write_index=false or the alias points to multiple indices without one being designated as a write index"
        ))),
        _ => Ok(name.to_owned()),
    }
}

fn execute(state: &AppState, a: Action) -> Value {
    let resolved = resolve_alias(state, &a.index);
    let target = resolved.as_deref().unwrap_or(&a.index).to_owned();
    let id = a.id.unwrap_or_else(auto_id);
    let body = a.body.unwrap_or(Value::Null);
    let result = match a.op.as_str() {
        _ if resolved.is_err() => resolved.map(|_| unreachable!()),
        _ if id.is_empty() => Err(AppError::IllegalArgument(
            "if _id is specified it must not be empty".into(),
        )),
        "index" => put_or_create(state, &target, &id, body, false),
        "create" => put_or_create(state, &target, &id, body, true),
        "update" => update_doc(state, &target, &id, &body),
        _ => delete_doc(state, &target, &id),
    };
    let item = match result {
        Ok((status, Json(mut v))) => {
            v["status"] = status.as_u16().into();
            v
        }
        Err(e) => {
            let (status, mut error) = e.parts();
            if let Value::Object(m) = &mut error {
                m.remove("root_cause");
            }
            json!({"_index": a.index, "_id": id, "status": status.as_u16(), "error": error})
        }
    };
    json!({ a.op: item })
}

async fn run(
    state: AppState,
    default_index: Option<&str>,
    body: Bytes,
) -> Result<Json<Value>, AppError> {
    let start = Instant::now();
    let items: Vec<Value> = parse(&body, default_index)?
        .into_iter()
        .map(|a| execute(&state, a))
        .collect();
    let errors = items.iter().any(|i| {
        i.as_object()
            .and_then(|m| m.values().next())
            .is_some_and(|v| v.get("error").is_some())
    });
    Ok(Json(
        json!({"took": start.elapsed().as_millis() as u64, "errors": errors, "items": items}),
    ))
}

pub async fn bulk(State(state): State<AppState>, body: Bytes) -> Result<Json<Value>, AppError> {
    run(state, None, body).await
}

pub async fn bulk_index(
    State(state): State<AppState>,
    Path(index): Path<String>,
    body: Bytes,
) -> Result<Json<Value>, AppError> {
    run(state, Some(&index), body).await
}
