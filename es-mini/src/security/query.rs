//! `GET|POST /_security/_query/api_key`: the query DSL over API keys, as Elasticsearch's
//! `RestQueryApiKeyAction` and `TransportQueryApiKeyAction` run it: the request's field names are
//! translated to [`INDEX`]'s (`FieldNameTranslators`), and the search runs there.

use super::{Authn, INDEX, Security, authz, info, validate};
use crate::error::AppError;
use axum::{
    Extension, Json,
    body::Bytes,
    extract::{Query, State},
    http::Uri,
};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::Arc;

const REQUEST: &str = "query_api_key_request_payload";

/// The fields a query names and the [`INDEX`] fields they read, as `FieldNameTranslators`
/// lists them; `metadata` is the flattened `metadata_flattened` (see [`translate`]).
const FIELDS: [(&str, &str); 9] = [
    ("username", "creator.principal"),
    ("realm_name", "creator.realm"),
    ("name", "name"),
    ("type", "type"),
    ("creation", "creation_time"),
    ("expiration", "expiration_time"),
    ("invalidated", "api_key_invalidated"),
    ("invalidation", "invalidation_time"),
    ("metadata", "metadata_flattened"),
];

pub(super) async fn query_keys(
    State(security): State<Arc<Security>>,
    Extension(authn): Extension<Authn>,
    uri: Uri,
    Query(params): Query<HashMap<String, String>>,
    body: Bytes,
) -> Result<Json<Value>, AppError> {
    let allowed = ["with_limited_by", "with_profile_uid", "typed_keys"];
    crate::params::check(&uri, &params, &allowed)?;
    let flag = |k: &str| params.get(k).is_some_and(|v| v.is_empty() || v == "true");
    let body = match body.is_empty() {
        true => json!({}),
        false => crate::docs::parse_json(&body)?,
    };
    let mut search = json!({"track_total_hits": true});
    for (k, v) in body.as_object().into_iter().flatten() {
        let wrong = || {
            let token = es_index::json_token(v);
            AppError::XContentParse(format!(
                "[{REQUEST}] {k} doesn't support values of type: {token}"
            ))
        };
        match k.as_str() {
            // ES parses the query before translating it, so a type it does not know is a
            // parsing_exception, not an unsupported one.
            // ponytail: other parse errors still follow translate_query's, as es-query cannot
            // tell its own gaps (options, types) from ES's errors
            "query" => {
                match es_query::parse_query(v) {
                    Err(e) if unknown_to_es(&e.msg) => return Err(failed_parse(k, e)),
                    _ => {}
                }
                let q = translate_query(v)?;
                es_query::parse_query(&q).map_err(|e| failed_parse(k, e))?;
                search[k] = q;
                continue;
            }
            "from" | "size" if !v.is_i64() => return Err(wrong()),
            "from" | "size" => {}
            "sort" => {
                for s in v.as_array().ok_or_else(wrong)? {
                    sort_form(k, s)?;
                }
            }
            "search_after" if !v.is_array() => return Err(wrong()),
            "search_after" => {}
            "aggs" | "aggregations" if !v.is_object() => return Err(wrong()),
            "aggs" | "aggregations" => {
                // ponytail: es-query has no scripts in aggregations; ES parses them and
                // translate_agg refuses them below
                match es_query::SearchRequest::parse(&json!({"aggs": v})) {
                    Err(e) if !e.msg.ends_with("unknown field [script]") => {
                        return Err(failed_parse(k, e));
                    }
                    _ => {}
                }
                search["aggs"] = v.clone();
                continue;
            }
            _ => {
                return Err(AppError::XContentParse(format!(
                    "[{REQUEST}] unknown field [{k}]"
                )));
            }
        }
        search[k] = v.clone();
    }
    let negative = ["from", "size"].into_iter().filter_map(|k| {
        let n = search[k].as_i64().filter(|n| *n < 0)?;
        Some(format!("[{k}] parameter cannot be negative but was [{n}]"))
    });
    validate(negative.collect())?;
    if body.get("aggs").is_some() && body.get("aggregations").is_some() {
        let outer = format!("Failed to build [{REQUEST}] after last required field arrived");
        let cause = json!({"type": "illegal_argument_exception", "reason": "Duplicate 'aggs' or 'aggregations' field"});
        return Err(AppError::Caused(
            Box::new(AppError::XContentParse(outer)),
            cause,
        ));
    }
    if let Some(aggs) = search.get("aggs").and_then(Value::as_object) {
        search["aggs"] = translate_aggs(aggs)?.into();
    }

    let mut query = json!({"bool": {"filter": [{"term": {"doc_type": "api_key"}}]}});
    if let Authn::ApiKey { id, roles, .. } = &authn
        && !authz::queries_all_keys(roles)
    {
        let filter = query["bool"]["filter"].as_array_mut().expect("built above");
        filter.push(json!({"ids": {"values": [id]}}));
    }
    if let Some(q) = search.get("query") {
        query["bool"]["must"] = json!([q]);
    }
    search["query"] = query;
    if let Some(sort) = search.get("sort") {
        let sort: Vec<Value> = (sort.as_array().into_iter().flatten())
            .map(translate_sort)
            .collect::<Result<_, _>>()?;
        search["sort"] = sort.into();
    }

    let system = security.store.system();
    let Some(index) = system.get(INDEX) else {
        return Ok(Json(json!({"total": 0, "count": 0, "api_keys": []})));
    };
    let mut req = es_query::SearchRequest::parse(&search)?;
    req.typed_keys = flag("typed_keys");
    let r = es_query::search_many(&[(INDEX, 0, index, None)], &req)?;
    let keys: Vec<Value> = (r.hits.iter())
        .map(|h| {
            let mut key = info(h.id, &h.doc.source, flag("with_limited_by"));
            if !h.sort.is_empty() {
                key["_sort"] = h.sort.clone().into();
            }
            key
        })
        .collect();
    let mut out = json!({"total": r.total, "count": keys.len(), "api_keys": keys});
    if let Some(aggs) = r.aggregations {
        out["aggregations"] = aggs;
    }
    Ok(Json(out))
}

