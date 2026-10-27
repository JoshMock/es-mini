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
    let date = |field: &str| {
        matches!(
            index.mappings.field(field),
            Some(FieldMapping::Date { .. } | FieldMapping::DateNanos { .. })
        )
    };
    // a term on a date field is the range from the instant to the instant rounded up
    let term = |field: String, v: Value| {
        let range = Query::Range {
            field,
            gt: None,
            gte: Some(Box::new(v.clone())),
            lt: None,
            lte: Some(Box::new(v)),
            format: None,
            time_zone: None,
        };
        resolve_dates(index, range, now)
    };
    Ok(match q {
        Query::Term { field, value } if date(&field) => term(field, value)?,
        Query::Match { field, query, .. } | Query::MatchPhrase { field, query, .. }
            if date(&field) =>
        {
            term(field, query.into())?
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
            if let Query::Prefix { field, prefix: t }
            | Query::Wildcard {
                field, pattern: t, ..
            } = &mut *query
                && Searcher::unscored(index, &Query::MatchAll).kind(field).0 == Kind::Text
            {
                // the standard analyzer's normalization
                *t = t.to_lowercase();
            }
            resolve_dates(index, *query, now)?
        }
        Query::Named { name, query } => Query::Named {
            name,
            query: Box::new(resolve_dates(index, *query, now)?),
        },
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
        } => {
            let (Some(FieldMapping::Date {
                format: f, locale, ..
            })
            | Some(FieldMapping::DateNanos { format: f, locale })) = index.mappings.field(&field)
            else {
                return Ok(Query::Range {
                    field,
                    gt,
                    gte,
                    lt,
                    lte,
                    format,
                    time_zone,
                });
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
                    es_index::date_math::parse(
                        &text(&b),
                        format,
                        locale.as_deref(),
                        &tz,
                        round_up,
                        now,
                    )
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
            let lo = millis(gte, false)?.or(millis(gt, true)?.map(|t| t.saturating_add(1)));
            let hi = millis(lte, true)?.or(millis(lt, false)?.map(|t| t.saturating_sub(1)));
            Query::Range {
                field,
                gt: None,
                gte: lo.map(|t| Box::new(Value::from(t))),
                lt: None,
                lte: hi.map(|t| Box::new(Value::from(t))),
                format: None,
                time_zone: None,
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
