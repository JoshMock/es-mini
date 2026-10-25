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
        assert!(err.msg.contains(msg), "{q}: {err}");
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

#[test]
fn sorting_on_a_text_field_is_refused() {
    let mut i = Index::default();
    i.mappings
        .properties
        .insert("t".into(), FieldMapping::Text { store: false });
    i.put("1", json!({"t": "a"}));
    let targets = [("i", &i, None)];
    let req = SearchRequest::parse(&json!({"sort": ["t"]})).unwrap();
    let e = es_query::check_sortable(&targets, &req).unwrap_err();
    assert!(e.msg.starts_with("Text fields are not optimised"), "{e}");
    let ok = SearchRequest::parse(&json!({"sort": ["_score"]})).unwrap();
    assert!(es_query::check_sortable(&targets, &ok).is_ok());
}

#[test]
fn sorting_on_an_unmapped_field_is_refused_unless_unmapped_type_is_set() {
    let idx = index();
    let targets = [("i", &idx, None)];
    let check = |sort: Value| {
        es_query::check_sortable(
            &targets,
            &SearchRequest::parse(&json!({"sort": sort})).unwrap(),
        )
    };
    let e = check(json!(["nope"])).unwrap_err();
    assert_eq!(e.msg, "No mapping found for [nope] in order to sort on");
    assert_eq!(e.kind, es_query::ErrorKind::IllegalArgument);
    assert!(check(json!([{"nope": {"unmapped_type": "long"}}])).is_ok());
    for ok in ["tag", "nested.x", "_score", "_doc", "_id", "_index"] {
        assert!(check(json!([ok])).is_ok(), "{ok}");
    }
}

#[test]
fn sort_missing_controls_where_absent_values_go() {
    let mut i = Index::default();
    i.mappings.properties.insert("n".into(), FieldMapping::Long);
    i.put("a", json!({"n": 5}));
    i.put("b", json!({}));
    i.put("c", json!({"n": 1}));
    let order = |sort: Value| {
        let req = SearchRequest::parse(&json!({"sort": sort})).unwrap();
        es_query::search(&i, &req)
            .hits
            .iter()
            .map(|h| h.id.to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(order(json!([{"n": {}}])), ["c", "a", "b"]);
    assert_eq!(
        order(json!([{"n": {"missing": "_first"}}])),
        ["b", "c", "a"]
    );
    assert_eq!(
        order(json!([{"n": {"order": "desc", "missing": "_first"}}])),
        ["b", "a", "c"]
    );
    assert_eq!(order(json!([{"n": {"missing": 3}}])), ["c", "b", "a"]);
}

#[test]
fn date_range_compares_instants_not_strings() {
    let mut idx = Index::default();
    idx.mappings.properties.insert(
        "d".into(),
        FieldMapping::Date {
            format: None,
            locale: None,
            fields: Default::default(),
        },
    );
    idx.put("a", json!({"d": "2024-01-01T00:00:00Z"}));
    idx.put("b", json!({"d": "2024-06-01"}));
    idx.put("c", json!({"d": 1_717_286_400_000_i64})); // 2024-06-02
    let hits = |r: Value| {
        let req = SearchRequest::parse(&json!({"query": {"range": {"d": r}}})).unwrap();
        let mut v: Vec<_> = search(&idx, &req)
            .hits
            .iter()
            .map(|h| h.id.to_owned())
            .collect();
        v.sort();
        v
    };
    assert_eq!(
        hits(json!({"gte": "2024-05-01T10:00:00+02:00"})),
        ["b", "c"]
    );
    assert_eq!(hits(json!({"gt": 1_717_200_000_000_i64})), ["c"]);
    assert_eq!(hits(json!({"lt": "2024-06-01T00:00:00Z"})), ["a"]);
}

#[test]
fn terms_matches_any_value_with_constant_score() {
    let q = json!({"query": {"terms": {"tag": ["animal", "Pet"]}}});
    assert_eq!(sorted(ids(q.clone())), ["1", "2", "4"]);
    let idx = index();
    let r = search(&idx, &SearchRequest::parse(&q).unwrap());
    assert!(r.hits.iter().all(|h| h.score == Some(1.0)));
}

#[test]
fn constant_score_filters_with_score_one() {
    let q = json!({"query": {"constant_score": {"filter": {"term": {"tag": "animal"}}}}});
    let got = ids(q.clone());
    assert!(!got.is_empty());
    let idx = index();
    let r = search(&idx, &SearchRequest::parse(&q).unwrap());
    assert!(r.hits.iter().all(|h| h.score == Some(1.0)));
}

#[test]
fn ids_matches_document_ids() {
    assert_eq!(
        sorted(ids(json!({"query": {"ids": {"values": ["2", "3", "9"]}}}))),
        ["2", "3"]
    );
}

#[test]
fn exists_matches_documents_with_a_value() {
    assert_eq!(
        sorted(ids(json!({"query": {"exists": {"field": "ok"}}}))),
        ["1", "2"]
    );
    assert_eq!(
        sorted(ids(json!({"query": {"exists": {"field": "nested"}}}))),
        ["3"]
    );
    assert!(ids(json!({"query": {"exists": {"field": "nope"}}})).is_empty());
}

#[test]
fn wildcard_matches_star_and_question_mark() {
    let w = |v: Value| sorted(ids(json!({"query": {"wildcard": {"tag": v}}})));
    assert_eq!(w(json!("an*")), ["1", "2"]);
    assert_eq!(w(json!({"value": "p?t"})), ["3"]);
    assert_eq!(
        w(json!({"value": "P*", "case_insensitive": true})),
        ["3", "4"]
    );
    assert_eq!(w(json!("*i*")), ["1", "2"]);
    assert!(w(json!("pe")).is_empty());
    // Text fields match against analyzed tokens.
    let t = ids(json!({"query": {"wildcard": {"title": "qu*k"}}}));
    assert_eq!(sorted(t), ["1", "2"]);
}

#[test]
fn literal_dotted_keyword_source_field_is_not_shadowed() {
    let mut i = Index::default();
    i.put("1", json!({"foo": "a", "foo.keyword": "b"}));
    let req = SearchRequest::parse(&json!({"query": {"term": {"foo.keyword": "b"}}})).unwrap();
    assert_eq!(search(&i, &req).total, 1);
}

#[test]
fn large_longs_keep_precision() {
    let mut i = Index::default();
    i.put("a", json!({"n": 9007199254740993_i64}));
    i.put("b", json!({"n": 9007199254740992_i64}));
    let req =
        SearchRequest::parse(&json!({"query": {"term": {"n": 9007199254740993_i64}}})).unwrap();
    let hits = search(&i, &req).hits;
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, "a");
}

#[test]
fn integer_term_matches_whole_float_source_value() {
    let mut i = Index::default();
    i.put("a", json!({"n": 10.0}));
    i.put("b", json!({"n": 10.5}));
    let req = SearchRequest::parse(&json!({"query": {"term": {"n": 10}}})).unwrap();
    let hits = search(&i, &req).hits;
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, "a");
}

