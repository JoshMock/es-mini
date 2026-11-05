use super::*;

/// The error of a failed [`es_index::date_math::parse`].
pub(crate) fn date_math_error(msg: String) -> QueryError {
    let kind = match msg.ends_with(es_index::date_math::TOO_FAR) {
        true => ErrorKind::IllegalArgument,
        false => ErrorKind::Parse,
    };
    QueryError::new(kind, msg)
}

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
        Query::Invalid(mut e) => {
            let uuid = index.settings.get(es_index::index_settings::UUID);
            e.index_uuid = uuid.map(str::to_owned);
            return Err(*e);
        }
        Query::Term { field, value } if field == "_routing" => routing_term(index, text(&value)),
        Query::Term { field, value } if field == "_ignored" => Query::Ignored(text(&value)),
        Query::Match { field, query, .. } if field == "_ignored" => Query::Ignored(query),
        // as ES's unmapped fields, which query as MatchNoDocsQuery
        // ponytail: `_`-prefixed names are taken as metadata fields, so unmapped `_x` still
        // reads `_source`; list ES's metadata fields if one needs it
        Query::Match { field, .. }
        | Query::MatchPhrase { field, .. }
        | Query::MatchPhrasePrefix { field, .. }
        | Query::Term { field, .. }
        | Query::Prefix { field, .. }
        | Query::Wildcard { field, .. }
        | Query::Exists { field }
        | Query::Range { field, .. }
            if !field.starts_with('_') && index.mappings.unindexed(&field) =>
        {
            query_string::match_none()
        }
        Query::TermCaseInsensitive { field, value } => {
            let kind = Searcher::unscored(index, &Query::MatchAll).kind(&field).0;
            let q = match (
                index.mappings.field(&field).filter(|m| !m.is_object()),
                kind,
            ) {
                (Some(FieldMapping::Boolean), _) | (None, Kind::Bool) => {
                    Query::Term { field, value }
                }
                // ES's case-insensitive term is a constant-score automaton query, as a wildcard's
                (Some(FieldMapping::Text { .. } | FieldMapping::Keyword { .. }), _)
                | (None, Kind::Text | Kind::Keyword(_)) => Query::Wildcard {
                    pattern: crate::parse::wildcard_escape(&text(&value)),
                    field,
                    case_insensitive: true,
                },
                _ => {
                    return Err(QueryError::illegal(format!(
                        "[{field}] field which is of type [{}], does not support case insensitive term queries",
                        type_name(index, &field, kind)
                    )));
                }
            };
            resolve_dates(index, q, now)?
        }
        Query::Term { field, value } if ip(&field) => ranges::ip_term(field, &value)?,
        Query::Term { field, value } if ranged(&field) => term(field, value)?,
        // non-text fields read a match query as a term query
        Query::Match { field, query, .. } | Query::MatchPhrase { field, query, .. }
            if ranged(&field) || ip(&field) =>
        {
            let value = query.into();
            resolve_dates(index, Query::Term { field, value }, now)?
        }
        // MatchQueryParser.analyzeMultiBoolean
        Query::MatchBoolPrefix { field, query, and } => {
            let searcher = Searcher::unscored(index, &Query::MatchAll);
            let kind = searcher.kind(&field).0;
            let mut terms = searcher.match_terms(&field, &query);
            // the last term is a prefix if no text follows it
            let prefix = match kind {
                Kind::Text => es_index::analysis::standard_tokenize(&query)
                    .last()
                    .copied(),
                _ => terms.last().map(String::as_str),
            }
            .is_some_and(|t| query.ends_with(t))
            .then(|| terms.pop())
            .flatten();
            let mut clauses: Vec<Query> = (terms.into_iter())
                .map(|t| Query::Term {
                    field: field.clone(),
                    value: Value::String(t),
                })
                .collect();
            clauses.extend(prefix.map(|prefix| Query::Prefix {
                field: field.clone(),
                prefix,
            }));
            let q = match clauses.len() {
                1 => clauses.remove(0),
                _ => query_string::boolean_of(and, clauses),
            };
            resolve_dates(index, q, now)?
        }
        Query::MatchPhrasePrefix {
            field,
            query,
            slop,
            max_expansions,
            ..
        } => {
            let searcher = Searcher::unscored(index, &Query::MatchAll);
            let (kind, path) = searcher.kind(&field);
            if kind != Kind::Text {
                let name = type_name(index, &field, kind);
                return Err(QueryError::illegal(format!(
                    "Can only use phrase prefix queries on text fields - not on [{field}] which is of type [{name}]"
                )));
            }
            let last = query_terms(kind, &query).pop();
            let expansions = last.map_or_else(Vec::new, |last| {
                searcher
                    .shard
                    .prefixed_terms(&field, path, &last, max_expansions)
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
            lenient,
        } => {
            let lenient = lenient || fields.iter().any(|(f, _)| f == "*");
            let mut queries = vec![];
            for (field, boost) in field_boosts(index, &fields) {
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
        // MultiMatchQueryParser.buildCrossFieldQuery
        Query::CrossFields {
            fields,
            query,
            and,
            tie_breaker,
        } => {
            let lenient = fields.iter().any(|(f, _)| f == "*");
            let searcher = Searcher::unscored(index, &Query::MatchAll);
            // the fields grouped by search analyzer: standard, whitespace or keyword
            let mut groups: Vec<(u8, Vec<(String, f64)>)> = vec![];
            for (field, boost) in field_boosts(index, &fields) {
                if index.mappings.unindexed(&field) {
                    continue;
                }
                let analyzer = match index.mappings.properties.get(&field) {
                    Some(FieldMapping::Keyword {
                        split_queries_on_whitespace: true,
                        ..
                    }) => 1,
                    _ if searcher.kind(&field).0 == Kind::Text => 0,
                    _ => 2,
                };
                match groups.iter_mut().find(|g| g.0 == analyzer) {
                    Some(g) => g.1.push((field, boost)),
                    None => groups.push((analyzer, vec![(field, boost)])),
                }
            }
            // blendTerms: a term on text and keyword fields is blended, on others a query
            // beside the blended one
            let blend = |term: &str, group: &[(String, f64)]| {
                let (mut queries, mut blended) = (vec![], vec![]);
                for (field, boost) in group {
                    let q = Query::Term {
                        field: field.clone(),
                        value: term.into(),
                    };
                    match searcher.kind(field).0 {
                        Kind::Text | Kind::Keyword(_) => blended.push(q.boosted(*boost)),
                        _ => match resolve_dates(index, q, now) {
                            Ok(q) => queries.push(q.boosted(*boost)),
                            Err(_) if lenient => {}
                            Err(e) => return Err(e),
                        },
                    }
                }
                if !blended.is_empty() {
                    queries.push(Query::Blended(Box::new(Query::DisMax {
                        queries: blended,
                        tie_breaker,
                    })));
                }
                Ok(match queries.len() {
                    1 => queries.remove(0),
                    _ => Query::DisMax {
                        queries,
                        tie_breaker,
                    },
                })
            };
            let mut queries = vec![];
            for (_, group) in groups {
                let mut clauses = (searcher.match_terms(&group[0].0, &query).iter())
                    .map(|t| blend(t, &group))
                    .collect::<Result<Vec<_>, _>>()?;
                queries.push(match clauses.len() {
                    0 => query_string::match_none(),
                    1 => clauses.remove(0),
                    _ => query_string::boolean_of(and, clauses),
                });
            }
            match queries.len() {
                0 => query_string::match_none(),
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
                b.map(parse).transpose().map_err(date_math_error)
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
                Some(m) => matches!(m, FieldMapping::Text { .. } | FieldMapping::Keyword { .. }),
                None => matches!(kind, Kind::Text | Kind::Keyword(_)),
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

/// The fields `fields` names, `*` patterns expanded to the mapped fields they match, with their
/// boosts: ES's resolveMappingFields, where a field two entries name takes both boosts.
fn field_boosts(index: &Index, fields: &[(String, f64)]) -> Vec<(String, f64)> {
    let mapped = index.mappings.names();
    let mut boosts: Vec<(String, f64)> = vec![];
    for (pattern, boost) in fields {
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
    boosts
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

/// A `term` query on `_routing`, scored as Lucene's BM25 scores a match of the shard's
/// `_routing` field, one term per document.
fn routing_term(index: &Index, value: String) -> Query {
    let routings: Vec<_> = (index.docs())
        .filter_map(|(_, d)| d.routing.clone())
        .collect();
    let n = routings.len() as f64;
    let df = routings.iter().filter(|r| **r == value).count() as f64;
    Query::Routing(value).boosted(f64::from(no_norms_score(n, df, 1.0)))
}

/// Lucene's BM25 score, under ES's LegacyBM25Similarity, of a term matched once in a field
/// without norms (so every length is 1) that `n` documents hold, `df` of them the term, with
/// `avg_len` terms on average.
pub(crate) fn no_norms_score(n: f64, df: f64, avg_len: f32) -> f32 {
    let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln() as f32;
    let (k1, b) = (1.2f32, 0.75f32);
    let weight = (1.0 + k1) * idf;
    let norm_inverse = 1.0 / (k1 * ((1.0 - b) + b / avg_len));
    weight - weight / (1.0 + norm_inverse)
}
