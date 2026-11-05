//! Elasticsearch's shard-level query rewrite, as far as it decides whether a query can match
//! (SearchService.queryStillMatchesAfterRewrite): the query builders that rewrite to
//! `match_none` or `match_all` without reading documents, and date ranges, which read the
//! shard's smallest and largest value.

use crate::{Query, Target, dates::resolve_dates, now_millis};
use es_index::{FieldMapping, aliases::glob, date_millis, postings::values};
use serde_json::Value;

#[derive(PartialEq)]
enum Rewritten {
    None,
    All,
    Other,
}

/// Whether `q` may match on target `t`: false where ES rewrites it to `match_none` there, as
/// field_caps' `index_filter` checks.
pub fn can_match(t: &Target, q: &Query) -> bool {
    rewrite(t, q, now_millis()) != Rewritten::None
}

fn rewrite(t: &Target, q: &Query, now: i64) -> Rewritten {
    let &(name, _, index, _) = t;
    let m = &index.mappings;
    // an index without mappings has no field types, not even metadata ones. ponytail: so does
    // one created with empty mappings, which es-mini does not tell apart
    let no_mappings = *m == Default::default();
    let mapped = |f: &str| {
        !no_mappings && (f.starts_with('_') || m.field(f).is_some() || m.runtime.contains_key(f))
    };
    // _index and _tier are ConstantFieldTypes: a query on them matches all or none
    let constant = |f: &str| match f {
        "_index" => Some(name),
        _ => (index.settings)
            .get("index.routing.allocation.include._tier_preference")
            .unwrap_or("data_content")
            .split(',')
            .next()
            .map(str::trim)
            .filter(|t| !t.is_empty()),
    };
    let leaf = |f: &str, pattern: String| match f {
        _ if !mapped(f) => Rewritten::None,
        "_index" | "_tier" if constant(f).is_some_and(|v| glob(&pattern, v)) => Rewritten::All,
        "_index" | "_tier" => Rewritten::None,
        _ => Rewritten::Other,
    };
    let text = es_index::postings::text;
    match q {
        Query::MatchAll => Rewritten::All,
        Query::Term { field, value } => leaf(field, text(value)),
        Query::TermCaseInsensitive { field, value } => {
            leaf(field, text(value).to_ascii_lowercase())
        }
        Query::Prefix { field, prefix } => leaf(field, format!("{prefix}*")),
        Query::Wildcard {
            field,
            pattern,
            case_insensitive,
        } => leaf(
            field,
            match case_insensitive {
                true => pattern.to_ascii_lowercase(),
                false => pattern.clone(),
            },
        ),
        Query::Exists { field } => {
            let under = |k: &String| glob(field, k) || k.starts_with(&format!("{field}."));
            match mapped(field) || m.properties.keys().chain(m.runtime.keys()).any(under) {
                true => Rewritten::Other,
                false => Rewritten::None,
            }
        }
        Query::Range { field, .. } if !mapped(field) => Rewritten::None,
        Query::Range { field, .. } => match m.field(field) {
            Some(
                FieldMapping::Date { format, locale, .. }
                | FieldMapping::DateNanos { format, locale },
            ) => {
                // DateFieldType.isFieldWithinQuery; a bound that fails to parse leaves it be
                let Ok(Query::Range { gte, lte, .. }) = resolve_dates(index, q.clone(), now) else {
                    return Rewritten::Other;
                };
                let millis = (index.docs())
                    .flat_map(|(_, d)| {
                        (values(&d.source, field).into_iter())
                            .filter_map(|v| date_millis(format.as_deref(), locale.as_deref(), v))
                            .collect::<Vec<_>>()
                    })
                    .fold(None, |r: Option<(i64, i64)>, v| {
                        Some(r.map_or((v, v), |(lo, hi)| (lo.min(v), hi.max(v))))
                    });
                let bound = |b: Option<Box<Value>>| b.and_then(|b| b.as_i64());
                let (from, to) = (bound(gte), bound(lte));
                match millis {
                    Some((lo, hi))
                        if from.is_none_or(|f| hi >= f) && to.is_none_or(|t| lo <= t) =>
                    {
                        Rewritten::Other
                    }
                    _ => Rewritten::None,
                }
            }
            _ => Rewritten::Other,
        },
        Query::Bool {
            must,
            should,
            must_not,
            filter,
            min_should,
        } => {
            if must.is_empty() && should.is_empty() && must_not.is_empty() && filter.is_empty() {
                // es-query's match_none, and an empty terms or ids query, require a should
                return match min_should {
                    0 => Rewritten::All,
                    _ => Rewritten::None,
                };
            }
            let any = |qs: &[Query], r| qs.iter().any(|c| rewrite(t, c, now) == r);
            let only_should = must.is_empty() && filter.is_empty() && must_not.is_empty();
            if (only_should && (should.iter()).all(|c| rewrite(t, c, now) == Rewritten::None))
                || any(must, Rewritten::None)
                || any(filter, Rewritten::None)
                || any(must_not, Rewritten::All)
            {
                return Rewritten::None;
            }
            Rewritten::Other
        }
        Query::Boosted { query, .. } | Query::Named { query, .. } => rewrite(t, query, now),
        _ => Rewritten::Other,
    }
}
