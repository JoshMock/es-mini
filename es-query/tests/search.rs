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

/// The documents and expected hits are Elasticsearch 9.5.5's.
fn dates() -> Index {
    let mut idx = Index::default();
    let date = |format: Option<&str>| FieldMapping::Date {
        format: format.map(Into::into),
        locale: None,
        fields: Default::default(),
    };
    let props = &mut idx.mappings.properties;
    props.insert("date".into(), date(None));
    props.insert("f".into(), date(Some("yyyy/MM/dd HH:mm||epoch_second")));
    idx.put(
        "1",
        json!({"date": "2022-01-01T12:12:12.123Z", "f": "2022/01/01 10:00"}),
    );
    idx.put(
        "2",
        json!({"date": "2022-01-02T00:30:00Z", "f": 1_641_081_600}),
    );
    idx.put(
        "3",
        json!({"date": 1_641_000_000_000_i64, "f": "2021/12/31 23:00"}),
    );
    idx.put("4", json!({"date": "2022"}));
    idx
}

fn date_hits(q: Value) -> Result<Vec<String>, es_query::QueryError> {
    let idx = dates();
    let req = SearchRequest::parse(&json!({"query": q}))?;
    let r = es_query::search_many(&[("d", &idx, None)], &req)?;
    Ok(sorted(r.hits.iter().map(|h| h.id.to_owned()).collect()))
}

#[test]
fn date_ranges_read_formats_time_zones_and_date_math() {
    let range = |r: Value| date_hits(json!({"range": {"date": r}})).unwrap();
    let tz = |r: Value, tz: &str| {
        let mut r = r;
        r["time_zone"] = tz.into();
        range(r)
    };
    let day = json!({"gte": "2022-01-01", "lte": "2022-01-01"});
    assert_eq!(range(day.clone()), ["1", "3", "4"]);
    assert_eq!(tz(day, "+01:00"), ["1", "3", "4"]);
    let next = json!({"gte": "2022-01-01T00:00:00||+1d"});
    assert_eq!(tz(next, "Europe/Berlin"), ["2"]);
    assert_eq!(range(json!({"gt": "2022-01-01||/d", "lte": "now"})), ["2"]);
    assert_eq!(
        range(json!({"lt": "2022-01-01T12:12:12", "gte": "2022"})),
        ["3", "4"]
    );
    assert_eq!(
        range(json!({"lte": "2022-01-01T12:12:12"})),
        ["1", "3", "4"]
    );
    assert_eq!(
        range(json!({"gt": "2022-01-01", "lt": "2022-01-03"})),
        ["2"]
    );
    let month = json!({"gte": "2022-01-01||/M", "lte": "2022-01-01||+1d/d"});
    assert_eq!(range(month), ["1", "2", "3", "4"]);
    // a number is epoch millis unless the query gives a format
    assert_eq!(range(json!({"gte": 2022})), ["1", "2", "3", "4"]);
    let formatted = json!({"gte": "01/01/2022", "format": "dd/MM/yyyy"});
    assert_eq!(range(formatted), ["1", "2", "3", "4"]);
    let secs = |k: &str| range(json!({k: "1641000000", "format": "epoch_second"}));
    assert_eq!(secs("gte"), ["1", "2", "3"]);
    assert_eq!(secs("lte"), ["3", "4"]);
    let offset = json!({"gte": "2022-01-01T12:12:12+01:00"});
    assert_eq!(tz(offset, "-05:00"), ["1", "2"]);
    // a mapping's format reads the bounds
    let f = json!({"range": {"f": {"gte": "2022/01/01 00:00", "lt": "1641081600"}}});
    assert_eq!(date_hits(f).unwrap(), ["1"]);
    let q = parse_query(
        &json!({"range": {"date": {"gte": "2021-12-31T19:00:00", "time_zone": "America/New_York"}}}),
    )
    .unwrap();
    assert_eq!(es_query::count_many(&[("d", &dates(), None)], &q), Ok(4));
}