/// Elasticsearch's error for body field `k` that failed with `cause` (type, reason).
fn failed_field(k: &str, cause: (&'static str, String)) -> AppError {
    let outer = format!("[{REQUEST}] failed to parse field [{k}]");
    AppError::Chain(vec![("x_content_parse_exception", outer), cause])
}

/// Elasticsearch's error for body field `k` that es-query failed to parse with `e`.
fn failed_parse(k: &str, e: es_query::QueryError) -> AppError {
    let kind = match e.kind {
        es_query::ErrorKind::Parsing => "parsing_exception",
        es_query::ErrorKind::IllegalArgument => "illegal_argument_exception",
        _ => "x_content_parse_exception",
    };
    failed_field(k, (kind, e.msg))
}

/// Whether `msg` is es-query's error for a query type Elasticsearch 9.5.5 does not know either:
/// one es-query lacks that is not in ES's SearchModule or default plugins (as probed on 9.5.5).
fn unknown_to_es(msg: &str) -> bool {
    const KNOWN: &str = "boosting combined_fields dis_max distance_feature exact_knn field_masking_span \
        function_score fuzzy geo_bounding_box geo_distance geo_grid geo_polygon \
        geo_shape has_child has_parent intervals knn knn_score_doc kql match_bool_prefix \
        match_none more_like_this nested parent_id percolate pinned random_sampling \
        rank_feature regexp rule rule_query script script_score semantic shape \
        span_containing span_field_masking span_first span_gap span_multi span_near \
        span_not span_or span_term span_within sparse_vector terms_set text_expansion \
        to_child_block_join weighted_tokens wrapper";
    (msg.strip_prefix("unknown query ["))
        .and_then(|m| m.strip_suffix(']'))
        .is_some_and(|name| !KNOWN.split_whitespace().any(|k| k == name))
}

/// Checks that `s` is a sort the request parser reads: a field name, or `{field: {options}}`.
fn sort_form(k: &str, s: &Value) -> Result<(), AppError> {
    match s {
        Value::String(_) => Ok(()),
        Value::Object(o) => match o.values().next() {
            Some(Value::Object(_)) | None => Ok(()),
            Some(v) => {
                let token = es_index::json_token(v);
                let reason = format!("[field_sort] Expected START_OBJECT but was: {token}");
                Err(failed_field(k, ("x_content_parse_exception", reason)))
            }
        },
        _ => {
            let cause = (
                "illegal_argument_exception",
                "mal-formatted sort object".into(),
            );
            Err(failed_field(k, cause))
        }
    }
}

fn illegal(reason: impl Into<String>) -> AppError {
    AppError::IllegalArgument(reason.into())
}

/// Index field name of query field `field`, as `FieldNameTranslators.translate`.
fn translate(field: &str) -> Result<String, AppError> {
    if field.contains('*') {
        return Err(illegal(format!(
            "Field name pattern [{field}] is not allowed for querying or aggregation"
        )));
    }
    if let Some(sub) = field.strip_prefix("metadata.") {
        // ponytail: es-mini has no flattened type; a metadata subfield reads as an unmapped
        // keyword (`.keyword`), and the whole of `metadata` matches nothing; aggregations,
        // which need a mapping, see it as unmapped
        return Ok(format!("metadata_flattened.{sub}.keyword"));
    }
    match FIELDS.iter().find(|(q, _)| *q == field) {
        Some((_, index)) => Ok((*index).to_owned()),
        None => Err(illegal(format!(
            "Field [{field}] is not allowed for querying or aggregation"
        ))),
    }
}

/// `q` with its field names translated, refusing query types Elasticsearch does not allow
/// here, as `FieldNameTranslators.translateQueryBuilderFields`.
fn translate_query(q: &Value) -> Result<Value, AppError> {
    let Some((kind, body)) = q.as_object().and_then(|o| o.iter().next()) else {
        return Ok(q.clone());
    };
    let mut body = body.clone();
    let renamed = |body: &Value, skip: &[&str]| -> Result<Value, AppError> {
        let Some(body) = body.as_object() else {
            return Ok(body.clone());
        };
        let fields = body.iter().map(|(f, v)| {
            Ok(match skip.contains(&f.as_str()) {
                true => (f.clone(), v.clone()),
                false => (translate(f)?, v.clone()),
            })
        });
        Ok(Value::Object(fields.collect::<Result<_, AppError>>()?))
    };
    let out = match kind.as_str() {
        "match_all" | "ids" => body,
        "bool" => {
            for clause in ["must", "should", "must_not", "filter"] {
                if let Some(c) = body.get_mut(clause) {
                    *c = match &*c {
                        Value::Array(qs) => (qs.iter().map(translate_query))
                            .collect::<Result<Vec<_>, _>>()?
                            .into(),
                        q => translate_query(q)?,
                    };
                }
            }
            body
        }
        "term" | "prefix" | "wildcard" | "match" => renamed(&body, &[])?,
        "terms" => {
            let skip = ["boost", "_name"];
            let lookup = (body.as_object().into_iter().flatten())
                .any(|(f, v)| !skip.contains(&f.as_str()) && v.is_object());
            if lookup {
                return Err(illegal(
                    "terms query with terms lookup is not currently supported in this context",
                ));
            }
            renamed(&body, &skip)?
        }
        "range" => {
            if (body.as_object().into_iter().flatten()).any(|(_, v)| v.get("relation").is_some()) {
                return Err(illegal(
                    "range query with relation is not currently supported in this context",
                ));
            }
            renamed(&body, &[])?
        }
        "exists" => {
            body["field"] = translate(body["field"].as_str().unwrap_or_default())?.into();
            body
        }
        "simple_query_string" => return simple_query_string(body),
        _ => {
            return Err(illegal(format!(
                "Query type [{kind}] is not currently supported in this context"
            )));
        }
    };
    Ok(json!({kind: out}))
}

/// A simple_query_string query with its field patterns translated to the index fields they
/// match (unknown ones dropped), as `FieldNameTranslators`; none left matches nothing.
fn simple_query_string(mut body: Value) -> Result<Value, AppError> {
    let mut fields: Vec<String> = (body["fields"].as_array().into_iter().flatten())
        .filter_map(|f| f.as_str().map(str::to_owned))
        .collect();
    if fields.is_empty() {
        fields.push("*".into());
    }
    // ES turns on `lenient` for `*`: values a field cannot hold match nothing there.
    if fields.iter().any(|f| f == "*") {
        body["lenient"] = true.into();
    }
    let mut out: Vec<(String, f64)> = vec![];
    for f in fields {
        let (pattern, boost) = match f.split_once('^') {
            Some((p, b)) => (p.to_owned(), b.parse().unwrap_or(1.0)),
            None => (f, 1.0),
        };
        for (q, index) in FIELDS {
            let index = match pattern.strip_prefix("metadata.") {
                Some(sub) if q == "metadata" && !pattern.contains('*') => {
                    format!("metadata_flattened.{sub}.keyword")
                }
                _ if es_index::aliases::glob(&pattern, q) => index.to_owned(),
                _ => continue,
            };
            match out.iter_mut().find(|(f, _)| *f == index) {
                Some((_, b)) => *b *= boost,
                None => out.push((index, boost)),
            }
        }
    }
    if out.is_empty() {
        return Ok(json!({"bool": {"must_not": {"match_all": {}}}}));
    }
    let fields: Vec<String> = (out.into_iter())
        .map(|(f, b)| if b == 1.0 { f } else { format!("{f}^{b}") })
        .collect();
    body["fields"] = fields.into();
    Ok(json!({"simple_query_string": body}))
}

/// A sort with its field translated, as `FieldNameTranslators.translateFieldSortBuilders`.
fn translate_sort(s: &Value) -> Result<Value, AppError> {
    let (field, options) = match s {
        Value::String(f) => (f.as_str(), json!({})),
        _ => match s.as_object().and_then(|o| o.iter().next()) {
            Some((f, o)) => (f.as_str(), o.clone()),
            None => return Ok(s.clone()),
        },
    };
    if options.get("nested").is_some() {
        return Err(illegal(
            "nested sorting is not currently supported in this context",
        ));
    }
    if field == "_doc" {
        return Ok(s.clone());
    }
    let mut out = Map::new();
    out.insert(translate(field)?, options);
    Ok(out.into())
}

/// The aggregation's type: its key other than sub-aggregations and `meta`.
fn agg_type(agg: &Value) -> &str {
    let keys = agg.as_object().into_iter().flat_map(|o| o.keys());
    let mut types = keys.filter(|k| !["aggs", "aggregations", "meta"].contains(&k.as_str()));
    types.next().map_or("", String::as_str)
}

/// Pipeline aggregation types, which Elasticsearch refuses here.
fn is_pipeline(t: &str) -> bool {
    t.ends_with("_bucket")
        || [
            "bucket_sort",
            "bucket_script",
            "bucket_selector",
            "bucket_count_ks_test",
            "bucket_correlation",
            "change_point",
            "cumulative_cardinality",
            "cumulative_sum",
            "derivative",
            "inference",
            "moving_fn",
            "moving_percentiles",
            "normalize",
            "serial_diff",
        ]
        .contains(&t)
}

fn unsupported_pipeline(aggs: &Map<String, Value>) -> Result<(), AppError> {
    match aggs.values().map(agg_type).find(|t| is_pipeline(t)) {
        Some(t) => Err(illegal(format!("Unsupported aggregation of type [{t}]"))),
        None => Ok(()),
    }
}

/// `aggs` with their field names translated, refusing what Elasticsearch does not allow here,
/// as `ApiKeyAggregationsBuilder.process`.
fn translate_aggs(aggs: &Map<String, Value>) -> Result<Map<String, Value>, AppError> {
    unsupported_pipeline(aggs)?;
    (aggs.iter())
        .map(|(name, agg)| Ok((name.clone(), translate_agg(name, agg)?)))
        .collect()
}

/// One aggregation translated, its sub-aggregations first (`AggregationBuilder.deepCopy`).
fn translate_agg(name: &str, agg: &Value) -> Result<Value, AppError> {
    let mut agg = agg.clone();
    for k in ["aggs", "aggregations"] {
        let Some(subs) = agg.get(k).and_then(Value::as_object) else {
            continue;
        };
        let mut out = Map::new();
        for (n, sub) in subs.iter().filter(|(_, s)| !is_pipeline(agg_type(s))) {
            out.insert(n.clone(), translate_agg(n, sub)?);
        }
        unsupported_pipeline(subs)?;
        agg[k] = out.into();
    }
    let t = agg_type(&agg).to_owned();
    let body = &mut agg[&t];
    match t.as_str() {
        // ponytail: composite is allowed by ES, but es-query has no composite aggregation
        "terms" | "range" | "date_range" | "missing" | "cardinality" | "value_count" => {
            if body.get("script").is_some() {
                return Err(illegal(format!(
                    "Unsupported script value source for [{name}] agg"
                )));
            }
            if let Some(f) = body.get("field").and_then(Value::as_str) {
                body["field"] = translate(f)?.into();
            }
        }
        "filter" => *body = translate_query(body)?,
        "filters" => match body.get_mut("filters") {
            Some(Value::Array(qs)) => {
                for q in qs {
                    *q = translate_query(q)?;
                }
            }
            Some(Value::Object(qs)) => {
                for q in qs.values_mut() {
                    *q = translate_query(q)?;
                }
            }
            _ => {}
        },
        _ => {
            return Err(illegal(format!(
                "Unsupported API Keys agg [{name}] of type [{t}]"
            )));
        }
    }
    Ok(agg)
}
