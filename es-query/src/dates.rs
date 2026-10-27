use super::*;

/// The current time in epoch millis, for date math's `now`.
pub(crate) fn now_millis() -> i64 {
    let since = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH);
    since.map_or(0, |d| d.as_millis() as i64)
}

/// `q` with the bounds of ranges on date fields of `index` resolved to inclusive epoch millis,
/// like ES's DateFieldType.dateRangeQuery: read with the query's `format` (epoch_millis if a
/// bound is a number), else the field's, in the query's `time_zone`, with date math from `now`;
/// `gt` and `lte` round up. Terms and matches on date fields become such ranges. Phrase
/// prefixes also get the terms their last term expands to.
pub(crate) fn resolve_dates(index: &Index, q: Query, now: i64) -> Result<Query, QueryError> {
    let all = |qs: Vec<Query>| -> Result<Vec<Query>, QueryError> {
        qs.into_iter()
            .map(|c| resolve_dates(index, c, now))
            .collect()
    };
    let ranged = |field: &str| {
        index.mappings.field(field).is_some_and(|m| {
            m.is_range()
                || matches!(
                    m,
                    FieldMapping::Date { .. } | FieldMapping::DateNanos { .. }
                )
        })
    };
    // a term on a date field is the range from the instant to the instant rounded up, and on a
    // range field the ranges that hold the value
    let term = |field: String, v: Value| {
        let range = Query::Range {
            field,
            gt: None,
            gte: Some(Box::new(v.clone())),
            lt: None,
            lte: Some(Box::new(v)),
            format: None,
            time_zone: None,
            relation: Relation::Intersects,
        };
        resolve_dates(index, range, now)
    };
    let ip = |field: &str| matches!(index.mappings.field(field), Some(FieldMapping::Ip));
    Ok(match q {
        Query::Term { field, value } if ip(&field) => ranges::ip_term(field, &value)?,
        Query::Term { field, value } if ranged(&field) => term(field, value)?,
        // non-text fields read a match query as a term query
        Query::Match { field, query, .. } | Query::MatchPhrase { field, query, .. }
            if ranged(&field) || ip(&field) =>
        {
            let value = query.into();
            resolve_dates(index, Query::Term { field, value }, now)?
        }
        Query::MatchPhrasePrefix {
            field,
            query,
            slop,
            max_expansions,
            ..
        } => {
            let (kind, path) = Searcher::unscored(index, &Query::MatchAll).kind(&field);
            if kind != Kind::Text {
                let name = type_name(index, &field, kind);
                return Err(QueryError::illegal(format!(
                    "Can only use phrase prefix queries on text fields - not on [{field}] which is of type [{name}]"
                )));
            }
            let last = query_terms(kind, &query).pop();
            let expansions = last.map_or_else(Vec::new, |last| {
                index.prefixed_terms(&field, path, &last, max_expansions)
            });
            Query::MatchPhrasePrefix {
                field,
                query,
                slop,
                max_expansions,
                expansions,
            }
        }
        Query::Bool {
            must,
            should,
            must_not,
            filter,
            min_should,
        } => Query::Bool {
            must: all(must)?,
            should: all(should)?,
            must_not: all(must_not)?,
            filter: all(filter)?,
            min_should,
        },
        Query::Boosted { boost, query } => Query::Boosted {
            boost,
            query: Box::new(resolve_dates(index, *query, now)?),
        },
        Query::Normalized(mut query) => {
            let text = |field: &str| {
                Searcher::unscored(index, &Query::MatchAll).kind(field).0 == Kind::Text
            };
            // the standard analyzer's normalization
            let lower = |t: &mut String| *t = t.to_lowercase();
            match &mut *query {
                Query::Prefix { field, prefix: t }
                | Query::Wildcard {
                    field, pattern: t, ..
                } if text(field) => lower(t),
                Query::Range {
                    field,
                    gt,
                    gte,
                    lt,
                    lte,
                    ..
                } if text(field) => {
                    for b in [gt, gte, lt, lte].into_iter().flatten() {
                        if let Value::String(t) = &mut **b {
                            lower(t);
                        }
                    }
                }
                _ => {}
            }
            resolve_dates(index, *query, now)?
        }
        Query::Named { name, query } => Query::Named {
            name,
            query: Box::new(resolve_dates(index, *query, now)?),
        },
        Query::Fields {
            fields,
            query,
            tie_breaker,
        } => {
            // ES's resolveMappingFields: a field two entries name takes both boosts
            let mapped = (index.mappings.properties.iter())
                .filter(|(_, m)| !m.is_object())
                .flat_map(|(f, m)| {
                    let subs = m.fields().into_iter().flatten();
                    [f.clone()]
                        .into_iter()
                        .chain(subs.map(move |(s, _)| format!("{f}.{s}")))
                })
                .collect::<Vec<_>>();
            let mut boosts: Vec<(String, f64)> = vec![];
            for (pattern, boost) in &fields {
                let names = match pattern.contains('*') {
                    true => (mapped.iter())
                        .filter(|f| es_index::aliases::glob(pattern, f))
                        .cloned()
                        .collect(),
                    false => vec![pattern.clone()],
                };
                for name in names {
                    match boosts.iter_mut().find(|(f, _)| *f == name) {
                        Some((_, b)) => *b *= boost,
                        None => boosts.push((name, *boost)),
                    }
                }
            }
            let lenient = fields.iter().any(|(f, _)| f == "*");
            let mut queries = vec![];
            for (field, boost) in boosts {
                match resolve_dates(index, query.as_ref().clone().on_field(&field), now) {
                    Ok(q) => queries.push(q.boosted(boost)),
                    Err(_) if lenient => {}
                    Err(e) => return Err(e),
                }
            }
            match queries.len() {
                0 => Query::Bool {
                    must: vec![],
                    should: vec![],
                    must_not: vec![],
                    filter: vec![],
                    min_should: 1,
                },
                1 => queries.remove(0),
                _ => Query::DisMax {
                    queries,
                    tie_breaker,
                },
            }
        }
        Query::DisMax {
            queries,
            tie_breaker,
        } => Query::DisMax {
            queries: all(queries)?,
            tie_breaker,
        },
        Query::Range {
            field,
            gt,
            gte,
            lt,
            lte,
            format,
            time_zone,
            relation,
        } => {
            let mapping = index.mappings.field(&field);
            let (f, locale) = match mapping {
                Some(
                    FieldMapping::Date { format, locale, .. }
                    | FieldMapping::DateNanos { format, locale },
                ) => (format, locale.as_deref()),
                Some(FieldMapping::DateRange { format }) => (format, None),
                Some(m) if m.is_range() || matches!(m, FieldMapping::Ip) => {
                    let bounds = [gt, gte, lt, lte];
                    return ranges::range(
                        m,
                        field,
                        bounds.each_ref().map(|b| b.as_deref()),
                        relation,
                    );
                }
                _ => {
                    return Ok(Query::Range {
                        field,
                        gt,
                        gte,
                        lt,
                        lte,
                        format,
                        time_zone,
                        relation,
                    });
                }
            };
            let number = [&gt, &gte, &lt, &lte]
                .iter()
                .any(|b| b.as_deref().is_some_and(Value::is_number));
            let format = (format.as_deref())
                .or(number.then_some("epoch_millis"))
                .or(f.as_deref());
            let tz = time_zone.unwrap_or(es_index::tz::TimeZone::UTC);
            let millis = |b: Option<Box<Value>>, round_up| {
                let parse = |b: Box<Value>| {
                    es_index::date_math::parse(&text(&b), format, locale, &tz, round_up, now)
                };
                b.map(parse).transpose().map_err(|msg| QueryError {
                    kind: match msg.ends_with(es_index::date_math::TOO_FAR) {
                        true => ErrorKind::IllegalArgument,
                        false => ErrorKind::Parse,
                    },
                    msg,
                })
            };
            // parse_query keeps one of `gt` and `gte`, and of `lt` and `lte`
            let [gt, gte, lt, lte] = [
                millis(gt, true)?,
                millis(gte, false)?,
                millis(lt, false)?,
                millis(lte, true)?,
            ];
            if let Some(m @ FieldMapping::DateRange { .. }) = mapping {
                let bounds = [gt, gte, lt, lte].map(|b| b.map(Value::from));
                return ranges::range(m, field, bounds.each_ref().map(Option::as_ref), relation);
            }
            let lo = gte.or(gt.map(|t| t.saturating_add(1)));
            let hi = lte.or(lt.map(|t| t.saturating_sub(1)));
            Query::Range {
                field,
                gt: None,
                gte: lo.map(|t| Box::new(Value::from(t))),
                lt: None,
                lte: hi.map(|t| Box::new(Value::from(t))),
                format: None,
                time_zone: None,
                relation,
            }
        }
        Query::Prefix { ref field, .. } | Query::Wildcard { ref field, .. } => {
            let kind = Searcher::unscored(index, &Query::MatchAll).kind(field).0;
            let mapping = index.mappings.field(field).filter(|m| !m.is_object());
            let ok = match mapping {
                Some(m) => matches!(
                    m,
                    FieldMapping::Text { .. }
                        | FieldMapping::TextKeyword { .. }
                        | FieldMapping::Keyword { .. }
                ),
                None => matches!(kind, Kind::Text | Kind::Keyword),
            };
            if !ok {
                let what = match q {
                    Query::Prefix { .. } => "prefix",
                    _ => "wildcard",
                };
                let name = type_name(index, field, kind);
                return Err(QueryError::illegal(format!(
                    "Can only use {what} queries on keyword, text and wildcard fields - not on [{field}] which is of type [{name}]"
                )));
            }
            q
        }
        q => q,
    })
}

/// A field's type as ES names it in errors.
fn type_name(index: &Index, field: &str, kind: Kind) -> &'static str {
    // ponytail: unmapped fields are named by the kind they resolve to
    match (index.mappings.field(field), kind) {
        (Some(m), _) => m.type_name(),
        (None, Kind::Number) => "long",
        (None, Kind::Bool) => "boolean",
        (None, _) => "keyword",
    }
}