#[test]
fn error_kinds() {
    use es_query::{ErrorKind, SearchRequest};
    let k = |b: serde_json::Value| SearchRequest::parse(&b).unwrap_err().kind;
    assert_eq!(
        k(serde_json::json!({"size": -1})),
        ErrorKind::IllegalArgument
    );
    assert_eq!(k(serde_json::json!({"bogus": 1})), ErrorKind::Parsing);
}

#[test]
fn parse_query_error_messages() {
    let err = |v: Value| parse_query(&v).unwrap_err().to_string();
    assert_eq!(
        err(json!({"match_all": {}, "term": {}})),
        "query malformed, expected a single query type"
    );
    assert_eq!(
        err(json!({"match": {"a": 1, "b": 2}})),
        "[match] query malformed, expected a single field"
    );
    assert_eq!(err(json!({"terms": 1})), "[terms] query malformed");
    assert_eq!(
        err(json!({"match": {"a": {"query": "x", "operator": "xor"}}})),
        "Unsupported defaultOperator [xor], can either be 'and' or 'or'"
    );
}

/// In-memory postings narrow term, match and bool queries; these must find exactly what a scan
/// would, including `_id` terms, arrays, nested paths, and documents replaced or deleted after
/// the postings were built.
#[test]
fn postings_narrowed_queries_track_writes() {
    let mut idx = index();
    let run = |idx: &Index, q: Value| -> Vec<String> {
        let mut v: Vec<String> = search(idx, &SearchRequest::parse(&json!({"query": q})).unwrap())
            .hits
            .iter()
            .map(|h| h.id.to_owned())
            .collect();
        v.sort();
        v
    };
    assert_eq!(run(&idx, json!({"term": {"_id": "3"}})), ["3"]);
    assert_eq!(run(&idx, json!({"match": {"title": "dog"}})), ["3", "4"]);
    idx.put("3", json!({"title": "Lazy cat", "tag": "pet", "n": 1}));
    idx.put("5", json!({"title": "dog days", "tag": "pet", "n": 2}));
    idx.delete("4");
    assert_eq!(run(&idx, json!({"match": {"title": "dog"}})), ["5"]);
    assert_eq!(
        run(&idx, json!({"term": {"nested.x": "deep"}})),
        Vec::<String>::new()
    );
    let q = json!({"bool": {"must": {"match": {"title": "cat dog"}}, "filter": {"term": {"tag": "pet"}}}});
    assert_eq!(run(&idx, q), ["3", "5"]);
    assert_eq!(
        run(&idx, json!({"term": {"_id": "4"}})),
        Vec::<String>::new()
    );
}

#[test]
fn simple_query_string_matches_each_whitespace_term_across_fields() {
    let q = |p: Value| sorted(ids(json!({"query": {"simple_query_string": p}})));
    // Each term is analyzed on its own, so a keyword field matches single words.
    assert_eq!(
        q(json!({"fields": ["tag"], "query": "pet animal"})),
        ["1", "2", "3"]
    );
    assert_eq!(
        q(json!({"fields": ["title", "tag"], "query": "lazy animal", "minimum_should_match": 1})),
        ["1", "2", "3"]
    );
    assert_eq!(
        q(json!({"fields": ["title"], "query": "brown dog", "default_operator": "AND"})),
        ["4"]
    );
    assert_eq!(
        q(json!({"fields": ["title"], "query": "brown dog", "minimum_should_match": "100%"})),
        ["4"]
    );
    let err = |p: Value| parse_query(&json!({"simple_query_string": p})).unwrap_err();
    err(json!({"fields": ["title"], "query": "brown -dog"}));
    err(json!({"query": "dog"}));
}