#[test]
fn date_terms_match_the_instants_they_name() {
    let hits = |q: Value| date_hits(q).unwrap();
    // a date without a time matches its whole day (the upper bound rounds up)
    assert_eq!(
        hits(json!({"term": {"date": "2022-01-01"}})),
        ["1", "3", "4"]
    );
    let at = json!({"term": {"date": "2022-01-01T12:12:12"}});
    assert_eq!(hits(at), ["1"]);
    assert_eq!(hits(json!({"match": {"date": "2022-01-02"}})), ["2"]);
    let millis = json!({"term": {"date": 1_641_000_000_000_i64}});
    assert_eq!(hits(millis), ["3"]);
    assert!(hits(json!({"term": {"date": 2022}})).is_empty());
    let any = json!({"terms": {"date": ["2022-01-02", "2021"]}});
    assert_eq!(hits(any), ["2"]);
    let e = date_hits(json!({"term": {"date": "bogus"}})).unwrap_err();
    assert_eq!(e.kind, es_query::ErrorKind::Parse);
}

#[test]
fn filter_aggregations_read_dates_as_queries_do() {
    let idx = dates();
    let aggs = json!({
        "day": {"filter": {"term": {"date": "2022-01-01"}}},
        "by": {"filters": {"filters": {"past": {"range": {"date": {"lt": "now"}}}}},
            "aggs": {"tz": {"filter": {"range": {"date": {"gte": "2022-01-01", "time_zone": "-01:00"}}}}}},
    });
    let req = SearchRequest::parse(&json!({"size": 0, "aggs": aggs})).unwrap();
    let r = es_query::search_many(&[("d", &idx, None)], &req).unwrap();
    let a = r.aggregations.unwrap();
    assert_eq!(a["day"]["doc_count"], 3);
    assert_eq!(a["by"]["buckets"]["past"]["doc_count"], 4);
    assert_eq!(a["by"]["buckets"]["past"]["tz"]["doc_count"], 3);
}

#[test]
fn dates_sort_as_epoch_millis() {
    let idx = dates();
    let sort = |s: Value| {
        let req = SearchRequest::parse(&json!({"sort": [s]})).unwrap();
        (search(&idx, &req).hits.iter())
            .map(|h| (h.id.to_owned(), h.sort[0].clone()))
            .collect::<Vec<_>>()
    };
    let got = sort(json!({"date": "desc"}));
    let want = [
        ("2", 1_641_083_400_000_i64),
        ("1", 1_641_039_132_123),
        ("3", 1_641_000_000_000),
        ("4", 1_640_995_200_000),
    ];
    assert_eq!(got, want.map(|(id, t)| (id.to_owned(), json!(t))));
    // the mapping's format reads the values; documents without one sort last
    let got = sort(json!({"f": "asc"}));
    let ids: Vec<_> = got.iter().map(|(id, _)| id.as_str()).collect();
    assert_eq!(ids, ["3", "1", "2", "4"]);
    assert_eq!(got[0].1, json!(1_640_991_600_000_i64));
    assert_eq!(got[2].1, json!(1_641_081_600_000_i64));
}

