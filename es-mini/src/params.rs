//! Query-parameter validation, as Java's `BaseRestHandler` does it.

use crate::error::AppError;
use axum::http::Uri;
use std::collections::HashMap;

/// Parameters every REST handler accepts (`RestController` and `RestRequest` consume them).
const COMMON: [&str; 7] = [
    "pretty",
    "human",
    "error_trace",
    "source",
    "source_content_type",
    "filter_path",
    "format",
];

/// Rejects any parameter that is neither common nor in `allowed`.
pub fn check(
    uri: &Uri,
    params: &HashMap<String, String>,
    allowed: &[&str],
) -> Result<(), AppError> {
    let mut unknown: Vec<&str> = params
        .keys()
        .map(String::as_str)
        .filter(|k| !COMMON.contains(k) && !allowed.contains(k))
        .collect();
    unknown.sort_unstable();
    match unknown.as_slice() {
        [] => Ok(()),
        [one] => Err(AppError::IllegalArgument(format!(
            "request [{}] contains unrecognized parameter: [{one}]",
            uri.path()
        ))),
        many => Err(AppError::IllegalArgument(format!(
            "request [{}] contains unrecognized parameters: [{}]",
            uri.path(),
            many.join("], [")
        ))),
    }
}
