use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;

/// Errors rendered in the ES `{error: {root_cause, type, reason, ...}, status}` envelope.
#[derive(Debug)]
pub enum AppError {
    IndexNotFound(String),
    IndexExists(String),
    Parse(String),
    MapperParsing(String),
    VersionConflict { id: String, current_version: u64 },
    DocumentMissing(String),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, kind, reason, index) = match self {
            Self::IndexNotFound(i) => (
                StatusCode::NOT_FOUND,
                "index_not_found_exception",
                format!("no such index [{i}]"),
                Some(i),
            ),
            Self::IndexExists(i) => (
                StatusCode::BAD_REQUEST,
                "resource_already_exists_exception",
                format!("index [{i}/_na_] already exists"),
                Some(i),
            ),
            Self::Parse(r) => (StatusCode::BAD_REQUEST, "parse_exception", r, None),
            Self::MapperParsing(r) => {
                (StatusCode::BAD_REQUEST, "mapper_parsing_exception", r, None)
            }
            Self::VersionConflict {
                id,
                current_version,
            } => (
                StatusCode::CONFLICT,
                "version_conflict_engine_exception",
                format!(
                    "[{id}]: version conflict, document already exists (current version [{current_version}])"
                ),
                None,
            ),
            Self::DocumentMissing(id) => (
                StatusCode::NOT_FOUND,
                "document_missing_exception",
                format!("[{id}]: document missing"),
                None,
            ),
        };
        let mut cause = json!({"type": kind, "reason": reason});
        if let Some(i) = index {
            cause["index_uuid"] = "_na_".into();
            cause["index"] = i.into();
        }
        let mut error = cause.clone();
        error["root_cause"] = json!([cause]);
        (
            status,
            Json(json!({"error": error, "status": status.as_u16()})),
        )
            .into_response()
    }
}
