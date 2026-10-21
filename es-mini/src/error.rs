use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};

/// Errors rendered in the ES `{error: {root_cause, type, reason, ...}, status}` envelope.
#[derive(Debug)]
pub enum AppError {
    IndexNotFound(String),
    /// Write to a non-alias with `require_alias` set.
    RequireAlias(String),
    IndexExists(String),
    IndexClosed(String),
    /// Removing or reading aliases that do not exist.
    AliasesNotFound(Vec<String>),
    InvalidAliasName {
        alias: String,
        reason: String,
    },
    InvalidIndexName {
        index: String,
        reason: String,
    },
    Parse(String),
    MapperParsing(String),
    DocumentParsing(String),
    VersionConflict {
        index: String,
        id: String,
        current_version: u64,
    },
    DocumentMissing {
        index: String,
        id: String,
    },
    ActionRequestValidation(String),
    ResourceNotFound(String),
    IllegalArgument(String),
    ParsingException(String),
    Verification(String),
}

impl AppError {
    /// Status and the `error` object (with `root_cause`) of the ES envelope.
    pub(crate) fn parts(self) -> (StatusCode, Value) {
        let (status, kind, reason, index) = match self {
            Self::IndexClosed(i) => (
                StatusCode::BAD_REQUEST,
                "index_closed_exception",
                "closed".into(),
                Some(i),
            ),
            Self::AliasesNotFound(a) => (
                StatusCode::NOT_FOUND,
                "aliases_not_found_exception",
                format!("aliases [{}] missing", a.join(",")),
                None,
            ),
            Self::InvalidAliasName { alias, reason } => (
                StatusCode::BAD_REQUEST,
                "invalid_alias_name_exception",
                format!("Invalid alias name [{alias}]: {reason}"),
                None,
            ),
            Self::InvalidIndexName { index, reason } => (
                StatusCode::BAD_REQUEST,
                "invalid_index_name_exception",
                format!("Invalid index name [{index}], {reason}"),
                Some(index),
            ),
            Self::IndexNotFound(i) => (
                StatusCode::NOT_FOUND,
                "index_not_found_exception",
                format!("no such index [{i}]"),
                Some(i),
            ),
            Self::RequireAlias(i) => (
                StatusCode::NOT_FOUND,
                "index_not_found_exception",
                format!(
                    "no such index [{i}] and [require_alias] request flag is [true] and [{i}] is not an alias"
                ),
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
            Self::DocumentParsing(r) => (
                StatusCode::BAD_REQUEST,
                "document_parsing_exception",
                r,
                None,
            ),
            Self::VersionConflict {
                index,
                id,
                current_version,
            } => (
                StatusCode::CONFLICT,
                "version_conflict_engine_exception",
                format!(
                    "[{id}]: version conflict, document already exists (current version [{current_version}])"
                ),
                Some(index),
            ),
            Self::DocumentMissing { index, id } => (
                StatusCode::NOT_FOUND,
                "document_missing_exception",
                format!("[{id}]: document missing"),
                Some(index),
            ),
            Self::ActionRequestValidation(r) => (
                StatusCode::BAD_REQUEST,
                "action_request_validation_exception",
                format!("Validation Failed: 1: {r};"),
                None,
            ),
            Self::ResourceNotFound(r) => (
                StatusCode::NOT_FOUND,
                "resource_not_found_exception",
                r,
                None,
            ),
            Self::ParsingException(r) => (StatusCode::BAD_REQUEST, "parsing_exception", r, None),
            Self::Verification(r) => (StatusCode::BAD_REQUEST, "verification_exception", r, None),
            Self::IllegalArgument(r) => (
                StatusCode::BAD_REQUEST,
                "illegal_argument_exception",
                r,
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
        (status, error)
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, error) = self.parts();
        (
            status,
            Json(json!({"error": error, "status": status.as_u16()})),
        )
            .into_response()
    }
}

impl From<es_index::aliases::ResolveError> for AppError {
    fn from(e: es_index::aliases::ResolveError) -> Self {
        use es_index::aliases::ResolveError;
        match e {
            ResolveError::IndexNotFound(i) => Self::IndexNotFound(i),
            ResolveError::IndexClosed(i) => Self::IndexClosed(i),
            ResolveError::InvalidIndexName(index) => Self::InvalidIndexName {
                index,
                reason: "must not start with '_'.".into(),
            },
            ResolveError::NoWriteIndex(a) => Self::IllegalArgument(format!(
                "no write index is defined for alias [{a}]. The write index may be explicitly disabled using is_write_index=false or the alias points to multiple indices without one being designated as a write index"
            )),
        }
    }
}

impl From<es_query::QueryError> for AppError {
    fn from(e: es_query::QueryError) -> Self {
        match e.kind {
            es_query::ErrorKind::Parsing => Self::ParsingException(e.msg),
            es_query::ErrorKind::IllegalArgument => Self::IllegalArgument(e.msg),
        }
    }
}
