use crate::{
    AppState,
    docs::{delete_doc, force_refresh, parse_json, put_or_create, update_doc},
    error::AppError,
    params,
};
use axum::{
    Json,
    body::Bytes,
    extract::{Path, Query, State},
    http::Uri,
};
use es_index::{aliases, auto_id};
use serde_json::{Value, json};
use std::{collections::HashMap, time::Instant};

struct Action {
    op: String,
    index: String,
    id: Option<String>,
    body: Option<Value>,
    require_alias: bool,
}

/// Parses the whole NDJSON body up front, so a malformed request executes nothing.
fn parse(
    body: &[u8],
    default_index: Option<&str>,
    require_alias: bool,
) -> Result<Vec<Action>, AppError> {
    if body.last().is_some_and(|b| *b != b'\n') {
        return Err(AppError::IllegalArgument(
            "The bulk request must be terminated by a newline [\\n]".into(),
        ));
    }
    let mut lines = body
        .split(|b| *b == b'\n')
        .filter(|l| !l.trim_ascii().is_empty())
        .enumerate();
    let mut actions = Vec::new();
    while let Some((n, line)) = lines.next() {
        let meta = parse_json(line)?;
        let Some((op, meta)) = meta
            .as_object()
            .filter(|m| m.len() == 1)
            .and_then(|m| m.iter().next())
        else {
            if meta.as_object().is_some_and(|m| m.is_empty()) {
                return Err(AppError::IllegalArgument(format!(
                    "Malformed action/metadata line [{}], expected FIELD_NAME but found [END_OBJECT]",
                    n + 1
                )));
            }
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
            "create"
        } else {
            op.as_str()
        };
        let body = match op {
            "delete" => None,
            _ => {
                // Like Elasticsearch, an action line with no source line is dropped.
                let Some((_, line)) = lines.next() else { break };
                Some(parse_json(line)?)
            }
        };
        actions.push(Action {
            op: op.to_owned(),
            index: index.to_owned(),
            id,
            body,
            require_alias: meta["require_alias"].as_bool().unwrap_or(require_alias),
        });
    }
    if actions.is_empty() {
        return Err(AppError::ActionRequestValidation(
            "no requests added".into(),
        ));
    }
    Ok(actions)
}

fn resolve_alias(state: &AppState, name: &str) -> Result<String, AppError> {
    aliases::resolve_write(&*state.read(), name).map_err(AppError::from)
}

fn is_alias(state: &AppState, name: &str) -> bool {
    aliases::is_alias(&*state.read(), name)
}

fn execute(state: &AppState, params: &HashMap<String, String>, a: Action) -> Value {
    let resolved = if a.require_alias && !is_alias(state, &a.index) {
        Err(AppError::RequireAlias(a.index.clone()))
    } else {
        resolve_alias(state, &a.index)
    };
    let target = resolved.as_deref().unwrap_or(&a.index).to_owned();
    let (id, ts) = a.id.map_or_else(auto_id, |id| (id, -1));
    let body = a.body.unwrap_or(Value::Null);
    let result = (|| {
        resolved?;
        if id.is_empty() {
            return Err(AppError::IllegalArgument(
                "if _id is specified it must not be empty".into(),
            ));
        }
        let mut reply = match a.op.as_str() {
            "index" => put_or_create(state, &target, &id, body, false, ts),
            "create" => put_or_create(state, &target, &id, body, true, ts),
            "update" => update_doc(state, &target, &id, &body),
            _ => delete_doc(state, &target, &id),
        }?;
        force_refresh(state, params, &target, &mut reply)?;
        Ok(reply)
    })();
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
    params: &HashMap<String, String>,
    body: Bytes,
) -> Result<Json<Value>, AppError> {
    let start = Instant::now();
    let items: Vec<Value> = parse(&body, default_index, flag(params))?
        .into_iter()
        .map(|a| execute(&state, params, a))
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

fn flag(params: &HashMap<String, String>) -> bool {
    params.get("require_alias").is_some_and(|v| v == "true")
}

const BULK_PARAMS: [&str; 12] = [
    "_source",
    "_source_excludes",
    "_source_includes",
    "include_source_on_error",
    "list_executed_pipelines",
    "pipeline",
    "refresh",
    "require_alias",
    "require_data_stream",
    "routing",
    "timeout",
    "wait_for_active_shards",
];

pub async fn bulk(
    State(state): State<AppState>,
    uri: Uri,
    Query(params): Query<HashMap<String, String>>,
    body: Bytes,
) -> Result<Json<Value>, AppError> {
    params::check(&uri, &params, &BULK_PARAMS)?;
    run(state, None, &params, body).await
}

pub async fn bulk_index(
    State(state): State<AppState>,
    Path(index): Path<String>,
    uri: Uri,
    Query(params): Query<HashMap<String, String>>,
    body: Bytes,
) -> Result<Json<Value>, AppError> {
    params::check(&uri, &params, &BULK_PARAMS)?;
    run(state, Some(&index), &params, body).await
}

#[cfg(test)]
mod tests {
    use super::parse;

    fn err(body: &str) -> String {
        match parse(body.as_bytes(), Some("i"), false) {
            Err(e) => e.parts().1["reason"].as_str().unwrap().to_owned(),
            Ok(_) => "ok".into(),
        }
    }

    #[test]
    fn empty_body_has_no_requests() {
        assert_eq!(err(""), "Validation Failed: 1: no requests added;");
    }

    #[test]
    fn unterminated_last_line() {
        let want = "The bulk request must be terminated by a newline [\\n]";
        assert_eq!(err("{\"delete\":{\"_id\":\"1\"}}"), want);
        assert_eq!(err("{\"index\":{}}\n{\"a\":1}"), want);
    }

    #[test]
    fn terminated_request_parses() {
        assert_eq!(err("{\"index\":{}}\n{\"a\":1}\n"), "ok");
    }
}
