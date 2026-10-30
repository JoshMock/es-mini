//! The field capabilities API (`_field_caps`).

use crate::{AppState, error::AppError, indices::parse_body, params};
use axum::{
    Json,
    body::Bytes,
    extract::{Path, Query, State},
    http::Uri,
};
use es_index::{FieldMapping, Index, aliases::glob};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};

const PARAMS: [&str; 8] = [
    "fields",
    "ignore_unavailable",
    "allow_no_indices",
    "expand_wildcards",
    "include_unmapped",
    "filters",
    "types",
    "include_empty_fields",
];

/// Metadata fields every index has: name, type, searchable, aggregatable.
const METADATA: [(&str, &str, bool, bool); 16] = [
    (
        "_data_stream_timestamp",
        "_data_stream_timestamp",
        false,
        false,
    ),
    ("_doc_count", "integer", false, false),
    ("_feature", "_feature", false, false),
    ("_field_names", "_field_names", true, false),
    ("_id", "_id", true, false),
    ("_ignored", "_ignored", true, true),
    ("_ignored_source", "_ignored_source", false, false),
    ("_index", "_index", true, true),
    ("_index_mode", "_index_mode", true, true),
    ("_inference_fields", "_inference_fields", false, false),
    ("_nested_path", "_nested_path", true, false),
    ("_routing", "_routing", true, false),
    ("_seq_no", "_seq_no", true, true),
    ("_source", "_source", false, false),
    ("_tier", "keyword", true, true),
    ("_version", "_version", false, true),
];

pub async fn field_caps_all(
    state: State<AppState>,
    uri: Uri,
    params: Query<HashMap<String, String>>,
    body: Bytes,
) -> Result<Json<Value>, AppError> {
    field_caps(state, Path("_all".into()), uri, params, body).await
}

// ponytail: `index_filter` keeps empty indices and those with a matching document; Elasticsearch only drops those it can prove empty of matches
pub async fn field_caps(
    State(state): State<AppState>,
    Path(name): Path<String>,
    uri: Uri,
    Query(params): Query<HashMap<String, String>>,
    body: Bytes,
) -> Result<Json<Value>, AppError> {
    params::check(&uri, &params, &PARAMS)?;
    let body = parse_body(&body)?;
    let patterns: Vec<String> = match (params.get("fields"), &body["fields"]) {
        (Some(_), f) if !f.is_null() => {
            return Err(AppError::IllegalArgument(
                "can't specify a request body and [fields] request parameter, either specify a request body or the [fields] request parameter".into(),
            ));
        }
        (Some(p), _) => p.split(',').map(str::to_owned).collect(),
        (None, Value::Array(a)) => a
            .iter()
            .filter_map(|f| f.as_str().map(str::to_owned))
            .collect(),
        _ => Vec::new(),
    };
    if patterns.is_empty() {
        return Err(AppError::ActionRequestValidation(
            "no fields specified".into(),
        ));
    }
    let list =
        |k: &str| -> Vec<&str> { params.get(k).map_or(Vec::new(), |v| v.split(',').collect()) };
    let (filters, types) = (list("filters"), list("types"));
    let shared = state.read();
    let resolved = crate::indices::data_targets(&*shared, &name, &params)?;
    let mut names: BTreeSet<String> = resolved.iter().map(|t| t.index.clone()).collect();
    let indices = shared.lock(&names);
    if !body["index_filter"].is_null() {
        let q = es_query::parse_query(&body["index_filter"])
            .map_err(|e| AppError::ParsingException(e.to_string()))?;
        names.retain(|n| {
            let one = [es_index::aliases::Target {
                index: n.clone(),
                filter: None,
            }];
            let split = crate::search::split(&indices, &one);
            let shards = crate::search::targets(&indices, &one, &split);
            es_query::count_many(&shards, &q)
                .map_or(true, |c| c > 0 || indices[n].docs().count() == 0)
        });
    }
    let runtime: Vec<(String, String)> =
        (body["runtime_mappings"].as_object().into_iter().flatten())
            .filter_map(|(f, s)| Some((f.clone(), s["type"].as_str()?.to_owned())))
            .collect();

    // field -> type -> (capabilities, indices having the field with that type)
    let mut found: BTreeMap<String, BTreeMap<String, (Value, Vec<&str>)>> = BTreeMap::new();
    for index in &names {
        for (field, t, metadata, searchable, aggregatable) in
            fields(&indices[index], &filters, &runtime)
        {
            if !patterns.iter().any(|p| glob(p, &field))
                || !(types.is_empty() || types.contains(&t.as_str()))
            {
                continue;
            }
            let caps = cap(&t, metadata, searchable, aggregatable);
            let by_type = found.entry(field).or_default();
            by_type.entry(t).or_insert((caps, Vec::new())).1.push(index);
        }
    }
    if params.get("include_unmapped").is_some_and(|v| v == "true") {
        for by_type in found.values_mut() {
            let mapped: BTreeSet<&str> = by_type.values().flat_map(|(_, i)| i.clone()).collect();
            let unmapped: Vec<&str> = names
                .iter()
                .map(String::as_str)
                .filter(|n| !mapped.contains(n))
                .collect();
            if !unmapped.is_empty() {
                by_type.insert(
                    "unmapped".into(),
                    (cap("unmapped", false, false, false), unmapped),
                );
            }
        }
    }
    let fields: Map<String, Value> = found
        .into_iter()
        .map(|(field, by_type)| {
            let conflicting = by_type.len() > 1;
            let caps = by_type
                .into_iter()
                .map(|(t, (mut caps, indices))| {
                    if conflicting {
                        caps["indices"] = json!(indices);
                    }
                    (t, caps)
                })
                .collect();
            (field, Value::Object(caps))
        })
        .collect();
    Ok(Json(json!({"indices": names, "fields": fields})))
}

fn cap(t: &str, metadata: bool, searchable: bool, aggregatable: bool) -> Value {
    json!({"type": t, "metadata_field": metadata, "searchable": searchable,
        "aggregatable": aggregatable, "inference": false})
}

/// An index's fields as (name, type, metadata, searchable, aggregatable), honoring the `filters` parameter.
fn fields(
    index: &Index,
    filters: &[&str],
    runtime: &[(String, String)],
) -> Vec<(String, String, bool, bool, bool)> {
    let mapped = |name: String, m: &FieldMapping| {
        let t = m.mapping_type();
        let aggregatable = t != "text";
        (name, t, false, true, aggregatable)
    };
    let mut out = Vec::new();
    if !filters.contains(&"-metadata") {
        out.extend(METADATA.map(|(f, t, s, a)| (f.to_owned(), t.to_owned(), true, s, a)));
    }
    // Like Elasticsearch, objects without subfields are not reported.
    for (name, m) in &index.mappings.properties {
        if *m == FieldMapping::Object {
            continue;
        }
        out.push(mapped(name.clone(), m));
        if let FieldMapping::TextKeyword { fields } = m
            && !filters.contains(&"-multifield")
        {
            out.extend(
                fields
                    .iter()
                    .map(|(sub, m)| mapped(format!("{name}.{sub}"), m)),
            );
        }
    }
    // Runtime fields replace mapped fields of the same name.
    out.retain(|(n, ..)| !runtime.iter().any(|(r, _)| r == n));
    out.extend(
        runtime
            .iter()
            .map(|(n, t)| (n.clone(), t.clone(), false, true, true)),
    );
    out
}
