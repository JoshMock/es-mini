use es_index::{FieldMapping, Index};
use es_query::{Query, SearchRequest, count, parse_query, search};
use serde_json::{Value, json};

fn index() -> Index {
    let mut i = Index::default();
    i.mappings
        .properties
        .insert("tag".into(), FieldMapping::Keyword { ignore_above: None });
    i.put(
        "1",
        json!({"title": "The quick brown fox", "tag": "animal", "n": 5, "ok": true}),
    );
    i.put(
        "2",
        json!({"title": "Quick quick rabbits", "tag": "animal", "n": 10, "ok": false}),
    );
    i.put(
        "3",
        json!({"title": "Lazy dog", "tag": "pet", "n": 1, "nested": {"x": "deep"}}),
    );
    i.put(
        "4",
        json!({"title": ["a brown", "dog"], "tag": "Pet", "n": 7}),
    );
    i
}

fn ids(body: Value) -> Vec<String> {
    let idx = index();
    let req = SearchRequest::parse(&body).unwrap();
    search(&idx, &req)
        .hits
        .iter()
        .map(|h| h.id.to_owned())
        .collect()
}

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

#[test]
fn match_all_is_default() {
    let idx = index();
    let r = search(&idx, &SearchRequest::parse(&json!({})).unwrap());
    assert_eq!(r.total, 4);
    assert_eq!(r.hits[0].score, Some(1.0));
}

#[test]
fn match_analyzes_and_ranks_by_bm25() {
    // "quick" twice in a shorter doc outranks once.
    assert_eq!(
        ids(json!({"query": {"match": {"title": "QUICK"}}})),
        ["2", "1"]
    );
}

#[test]
fn match_operator_and() {
    assert_eq!(
        ids(json!({"query": {"match": {"title": {"query": "brown dog", "operator": "and"}}}})),
        ["4"]
    );
    assert_eq!(
        sorted(ids(json!({"query": {"match": {"title": "brown dog"}}}))),
        ["1", "3", "4"]
    );
}

#[test]
fn term_is_not_analyzed() {
    assert_eq!(ids(json!({"query": {"term": {"tag": "pet"}}})), ["3"]);
    assert_eq!(
        ids(json!({"query": {"term": {"tag": {"value": "Pet"}}}})),
        ["4"]
    );
    // Text fields hold lowercased tokens, so an uppercase term finds nothing.
    assert!(ids(json!({"query": {"term": {"title": "Quick"}}})).is_empty());
    assert_eq!(
        sorted(ids(json!({"query": {"term": {"title": "quick"}}}))),
        ["1", "2"]
    );
}

#[test]
fn unmapped_string_has_keyword_subfield() {
    assert_eq!(
        ids(json!({"query": {"term": {"title.keyword": "Lazy dog"}}})),
        ["3"]
    );
    assert_eq!(ids(json!({"query": {"term": {"nested.x": "deep"}}})), ["3"]);
}

#[test]
fn term_on_numbers_and_booleans() {
    assert_eq!(ids(json!({"query": {"term": {"n": 5}}})), ["1"]);
    assert_eq!(ids(json!({"query": {"term": {"n": "10"}}})), ["2"]);
    assert_eq!(ids(json!({"query": {"term": {"ok": false}}})), ["2"]);
}

#[test]
fn range() {
    let q = json!({"query": {"range": {"n": {"gte": 5, "lt": 10}}}});
    assert_eq!(sorted(ids(q)), ["1", "4"]);
    assert_eq!(ids(json!({"query": {"range": {"n": {"gt": 7}}}})), ["2"]);
}

#[test]
fn range_gte_overrides_gt_and_lte_overrides_lt() {
    let q = json!({"query": {"range": {"n": {"gt": 7, "gte": 5}}}});
    assert_eq!(sorted(ids(q)), ["1", "2", "4"]);
    let q = json!({"query": {"range": {"n": {"lt": 5, "lte": 5, "gte": 5}}}});
    assert_eq!(ids(q), ["1"]);
}

#[test]
fn bool_clauses() {
    let q = json!({"query": {"bool": {
        "must": [{"match": {"title": "dog"}}],
        "must_not": {"term": {"tag": "pet"}},
    }}});
    assert_eq!(ids(q), ["4"]);
    let q = json!({"query": {"bool": {
        "filter": [{"range": {"n": {"gte": 5}}}],
        "should": [{"match": {"title": "rabbits"}}],
    }}});
    // should is optional next to a filter, but boosts matches.
    assert_eq!(ids(q)[0], "2");
    let q = json!({"query": {"bool": {"should": [{"term": {"n": 1}}, {"term": {"n": 5}}]}}});
    assert_eq!(sorted(ids(q)), ["1", "3"]);
    let q = json!({"query": {"bool": {"should": [{"term": {"n": 1}}, {"term": {"tag": "pet"}}], "minimum_should_match": 2}}});
    assert_eq!(ids(q), ["3"]);
}

#[test]
fn filter_only_scores_zero() {
    let idx = index();
    let req =
        SearchRequest::parse(&json!({"query": {"bool": {"filter": {"term": {"tag": "pet"}}}}}))
            .unwrap();
    assert_eq!(search(&idx, &req).hits[0].score, Some(0.0));
}

#[test]
fn sort_and_pagination() {
    assert_eq!(
        ids(json!({"sort": [{"n": {"order": "desc"}}]})),
        ["2", "4", "1", "3"]
    );
    assert_eq!(
        ids(json!({"sort": ["n"], "from": 1, "size": 2})),
        ["1", "4"]
    );
    assert_eq!(
        ids(json!({"sort": [{"tag": "asc"}, {"n": "desc"}]})),
        ["4", "2", "1", "3"]
    );
    let idx = index();
    let r = search(
        &idx,
        &SearchRequest::parse(&json!({"sort": ["n"], "size": 1})).unwrap(),
    );
    assert_eq!(
        (r.total, r.hits[0].score, r.hits[0].sort.clone()),
        (4, None, vec![json!(1)])
    );
}

