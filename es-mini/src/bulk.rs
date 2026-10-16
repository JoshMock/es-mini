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

fn execute(state: &AppState, a: Action) -> Value {
    let id = a.id.unwrap_or_else(auto_id);
    let body = a.body.unwrap_or(Value::Null);
    let result = match a.op.as_str() {
        "index" => put_or_create(state, &a.index, &id, body, false),
        "create" => put_or_create(state, &a.index, &id, body, true),
        "update" => update_doc(state, &a.index, &id, &body),
        _ => delete_doc(state, &a.index, &id),
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
