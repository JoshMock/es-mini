//! Query DSL parsing.

use crate::{QueryError, text};
use es_index::tz::TimeZone;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub enum Query {
    MatchAll,
    Match {
        field: String,
        query: String,
        and: bool,
    },
    /// The analyzed terms in order, at most `slop` position moves apart in all.
    MatchPhrase {
        field: String,
        query: String,
        slop: u32,
    },
    /// A phrase whose last term is any of the first `max_expansions` indexed terms it prefixes:
    /// `expansions`, filled in for each index searched (see `resolve_dates`).
    MatchPhrasePrefix {
        field: String,
        query: String,
        slop: u32,
        max_expansions: usize,
        expansions: Vec<String>,
    },
    Term {
        field: String,
        value: Value,
    },
    Prefix {
        field: String,
        prefix: String,
    },
    Wildcard {
        field: String,
        pattern: String,
        case_insensitive: bool,
    },
    Exists {
        field: String,
    },
    Range {
        field: String,
        gt: Option<Box<Value>>,
        gte: Option<Box<Value>>,
        lt: Option<Box<Value>>,
        lte: Option<Box<Value>>,
        /// The date format the bounds are in, instead of the field's.
        format: Option<String>,
        /// The time zone of date bounds without an offset.
        time_zone: Option<TimeZone>,
    },
    Bool {
        must: Vec<Query>,
        should: Vec<Query>,
        must_not: Vec<Query>,
        filter: Vec<Query>,
        min_should: usize,
    },
    /// Lucene's DisjunctionMaxQuery: documents any of `queries` match, scored by the best
    /// score plus `tie_breaker` times the others'.
    DisMax {
        queries: Vec<Query>,
        tie_breaker: f64,
    },
    /// A query given a `_name`, which hits report in `matched_queries`.
    /// A query whose scores are multiplied by `boost`.
    Boosted {
        boost: f64,
        query: Box<Query>,
    },
    Named {
        name: String,
        query: Box<Query>,
    },
    /// A query_string prefix, wildcard or range whose text the field's search analyzer
    /// normalizes; `resolve_dates` replaces it with the query it wraps, normalized.
    Normalized(Box<Query>),
}

/// The single `{field: params}` entry of a leaf query.
fn single_field<'a>(kind: &str, v: &'a Value) -> Result<(&'a String, &'a Value), String> {
    match v
        .as_object()
        .filter(|m| m.len() == 1)
        .and_then(|m| m.iter().next())
    {
        Some(entry) => Ok(entry),
        None => Err(format!("[{kind}] query malformed, expected a single field")),
    }
}

const MATCH_KEYS: &[&str] = &[
    "query",
    "operator",
    "minimum_should_match",
    "analyzer",
    "fuzziness",
    "prefix_length",
    "max_expansions",
    "fuzzy_transpositions",
    "fuzzy_rewrite",
    "lenient",
    "zero_terms_query",
    "auto_generate_synonyms_phrase_query",
    "cutoff_frequency",
    "boost",
    "_name",
];

fn case_insensitive(o: &serde_json::Map<String, Value>) -> bool {
    o.get("case_insensitive").and_then(Value::as_bool) == Some(true)
}

/// A literal as a wildcard pattern that matches only itself.
fn wildcard_escape(s: &str) -> String {
    s.chars()
        .flat_map(|c| match c {
            '*' | '?' | '\\' => vec!['\\', c],
            c => vec![c],
        })
        .collect()
}

/// ES rejects parameters a query does not know: `[kind] query does not support [k]`.
fn check_keys(
    kind: &str,
    o: &serde_json::Map<String, Value>,
    known: &[&str],
) -> Result<(), String> {
    match o.keys().find(|k| !known.contains(&k.as_str())) {
        Some(k) => Err(format!("[{kind}] query does not support [{k}]")),
        None => Ok(()),
    }
}

