//! The `searches` and `fetch` sections of a shard's search profile, shaped like Elasticsearch's
//! (QueryProfileShardResult, FetchProfiler). Timings are es-mini's own: the whole shard's time
//! goes to the top query and the collectors, its children's to none.

use crate::{Query, SearchRequest};
use serde_json::{Value, json};

/// The `searches` section of a shard that resolved its query in `rewrite` nanoseconds and
/// searched in `took`.
pub(crate) fn searches(req: &SearchRequest, query: &Query, rewrite: u64, took: u64) -> Value {
    // ES needs no scores without top hits, so it wraps the query in a constant score
    let query = match req.size {
        0 => {
            let (_, inner, _) = describe(query);
            let mut q = node(
                "ConstantScoreQuery",
                format!("ConstantScore({inner})"),
                took,
            );
            q["children"] = json!([tree(query, 0)]);
            q
        }
        _ => tree(query, took),
    };
    let by_score = match &req.sort[..] {
        [] => true,
        [f] => f.field == "_score",
        _ => false,
    };
    let mut children = vec![match req.size {
        0 => ("PartialHitCountCollector".to_string(), "search_count"),
        _ if by_score => ("TopScoreDocCollector".into(), "search_top_hits"),
        _ => ("SimpleFieldCollector".into(), "search_top_hits"),
    }];
    if !req.aggs.is_empty() {
        let names: Vec<_> = req.aggs.iter().map(|a| a.name()).collect();
        let name = format!("AggregatorCollector: [{}]", names.join(", "));
        children.push((name, "aggregation"));
    }
    let children: Vec<_> = (children.into_iter())
        .map(|(name, reason)| json!({"name": name, "reason": reason, "time_in_nanos": took}))
        .collect();
    json!([{
        "query": [query],
        "rewrite_time": rewrite,
        "collector": [{"name": "QueryPhaseCollector", "reason": "search_query_phase",
            "time_in_nanos": took, "children": children}],
    }])
}

/// The `fetch` section of a shard that fetched `n` hits in `took` nanoseconds.
/// ponytail: always the fields, source and stored-fields phases of a plain fetch, all hits
/// read as from one segment; ES adds phases (highlight, version, ...) per request.
pub(crate) fn fetch(n: usize, took: u64) -> Value {
    let phase = |kind: &str| {
        json!({"type": kind, "description": "", "time_in_nanos": took, "breakdown": {
            "process_count": n, "process": took, "next_reader": 0, "next_reader_count": 1}})
    };
    let mut source = phase("FetchSourcePhase");
    source["debug"] = json!({"fast_path": n});
    json!({
        "type": "fetch",
        "description": "",
        "time_in_nanos": took,
        "breakdown": {"load_stored_fields": took, "load_stored_fields_count": n,
            "load_source": 0, "load_source_count": n, "next_reader": 0, "next_reader_count": 1},
        "debug": {"stored_fields": ["_id", "_routing", "_source"]},
        "children": [phase("FetchFieldsPhase"), source, phase("StoredFieldsPhase")],
    })
}

/// A query's profile node and its children's, `took` nanoseconds spent in it.
fn tree(q: &Query, took: u64) -> Value {
    let (kind, description, children) = describe(q);
    let mut v = node(&kind, description, took);
    if !children.is_empty() {
        v["children"] = children.into_iter().map(|c| tree(c, 0)).collect();
    }
    v
}

fn node(kind: &str, description: String, took: u64) -> Value {
    json!({"type": kind, "description": description, "time_in_nanos": took, "breakdown": {
        "set_min_competitive_score_count": 0, "match_count": 0, "shallow_advance_count": 0,
        "set_min_competitive_score": 0, "next_doc": 0, "match": 0, "next_doc_count": 0,
        "score_count": 0, "compute_max_score_count": 0, "compute_max_score": 0, "advance": 0,
        "advance_count": 0, "count_weight_count": 0, "score": 0, "build_scorer_count": 1,
        "create_weight": 0, "shallow_advance": 0, "count_weight": 0, "create_weight_count": 1,
        "build_scorer": took}})
}

/// The Lucene query class ES profiles `q` as, its `toString`, and its sub-queries.
/// ponytail: Lucene's names only for match_all, string terms, bool and boosts, without Lucene's
/// rewrites (a lone clause, a filter-only bool); other queries report their es-mini variant
/// and no description. A should-only bool's `~1` is left out, as ES's default is.
fn describe(q: &Query) -> (String, String, Vec<&Query>) {
    match q {
        Query::MatchAll => ("MatchAllDocsQuery".into(), "*:*".into(), vec![]),
        Query::Term {
            field,
            value: Value::String(v),
        } => ("TermQuery".into(), format!("{field}:{v}"), vec![]),
        Query::Named { query, .. } => describe(query),
        Query::Boosted { boost, query } => {
            let (_, inner, _) = describe(query);
            let description = format!("({inner})^{:?}", *boost as f32);
            ("BoostQuery".into(), description, vec![query])
        }
        Query::Bool {
            must,
            should,
            must_not,
            filter,
            min_should,
        } => {
            let only_should = must.is_empty() && filter.is_empty();
            let min = (*min_should > usize::from(only_should)).then_some(*min_should);
            let clauses: Vec<_> = (must.iter().map(|q| ("+", q)))
                .chain(must_not.iter().map(|q| ("-", q)))
                .chain(should.iter().map(|q| ("", q)))
                .chain(filter.iter().map(|q| ("#", q)))
                .collect();
            let text: Vec<_> = (clauses.iter())
                .map(|(occur, q)| match describe(q) {
                    (kind, d, _) if kind == "BooleanQuery" => format!("{occur}({d})"),
                    (_, d, _) => format!("{occur}{d}"),
                })
                .collect();
            let text = text.join(" ");
            let description = match min {
                Some(n) => format!("({text})~{n}"),
                None => text,
            };
            let children = clauses.into_iter().map(|(_, q)| q).collect();
            ("BooleanQuery".into(), description, children)
        }
        _ => {
            let debug = format!("{q:?}");
            let variant = debug.split(|c: char| !c.is_alphanumeric()).next();
            (variant.unwrap_or_default().into(), String::new(), vec![])
        }
    }
}