#[test]
fn count_matches_total() {
    let idx = index();
    let q = parse_query(&json!({"match": {"title": "dog"}})).unwrap();
    assert_eq!(count(&idx, &q), 2);
}

#[test]
fn bad_queries_are_errors() {
    for q in [
        json!({"nope": {}}),
        json!({"match": {}}),
        json!({"term": 5}),
        json!({"range": {"n": {"gte": 1, "zzz": 2}}}),
    ] {
        assert!(parse_query(&q).is_err(), "{q}");
    }
}

mod many {
    use super::*;
    use es_query::{count_many, search_many};

    fn two() -> (Index, Index) {
        let mut a = Index::default();
        a.put("1", json!({"t": "fox", "n": 3}));
        a.put("2", json!({"t": "dog", "n": 1}));
        let mut b = Index::default();
        b.put("1", json!({"t": "fox fox", "n": 2}));
        (a, b)
    }

    #[test]
    fn merges_hits_across_indices_in_sort_order() {
        let (a, b) = two();
        let req = SearchRequest::parse(&json!({"sort": ["n"]})).unwrap();
        let targets = [("a", &a, None), ("b", &b, None)];
        let r = search_many(&targets, &req).unwrap();
        let got: Vec<(&str, &str)> = r.hits.iter().map(|h| (h.index, h.id)).collect();
        assert_eq!(got, [("a", "2"), ("b", "1"), ("a", "1")]);
        assert_eq!(r.total, 3);
    }

    #[test]
    fn applies_per_target_filters_and_pagination() {
        let (a, b) = two();
        let req =
            SearchRequest::parse(&json!({"query": {"match": {"t": "fox"}}, "size": 1})).unwrap();
        let only_n3 = json!({"term": {"n": 3}});
        let r = search_many(&[("a", &a, Some(&only_n3)), ("b", &b, None)], &req).unwrap();
        assert_eq!(r.total, 2);
        assert_eq!(r.hits.len(), 1);
        let q = parse_query(&json!({"match_all": {}})).unwrap();
        assert_eq!(
            count_many(&[("a", &a, Some(&only_n3)), ("b", &b, None)], &q).unwrap(),
            2
        );
        let bad = json!({"bogus": {}});
        assert!(search_many(&[("a", &a, Some(&bad))], &req).is_err());
    }
}

/// ES's default similarity is LegacyBM25Similarity, which scales Lucene's BM25 by (1 + k1).
/// One doc, one matching term: idf = ln(1 + 0.5/1.5), tf = 1/(1 + 1.2), times 2.2.
#[test]
fn bm25_matches_elasticsearch_legacy_scaling() {
    let mut idx = Index::default();
    idx.put("1", json!({"t": "fox"}));
    let req = SearchRequest::parse(&json!({"query": {"match": {"t": "fox"}}})).unwrap();
    let score = search(&idx, &req).hits[0].score.unwrap();
    assert!((score - 0.287_682_1).abs() < 1e-6, "{score}");
}

#[test]
fn leaf_and_bool_queries_validate_parameters() {
    for (q, msg) in [
        (
            json!({"match": {"title": {"query": "a", "x": 1}}}),
            "[match] query does not support [x]",
        ),
        (
            json!({"match": {"title": {"query": "a", "operator": "xor"}}}),
            "Unsupported defaultOperator [xor], can either be 'and' or 'or'",
        ),
        (
            json!({"term": {"tag": {"value": "a", "x": 1}}}),
            "[term] query does not support [x]",
        ),
        (
            json!({"bool": {"must": [], "x": 1}}),
            "[bool] query does not support [x]",
        ),
        (
            json!({"bool": {"should": [], "minimum_should_match": "abc"}}),
            "abc",
        ),
        (
            json!({"bool": {"should": [], "minimum_should_match": "-1<2 3"}}),
            "-1<2 3",
        ),
    ] {
        let err = parse_query(&q).unwrap_err();
        assert!(err.contains(msg), "{q}: {err}");
    }
    assert!(parse_query(&json!({"match": {"title": {"query": "a", "operator": "AND", "boost": 2, "_name": "n"}}})).is_ok());
    assert!(
        parse_query(&json!({"term": {"tag": {"value": "a", "case_insensitive": true}}})).is_ok()
    );
}

#[test]
fn range_accepts_time_zone_and_relation() {
    assert!(
        parse_query(
            &json!({"range": {"n": {"gte": 1, "time_zone": "+01:00", "relation": "within"}}})
        )
        .is_ok()
    );
}

#[test]
fn minimum_should_match_accepts_elasticsearch_specs() {
    let msm = |spec: Value| {
        let should = vec![json!({"match_all": {}}); 4];
        let q = json!({"bool": {"should": should, "minimum_should_match": spec}});
        match parse_query(&q).unwrap() {
            Query::Bool { min_should, .. } => min_should,
            q => panic!("{q:?}"),
        }
    };
    // 4 should clauses; a result of 0 or less leaves the default of 1.
    for (spec, want) in [
        (json!(2), 2),
        (json!("2"), 2),
        (json!(-1), 3),
        (json!("75%"), 3),
        (json!("-25%"), 3),
        (json!("3<-1"), 3),
        (json!("5<-1"), 4),
        (json!("2<-1 3 < 50%"), 2),
        (json!(-5), 1),
        (json!(0), 1),
    ] {
        assert_eq!(msm(spec.clone()), want, "{spec}");
    }
}