/// Port of ES `Queries.calculateMinShouldMatch`: `3`, `-1`, `75%`, `-25%` or `2<-1 5<50%`.
fn min_should_match(optional: usize, spec: &str) -> Result<usize, String> {
    let n = optional as i64;
    let err = || format!("failed to parse minimum_should_match [{spec}]");
    let int = |s: &str| s.parse::<i32>().map(i64::from).map_err(|_| err());
    if spec.contains('<') {
        let spec = spec.split('<').map(str::trim).collect::<Vec<_>>().join("<");
        let mut result = optional;
        for cond in spec.split_whitespace() {
            let (upper, s) = cond.split_once('<').ok_or_else(err)?;
            if n <= int(upper)? {
                return Ok(result);
            }
            result = min_should_match(optional, s)?;
        }
        return Ok(result);
    }
    let spec = spec.trim();
    let result = match spec.strip_suffix('%') {
        Some(p) => {
            let calc = (n * int(p)?) as f32 * (1.0 / 100.0);
            if calc < 0.0 {
                n + calc as i64
            } else {
                calc as i64
            }
        }
        None => match int(spec)? {
            c if c < 0 => n + c,
            c => c,
        },
    };
    Ok(result.max(0) as usize)
}

fn clauses(v: Option<&Value>) -> Result<Vec<Query>, QueryError> {
    match v {
        None => Ok(vec![]),
        Some(Value::Array(a)) => a.iter().map(parse_query).collect(),
        Some(q) => Ok(vec![parse_query(q)?]),
    }
}

/// Any of `vals` in `field`, scored a constant 1.0 as ES's terms query is.
fn constant_score_any(field: &str, vals: &[Value]) -> Query {
    let term = |v: &Value| Query::Term {
        field: field.to_owned(),
        value: v.clone(),
    };
    Query::Bool {
        must: vec![Query::MatchAll],
        should: vec![],
        must_not: vec![],
        filter: vec![Query::Bool {
            must: vec![],
            should: vals.iter().map(term).collect(),
            must_not: vec![],
            filter: vec![],
            min_should: 1,
        }],
        min_should: 0,
    }
}

pub fn parse_query(v: &Value) -> Result<Query, QueryError> {
    let (kind, body) = single_field("query", v)
        .map_err(|_| "query malformed, expected a single query type".to_owned())?;
    let mut query = parse_kind(kind, body)?;
    // `_name` sits in the query's body, or in its single field's parameters
    let name = body.get("_name").or_else(|| {
        let (_, p) = single_field(kind, body).ok()?;
        p.get("_name")
    });
    let boost = body.get("boost").or_else(|| {
        let (_, p) = single_field(kind, body).ok()?;
        p.get("boost")
    });
    if let Some(boost) = boost.and_then(Value::as_f64).filter(|b| *b != 1.0) {
        query = Query::Boosted {
            boost,
            query: Box::new(query),
        };
    }
    Ok(match name.and_then(Value::as_str) {
        Some(name) => Query::Named {
            name: name.to_owned(),
            query: Box::new(query),
        },
        None => query,
    })
}