#[test]
fn bad_date_range_bounds_are_errors() {
    use es_query::ErrorKind::{IllegalArgument, Parse};
    let err = |q: Value| date_hits(q).unwrap_err();
    let e = err(json!({"range": {"date": {"gte": "bogus"}}}));
    let f = "strict_date_optional_time||epoch_millis";
    let reason = format!("failed to parse date field [bogus] with format [{f}]");
    assert_eq!((e.kind, e.msg), (Parse, format!("{reason}: [{reason}]")));
    let e = err(json!({"range": {"date": {"gte": "now+1x"}}}));
    assert_eq!(
        (e.kind, e.msg.as_str()),
        (Parse, "unit [x] not supported for date math [+1x]")
    );
    // a number forces epoch_millis for both bounds
    let e = err(json!({"range": {"f": {"gte": "2022/01/01 10:00", "lt": 1_641_081_600}}}));
    assert!(e.msg.contains("with format [epoch_millis]"), "{e}");
    let e = err(json!({"range": {"date": {"gte": "2022", "time_zone": "Mars/Base"}}}));
    let reason = "java.time.zone.ZoneRulesException: Unknown time-zone ID: Mars/Base";
    assert_eq!((e.kind, e.msg.as_str()), (IllegalArgument, reason));
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

/// Like Lucene: the statistics sum the segments (with their replaced and deleted docs) and the
/// documents written since the index was opened.
#[test]
fn statistics_of_an_opened_index_count_later_writes() {
    let mapped = || {
        let mut i = Index::default();
        (i.mappings.properties).insert("title".into(), FieldMapping::Text { store: false });
        i
    };
    let (mut memory, mut saved) = (mapped(), mapped());
    for (id, title) in [
        ("1", "quick fox"),
        ("2", "lazy dog"),
        ("3", "brown fox jumps"),
    ] {
        memory.put(id, json!({"title": title}));
        saved.put(id, json!({"title": title}));
    }
    let dir = std::env::temp_dir().join(format!("esq-stats-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    es_index::disk::save(&dir, &es_index::Indices::from([("idx".into(), saved)])).unwrap();
    let mut opened = es_index::disk::open(&dir).unwrap().remove("idx").unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    for idx in [&mut memory, &mut opened] {
        idx.put("4", json!({"title": "fox"}));
        idx.put("2", json!({"title": "lazy fox"}));
    }
    let scores = |idx: &Index| {
        let req = SearchRequest::parse(&json!({"query": {"match": {"title": "fox"}}})).unwrap();
        let mut v: Vec<_> = (search(idx, &req).hits.iter())
            .map(|h| (h.id.to_owned(), h.score))
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    };
    assert_eq!(scores(&opened), scores(&memory));
    assert_eq!(scores(&opened.snapshot()), scores(&memory));
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

/// Scores are Elasticsearch 9.5.5's for `index()`'s documents.
#[test]
fn match_phrase_matches_terms_in_order_within_slop() {
    let hits = |p: Value| {
        let req = SearchRequest::parse(&json!({"query": {"match_phrase": {"title": p}}})).unwrap();
        let idx = index();
        let hits: Vec<_> = (search(&idx, &req).hits.iter())
            .map(|h| (h.id.to_owned(), h.score.unwrap()))
            .collect();
        hits
    };
    let close = |got: Vec<(String, f32)>, want: &[(&str, f32)]| {
        assert_eq!(got.len(), want.len(), "{got:?}");
        for ((id, s), (wid, ws)) in got.iter().zip(want) {
            assert_eq!(id, wid, "{got:?}");
            assert!((s - ws).abs() < 1e-5, "{got:?} vs {want:?}");
        }
    };
    close(hits(json!("quick brown")), &[("1", 1.2199391)]);
    close(hits(json!("the, quick!")), &[("1", 1.6694658)]);
    close(hits(json!("quick")), &[("2", 0.9530773), ("1", 0.60996956)]);
    close(hits(json!("quick quick")), &[("2", 1.3862942)]);
    // values are 100 positions apart
    close(hits(json!("brown dog")), &[]);
    close(
        hits(json!({"query": "brown dog", "slop": 101})),
        &[("4", 0.024957895)],
    );
    close(
        hits(json!({"query": "quick fox", "slop": 1})),
        &[("1", 1.043416)],
    );
    close(hits(json!({"query": "fox quick", "slop": 2})), &[]);
    close(
        hits(json!({"query": "fox quick", "slop": 3})),
        &[("1", 0.59623766)],
    );
    let err = |p: Value| {
        parse_query(&json!({"match_phrase": {"title": p}}))
            .unwrap_err()
            .to_string()
    };
    assert_eq!(
        err(json!({"query": "x", "operator": "and"})),
        "[match_phrase] query does not support [operator]"
    );
}

#[test]
fn match_phrase_scores_an_opened_index_like_memory() {
    let mut memory = index();
    let dir = std::env::temp_dir().join(format!("esq-phrase-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    es_index::disk::save(&dir, &es_index::Indices::from([("idx".into(), index())])).unwrap();
    let mut opened = es_index::disk::open(&dir).unwrap().remove("idx").unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    for idx in [&mut memory, &mut opened] {
        idx.put("5", json!({"title": "brown quick fox"}));
    }
    let q = json!({"query": {"match_phrase": {"title": {"query": "quick fox", "slop": 1}}}});
    let req = SearchRequest::parse(&q).unwrap();
    let scores = |idx: &Index| {
        let mut v: Vec<_> = (search(idx, &req).hits.iter())
            .map(|h| (h.id.to_owned(), h.score))
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    };
    assert_eq!(scores(&memory).len(), 2);
    assert_eq!(scores(&opened), scores(&memory));
    assert_eq!(count(&opened, &req.query), 2);
}

/// Scores are Elasticsearch 9.5.5's for `index()`'s documents and a fifth, in one segment.
#[test]
fn match_phrase_prefix_expands_the_last_term_to_indexed_terms() {
    let mut idx = index();
    idx.put("5", json!({"title": "quick brawny bear"}));
    let run = |q: Value| {
        let req = SearchRequest::parse(&json!({"query": {"match_phrase_prefix": q}}))?;
        es_query::search_many(&[("", &idx, None)], &req).map(|r| {
            (r.hits.iter())
                .map(|h| (h.id.to_owned(), h.score.unwrap()))
                .collect::<Vec<_>>()
        })
    };
    let close = |p: Value, want: &[(&str, f32)]| {
        let got = run(json!({"title": p})).unwrap();
        assert_eq!(got.len(), want.len(), "{got:?}");
        for ((id, s), (wid, ws)) in got.iter().zip(want) {
            assert_eq!(id, wid, "{got:?}");
            assert!((s - ws).abs() < 1e-5, "{got:?} vs {want:?}");
        }
    };
    close(json!("quick b"), &[("5", 4.187054), ("1", 3.684608)]);
    close(json!({"query": "quick b", "max_expansions": 1}), &[]);
    close(
        json!({"query": "quick b", "max_expansions": 2}),
        &[("5", 3.3115852)],
    );
    // one position: any of the expansions, as term queries
    close(
        json!("qu"),
        &[("2", 0.7411202), ("5", 0.53899646), ("1", 0.47431695)],
    );
    close(
        json!("B"),
        &[("5", 2.7725885), ("4", 0.87546873), ("1", 0.77041256)],
    );
    close(json!("brown d"), &[]);
    close(
        json!({"query": "brown d", "slop": 101}),
        &[("4", 0.03152275)],
    );
    close(json!({"query": "fox b", "slop": 3}), &[("1", 2.0137405)]);
    close(json!("the quick"), &[("1", 1.6942561)]);
    close(json!("x"), &[]);
    close(json!("!!"), &[]);
    let err = |q: Value| run(q).unwrap_err().to_string();
    assert_eq!(
        err(json!({"tag": "ani"})),
        "Can only use phrase prefix queries on text fields - not on [tag] which is of type [keyword]"
    );
    assert_eq!(
        err(json!({"title": {"query": "x", "operator": "and"}})),
        "[match_phrase_prefix] query does not support [operator]"
    );
    assert_eq!(
        err(json!({"title": {"query": "x", "max_expansions": -1}})),
        "No negative maxExpansions allowed."
    );
}

#[test]
fn match_phrase_prefix_expands_from_an_opened_index() {
    let dir = std::env::temp_dir().join(format!("esq-phrase-prefix-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    // mapped, so its terms are in the segment's term dictionary
    let mut idx = index();
    (idx.mappings.properties).insert("title".into(), FieldMapping::Text { store: false });
    es_index::disk::save(&dir, &es_index::Indices::from([("idx".into(), idx)])).unwrap();
    let mut opened = es_index::disk::open(&dir).unwrap().remove("idx").unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(opened.prefixed_terms("title", "title", "b", 50), ["brown"]);
    opened.put("5", json!({"title": "quick brawny bear"}));
    let q = json!({"query": {"match_phrase_prefix": {"title": "quick b"}}});
    let req = SearchRequest::parse(&q).unwrap();
    let hits: Vec<_> = (search(&opened, &req).hits.iter())
        .map(|h| (h.id.to_owned(), h.score.unwrap()))
        .collect();
    assert_eq!(hits.len(), 2, "{hits:?}");
    assert_eq!(hits[0].0, "5");
    assert!((hits[0].1 - 4.187054).abs() < 1e-5, "{hits:?}");
    assert!((hits[1].1 - 3.684608).abs() < 1e-5, "{hits:?}");
    assert_eq!(count(&opened, &req.query), 2);
}
