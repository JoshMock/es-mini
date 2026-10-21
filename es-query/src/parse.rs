//! Query DSL parsing.

use crate::{QueryError, text};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub enum Query {
    MatchAll,
    Match {
        field: String,
        query: String,
        and: bool,
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
        gt: Option<Value>,
        gte: Option<Value>,
        lt: Option<Value>,
        lte: Option<Value>,
    },
    Bool {
        must: Vec<Query>,
        should: Vec<Query>,
        must_not: Vec<Query>,
        filter: Vec<Query>,
        min_should: usize,
    },
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
    match kind.as_str() {
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
        "term" => {
            let (field, p) = single_field(kind, body)?;
            let value = match p {
                Value::Object(o) => {
                    check_keys("term", o, &["value", "boost", "_name", "case_insensitive"])?;
                    o.get("value").ok_or("[term] requires value")?.clone()
                }
                v => v.clone(),
            };
            Ok(Query::Term {
                field: field.clone(),
                value,
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
                        o.get("case_insensitive").and_then(Value::as_bool) == Some(true),
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
            // ponytail: boost is ignored, so the score is always 1.0
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
                    // ponytail: accepted but ignored; dates compare without a time zone
                    "time_zone",
                    "relation",
                ],
            )?;
            // ponytail: `uuuu` bounds become ISO strings (compared lexicographically); upper bounds
            // round up to the end of Jan 1 as ES does. Real date parsing would replace this.
            let year = p.get("format").and_then(Value::as_str) == Some("uuuu");
            let get = |k: &str| {
                let v = p.get(k)?;
                let Some(y) = v.as_i64().filter(|_| year) else {
                    return Some(v.clone());
                };
                let end = matches!(k, "gt" | "lte");
                let t = if end {
                    "23:59:59.999999999Z"
                } else {
                    "00:00:00"
                };
                Some(Value::String(format!("{y:04}-01-01T{t}")))
            };
            if p.get("format").and_then(Value::as_str) == Some("date_optional_time") {
                // ponytail: only bare-number years are checked; ES's max year is 292278993
                if let Some(n) = ["gt", "gte", "lt", "lte"]
                    .iter()
                    .filter_map(|k| p.get(*k)?.as_i64())
                    .find(|n| n.abs() > 292_278_993)
                {
                    return Err(QueryError::illegal(format!(
                        "failed to parse date field [{n}] with format [date_optional_time]"
                    )));
                }
            }
            Ok(Query::Range {
                field: field.clone(),
                // ES keeps the last of gt/gte (and lt/lte) parsed; serde_json sorts keys, so
                // the inclusive bound stands in for "last". ponytail: enable preserve_order for true order.
                gt: get("gte").map_or_else(|| get("gt"), |_| None),
                gte: get("gte"),
                lt: get("lte").map_or_else(|| get("lt"), |_| None),
                lte: get("lte"),
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
        other => Err(format!("unknown query [{other}]").into()),
    }
}

/// `q=` URI search: whitespace-separated `[field:]term` clauses, `term*` is a prefix.
// ponytail: no quoting, ranges, boolean operators or escapes
pub fn parse_query_string(q: &str, df: &str, and: bool) -> Query {
    let clauses: Vec<Query> = q
        .split_whitespace()
        .map(|t| {
            let (field, t) = t.split_once(':').unwrap_or((df, t));
            match t.strip_suffix('*') {
                Some(p) => Query::Prefix {
                    field: field.to_owned(),
                    prefix: p.to_lowercase(),
                },
                None => Query::Match {
                    field: field.to_owned(),
                    query: t.to_owned(),
                    and: false,
                },
            }
        })
        .collect();
    let (must, should) = if and {
        (clauses, vec![])
    } else {
        (vec![], clauses)
    };
    Query::Bool {
        min_should: usize::from(must.is_empty()),
        must,
        should,
        must_not: vec![],
        filter: vec![],
    }
}