fn parse_kind(kind: &str, body: &Value) -> Result<Query, QueryError> {
    match kind {
        "match_all" => Ok(Query::MatchAll),
        "match" => {
            let (field, p) = single_field(kind, body)?;
            let (query, op) = match p {
                Value::Object(o) => {
                    check_keys("match", o, MATCH_KEYS)?;
                    (
                        o.get("query").ok_or("[match] requires query")?,
                        o.get("operator"),
                    )
                }
                q => (q, None),
            };
            let and = match op.and_then(Value::as_str) {
                None => false,
                Some(o) if o.eq_ignore_ascii_case("and") => true,
                Some(o) if o.eq_ignore_ascii_case("or") => false,
                Some(o) => {
                    return Err(format!(
                        "Unsupported defaultOperator [{o}], can either be 'and' or 'or'"
                    )
                    .into());
                }
            };
            Ok(Query::Match {
                field: field.clone(),
                query: text(query),
                and,
            })
        }
        "match_phrase" | "match_phrase_prefix" => {
            let (field, p) = single_field(kind, body)?;
            let prefix = kind == "match_phrase_prefix";
            let (query, slop, max_expansions) = match p {
                Value::Object(o) => {
                    let mut known = vec![
                        "query",
                        "slop",
                        "analyzer",
                        "zero_terms_query",
                        "boost",
                        "_name",
                    ];
                    known.extend(prefix.then_some("max_expansions"));
                    check_keys(kind, o, &known)?;
                    let slop = match o.get("slop") {
                        None => 0,
                        Some(s) => (s.as_u64().and_then(|s| u32::try_from(s).ok()))
                            .ok_or(format!("[{kind}] slop must be a non-negative integer"))?,
                    };
                    let max = match o.get("max_expansions").map(Value::as_i64) {
                        None => 50,
                        Some(Some(n)) if n >= 0 => n as usize,
                        Some(Some(_)) => {
                            return Err(QueryError::illegal("No negative maxExpansions allowed."));
                        }
                        Some(None) => {
                            return Err(
                                "[match_phrase_prefix] max_expansions must be an integer".into()
                            );
                        }
                    };
                    let query = o.get("query");
                    (query.ok_or(format!("[{kind}] requires query"))?, slop, max)
                }
                q => (q, 0, 50),
            };
            let (field, query) = (field.clone(), text(query));
            Ok(match prefix {
                false => Query::MatchPhrase { field, query, slop },
                true => Query::MatchPhrasePrefix {
                    field,
                    query,
                    slop,
                    // Lucene's rewrite always takes one term
                    max_expansions: max_expansions.max(1),
                    expansions: vec![],
                },
            })
        }
        "term" => {
            let (field, p) = single_field(kind, body)?;
            let (value, case_insensitive) = match p {
                Value::Object(o) => {
                    check_keys("term", o, &["value", "boost", "_name", "case_insensitive"])?;
                    let v = o.get("value").ok_or("[term] requires value")?;
                    (v.clone(), case_insensitive(o))
                }
                v => (v.clone(), false),
            };
            Ok(match case_insensitive {
                // ES's case-insensitive term is a constant-score automaton query, as a wildcard's
                // ponytail: on other than keyword or text fields it fails with the wildcard's error
                true => Query::Wildcard {
                    field: field.clone(),
                    pattern: wildcard_escape(&text(&value)),
                    case_insensitive,
                },
                false => Query::Term {
                    field: field.clone(),
                    value,
                },
            })
        }
        "prefix" => {
            let (field, p) = single_field(kind, body)?;
            let (prefix, case_insensitive) = match p {
                Value::Object(o) => {
                    check_keys(
                        "prefix",
                        o,
                        &[
                            "value",
                            "prefix",
                            "boost",
                            "_name",
                            "rewrite",
                            "case_insensitive",
                        ],
                    )?;
                    let v = o.get("value").or_else(|| o.get("prefix"));
                    let v = text(v.ok_or("[prefix] query requires a value")?);
                    (v, case_insensitive(o))
                }
                v => (text(v), false),
            };
            Ok(match case_insensitive {
                true => Query::Wildcard {
                    field: field.clone(),
                    pattern: wildcard_escape(&prefix) + "*",
                    case_insensitive,
                },
                false => Query::Prefix {
                    field: field.clone(),
                    prefix,
                },
            })
        }
        "wildcard" => {
            let (field, p) = single_field(kind, body)?;
            let (pattern, case_insensitive) = match p {
                Value::Object(o) => {
                    check_keys(
                        "wildcard",
                        o,
                        &[
                            "value",
                            "wildcard",
                            "boost",
                            "_name",
                            "case_insensitive",
                            "rewrite",
                        ],
                    )?;
                    let v = o.get("value").or_else(|| o.get("wildcard"));
                    (
                        text(v.ok_or("[wildcard] query requires a value")?),
                        case_insensitive(o),
                    )
                }
                v => (text(v), false),
            };
            Ok(Query::Wildcard {
                field: field.clone(),
                pattern,
                case_insensitive,
            })
        }
        "terms" => {
            let o = body.as_object().ok_or("[terms] query malformed")?;
            let (field, vals) = o
                .iter()
                .find(|(k, _)| !matches!(k.as_str(), "boost" | "_name"))
                .ok_or("[terms] query requires a field name, followed by array of terms")?;
            let vals = vals
                .as_array()
                .ok_or("[terms] query does not support lookup or non-array values")?;
            Ok(constant_score_any(field, vals))
        }
        "constant_score" => {
            let o = body.as_object().ok_or("[constant_score] query malformed")?;
            check_keys("constant_score", o, &["filter", "boost", "_name"])?;
            let filter = o
                .get("filter")
                .ok_or("[constant_score] requires a 'filter' element")?;
            Ok(Query::Bool {
                must: vec![Query::MatchAll],
                should: vec![],
                must_not: vec![],
                filter: vec![parse_query(filter)?],
                min_should: 0,
            })
        }
        "ids" => {
            let o = body.as_object().ok_or("[ids] query malformed")?;
            check_keys("ids", o, &["values", "boost", "_name"])?;
            let vals = o.get("values").and_then(Value::as_array);
            Ok(constant_score_any(
                "_id",
                vals.map_or(&[][..], Vec::as_slice),
            ))
        }
        "exists" => {
            let o = body.as_object().ok_or("[exists] query malformed")?;
            check_keys("exists", o, &["field", "boost", "_name"])?;
            let field = o.get("field").and_then(Value::as_str);
            Ok(Query::Exists {
                field: field.ok_or("required [field] not specified")?.to_owned(),
            })
        }
        "range" => {
            let (field, p) = single_field(kind, body)?;
            let p = p.as_object().ok_or("[range] query malformed")?;
            check_keys(
                "range",
                p,
                &[
                    "gt",
                    "gte",
                    "lt",
                    "lte",
                    "format",
                    "boost",
                    "_name",
                    "time_zone",
                    "relation",
                ],
            )?;
            let get = |k: &str| p.get(k).cloned();
            let time_zone = match p.get("time_zone").and_then(Value::as_str) {
                None => None,
                Some(id) => Some(TimeZone::parse(id).ok_or_else(|| {
                    QueryError::illegal(format!(
                        "java.time.zone.ZoneRulesException: Unknown time-zone ID: {id}"
                    ))
                })?),
            };
            // ES keeps the last of gt/gte (and of lt/lte) it parses
            let last = |a: &str, b: &str| p.keys().rfind(|k| *k == a || *k == b).cloned();
            let (lower, upper) = (last("gt", "gte"), last("lt", "lte"));
            let bound = |b: &Option<String>, k: &str| match b.as_deref() == Some(k) {
                true => get(k).map(Box::new),
                false => None,
            };
            Ok(Query::Range {
                field: field.clone(),
                gt: bound(&lower, "gt"),
                gte: bound(&lower, "gte"),
                lt: bound(&upper, "lt"),
                lte: bound(&upper, "lte"),
                format: p.get("format").and_then(Value::as_str).map(Into::into),
                time_zone,
            })
        }
        "bool" => {
            let o = body.as_object().ok_or("[bool] query malformed")?;
            check_keys(
                "bool",
                o,
                &[
                    "must",
                    "should",
                    "filter",
                    "must_not",
                    "minimum_should_match",
                    "boost",
                    "_name",
                    "adjust_pure_negative",
                ],
            )?;
            let must = clauses(body.get("must"))?;
            let should = clauses(body.get("should"))?;
            let filter = clauses(body.get("filter"))?;
            let default = if must.is_empty() && filter.is_empty() && !should.is_empty() {
                1
            } else {
                0
            };
            let min_should = match body.get("minimum_should_match") {
                None | Some(Value::Null) => 0,
                Some(Value::String(m)) => min_should_match(should.len(), m)?,
                Some(m) => min_should_match(should.len(), &m.to_string())?,
            };
            Ok(Query::Bool {
                must,
                should,
                must_not: clauses(body.get("must_not"))?,
                filter,
                // ES applies only a positive minimum_should_match.
                min_should: if min_should > 0 { min_should } else { default },
            })
        }
        "multi_match" => {
            let o = body.as_object().ok_or("[multi_match] query malformed")?;
            check_keys(
                kind,
                o,
                &[
                    "query",
                    "fields",
                    "type",
                    "operator",
                    "tie_breaker",
                    "slop",
                    "max_expansions",
                    "boost",
                    "_name",
                ],
            )?;
            o.get("query")
                .ok_or("No text specified for multi_match query")?;
            // ponytail: no cross_fields or bool_prefix types, field boosts, field patterns or
            // default `*` fields
            let (field_kind, params, tie) = match o.get("type").and_then(Value::as_str) {
                None | Some("best_fields") => ("match", &["query", "operator"][..], 0.0),
                Some("most_fields") => ("match", &["query", "operator"][..], 1.0),
                Some("phrase") => ("match_phrase", &["query", "slop"][..], 0.0),
                Some("phrase_prefix") => (
                    "match_phrase_prefix",
                    &["query", "slop", "max_expansions"][..],
                    0.0,
                ),
                Some(t @ ("cross_fields" | "bool_prefix")) => {
                    return Err(format!("[multi_match] type [{t}] is not supported").into());
                }
                Some(t) => {
                    return Err(format!(
                        "failed to parse [multi_match] query type [{t}]. unknown type."
                    )
                    .into());
                }
            };
            let fields = (o.get("fields").and_then(Value::as_array))
                .filter(|f| !f.is_empty())
                .ok_or("[multi_match] requires fields")?;
            let p: serde_json::Map<_, _> = (o.iter())
                .filter(|(k, _)| params.contains(&k.as_str()))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            let mut queries = (fields.iter())
                .map(|f| match f.as_str() {
                    Some(f) if !f.contains(['^', '*']) => {
                        parse_kind(field_kind, &serde_json::json!({ f: p }))
                    }
                    _ => Err(format!("[multi_match] field [{f}] is not supported").into()),
                })
                .collect::<Result<Vec<_>, _>>()?;
            let tie_breaker = match o.get("tie_breaker") {
                None => tie,
                Some(t) => t
                    .as_f64()
                    .ok_or("[multi_match] tie_breaker must be a number")?,
            };
            Ok(match queries.len() {
                1 => queries.remove(0),
                _ => Query::DisMax {
                    queries,
                    tie_breaker,
                },
            })
        }
        "simple_query_string" => {
            let o = body
                .as_object()
                .ok_or("[simple_query_string] query malformed")?;
            check_keys(
                "simple_query_string",
                o,
                &[
                    "query",
                    "fields",
                    "default_operator",
                    "minimum_should_match",
                    "boost",
                    "_name",
                ],
            )?;
            let query = text(
                o.get("query")
                    .ok_or("[simple_query_string] requires query")?,
            );
            // ponytail: no `+ | - " * ( ) ~` operators, field boosts or default `*` fields
            if query.contains(['+', '|', '"', '*', '(', ')', '~'])
                || query.split_whitespace().any(|t| t.starts_with('-'))
            {
                return Err("[simple_query_string] operators are not supported".into());
            }
            let fields: Vec<&str> = o
                .get("fields")
                .and_then(Value::as_array)
                .ok_or("[simple_query_string] requires fields")?
                .iter()
                .filter_map(Value::as_str)
                .collect();
            let and = match o.get("default_operator").and_then(Value::as_str) {
                None => false,
                Some(op) if op.eq_ignore_ascii_case("and") => true,
                Some(op) if op.eq_ignore_ascii_case("or") => false,
                Some(op) => return Err(format!("no [default_operator] for [{op}]").into()),
            };
            // Each term matches in any field (ES: dis_max with tie_breaker 1.0, a sum).
            let terms: Vec<Query> = query
                .split_whitespace()
                .map(|t| Query::Bool {
                    must: vec![],
                    should: fields
                        .iter()
                        .map(|f| Query::Match {
                            field: (*f).to_owned(),
                            query: t.to_owned(),
                            and: false,
                        })
                        .collect(),
                    must_not: vec![],
                    filter: vec![],
                    min_should: 1,
                })
                .collect();
            if and {
                return Ok(Query::Bool {
                    must: terms,
                    should: vec![],
                    must_not: vec![],
                    filter: vec![],
                    min_should: 0,
                });
            }
            let min_should = match o.get("minimum_should_match") {
                None | Some(Value::Null) => 0,
                Some(Value::String(m)) => min_should_match(terms.len(), m)?,
                Some(m) => min_should_match(terms.len(), &m.to_string())?,
            };
            Ok(Query::Bool {
                min_should: min_should.max(1),
                must: vec![],
                should: terms,
                must_not: vec![],
                filter: vec![],
            })
        }
        "query_string" => {
            let o = body.as_object().ok_or("[query_string] query malformed")?;
            check_keys(
                kind,
                o,
                &[
                    "query",
                    "default_field",
                    "default_operator",
                    "lenient",
                    "boost",
                    "_name",
                ],
            )?;
            let query = o.get("query").and_then(Value::as_str);
            let and = match o.get("default_operator").and_then(Value::as_str) {
                None => false,
                Some(op) if op.eq_ignore_ascii_case("and") => true,
                Some(op) if op.eq_ignore_ascii_case("or") => false,
                Some(op) => return Err(format!("operator [{op}] is not allowed").into()),
            };
            // ES defaults to index.query.default_field, `*`
            let df = o.get("default_field").and_then(Value::as_str);
            crate::parse_query_string(
                query.ok_or("[query_string] must be provided with a [query]")?,
                df.unwrap_or("*"),
                and,
            )
        }
        other => Err(format!("unknown query [{other}]").into()),
    }
}
