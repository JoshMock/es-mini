use es_index::{FieldMapping, Index};
use es_query::{Query, SearchRequest, count, parse_query, search};
use serde_json::{Value, json};

fn index() -> Index {
    let mut i = Index::default();
    i.mappings.properties.insert(
        "tag".into(),
        FieldMapping::Keyword {
            ignore_above: None,
            split_queries_on_whitespace: false,
        },
    );
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
    let r = es_query::search_many(&[("d", 0, &idx, None)], &req)?;
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
    assert_eq!(es_query::count_many(&[("d", 0, &dates(), None)], &q), Ok(4));
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
    let r = es_query::search_many(&[("d", 0, &idx, None)], &req).unwrap();
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
        let targets = [("a", 0, &a, None), ("b", 0, &b, None)];
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
        let r = search_many(&[("a", 0, &a, Some(&only_n3)), ("b", 0, &b, None)], &req).unwrap();
        assert_eq!(r.total, 2);
        assert_eq!(r.hits.len(), 1);
        let q = parse_query(&json!({"match_all": {}})).unwrap();
        assert_eq!(
            count_many(&[("a", 0, &a, Some(&only_n3)), ("b", 0, &b, None)], &q).unwrap(),
            2
        );
        let bad = json!({"bogus": {}});
        assert!(search_many(&[("a", 0, &a, Some(&bad))], &req).is_err());
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
    i.mappings.properties.insert(
        "t".into(),
        FieldMapping::Text {
            store: false,
            fields: Default::default(),
        },
    );
    i.put("1", json!({"t": "a"}));
    let targets = [("i", 0, &i, None)];
    let req = SearchRequest::parse(&json!({"sort": ["t"]})).unwrap();
    let e = es_query::check_sortable(&targets, &req).unwrap_err();
    assert!(e.msg.starts_with("Text fields are not optimised"), "{e}");
    let ok = SearchRequest::parse(&json!({"sort": ["_score"]})).unwrap();
    assert!(es_query::check_sortable(&targets, &ok).is_ok());
}

#[test]
fn sorting_on_an_unmapped_field_is_refused_unless_unmapped_type_is_set() {
    let idx = index();
    let targets = [("i", 0, &idx, None)];
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
    for ok in ["tag", "_score", "_doc", "_id", "_index"] {
        assert!(check(json!([ok])).is_ok(), "{ok}");
    }
}

#[test]
fn mapping_validation_sorting_on_a_source_path_the_mappings_lack_is_refused() {
    let idx = index();
    let targets = [("i", 0, &idx, None)];
    let req = SearchRequest::parse(&json!({"sort": ["nested.x"]})).unwrap();
    let e = es_query::check_sortable(&targets, &req).unwrap_err();
    assert_eq!(e.msg, "No mapping found for [nested.x] in order to sort on");
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
fn date_and_boolean_sort_values_are_longs() {
    let mut i = Index::default();
    let date = json!({"type": "date", "format": "epoch_millis"});
    i.mappings
        .properties
        .insert("d".into(), serde_json::from_value(date).unwrap());
    i.mappings
        .properties
        .insert("b".into(), FieldMapping::Boolean);
    i.put("a", json!({"d": 5, "b": true}));
    i.put("b", json!({"b": false}));
    i.put("c", json!({"d": 1}));
    let sorts = |sort: Value| {
        let req = SearchRequest::parse(&json!({"sort": sort})).unwrap();
        let r = es_query::search(&i, &req);
        r.hits
            .iter()
            .map(|h| (h.id.to_string(), h.sort.clone()))
            .collect::<Vec<_>>()
    };
    // As Elasticsearch: booleans sort as 0 and 1, and a missing long as Long.MAX_VALUE, or
    // MIN_VALUE where missing values come first in the order.
    let (max, min) = (json!(i64::MAX), json!(i64::MIN));
    assert_eq!(
        sorts(json!(["d", "b"])),
        [
            ("c".into(), vec![json!(1), max.clone()]),
            ("a".into(), vec![json!(5), json!(1)]),
            ("b".into(), vec![max.clone(), json!(0)]),
        ]
    );
    assert_eq!(
        sorts(json!([{"d": {"order": "desc"}}])),
        [
            ("a".into(), vec![json!(5)]),
            ("c".into(), vec![json!(1)]),
            ("b".into(), vec![min.clone()]),
        ]
    );
    assert_eq!(sorts(json!([{"d": {"missing": "_first"}}]))[0].1, [min]);
    assert_eq!(
        sorts(json!([{"d": {"order": "desc", "missing": "_first"}}]))[0].1,
        [max]
    );
}

#[test]
fn boolean_sort_values_of_an_opened_index_are_longs() {
    let mut i = Index::default();
    (i.mappings.properties).insert("b".into(), FieldMapping::Boolean);
    i.put("a", json!({"b": true}));
    i.put("b", json!({"b": false}));
    i.put("c", json!({}));
    let dir = std::env::temp_dir().join(format!("esq-bool-sort-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    es_index::disk::save(&dir, &es_index::Indices::from([("idx".into(), i)])).unwrap();
    let mut opened = es_index::disk::open(&dir).unwrap().remove("idx").unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    // in memory, beside the documents on disk
    opened.put("d", json!({"b": false}));
    let req = SearchRequest::parse(&json!({"sort": ["b"]})).unwrap();
    let sorts: Vec<_> = (search(&opened, &req).hits.iter())
        .map(|h| (h.id.to_string(), h.sort.clone()))
        .collect();
    let expected = [("b", 0), ("d", 0), ("a", 1), ("c", i64::MAX)];
    let expected: Vec<_> = (expected.iter())
        .map(|(id, v)| (id.to_string(), vec![json!(v)]))
        .collect();
    assert_eq!(sorts, expected);
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
        (i.mappings.properties).insert(
            "title".into(),
            FieldMapping::Text {
                store: false,
                fields: Default::default(),
            },
        );
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
        es_query::search_many(&[("", 0, &idx, None)], &req).map(|r| {
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
    (idx.mappings.properties).insert(
        "title".into(),
        FieldMapping::Text {
            store: false,
            fields: Default::default(),
        },
    );
    es_index::disk::save(&dir, &es_index::Indices::from([("idx".into(), idx)])).unwrap();
    let mut opened = es_index::disk::open(&dir).unwrap().remove("idx").unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(
        opened.shards()[0].prefixed_terms("title", "title", "b", 50),
        ["brown"]
    );
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

/// Scores are Elasticsearch 9.5.5's for the same documents, in one segment.
#[test]
fn multi_match_combines_per_field_queries_by_type() {
    let mut idx = Index::default();
    (idx.mappings.properties).insert(
        "tag".into(),
        FieldMapping::Keyword {
            ignore_above: None,
            split_queries_on_whitespace: false,
        },
    );
    idx.put("1", json!({"title": "quick brown fox", "body": "lazy dog"}));
    idx.put(
        "2",
        json!({"title": "lazy fox", "body": "quick quick brown dog"}),
    );
    idx.put(
        "3",
        json!({"title": "brown dog", "body": "brown fox jumps"}),
    );
    idx.put("4", json!({"title": "slow turtle", "tag": "quick"}));
    let run = |mut q: Value| {
        if q.get("fields").is_none() {
            q["fields"] = json!(["title", "body"]);
        }
        let req = SearchRequest::parse(&json!({"query": {"multi_match": q}}))?;
        es_query::search_many(&[("", 0, &idx, None)], &req).map(|r| {
            (r.hits.iter())
                .map(|h| (h.id.to_owned(), h.score.unwrap()))
                .collect::<Vec<_>>()
        })
    };
    let close = |q: Value, want: &[(&str, f32)]| {
        let got = run(q).unwrap();
        assert_eq!(got.len(), want.len(), "{got:?}");
        for ((id, s), (wid, ws)) in got.iter().zip(want) {
            assert_eq!(id, wid, "{got:?}");
            assert!((s - ws).abs() < 1e-5, "{got:?} vs {want:?}");
        }
    };
    close(
        json!({"query": "quick fox"}),
        &[("1", 1.6694658), ("2", 1.2330425), ("3", 0.9808291)],
    );
    close(
        json!({"query": "quick fox", "tie_breaker": 0.3}),
        &[("1", 1.6694658), ("2", 1.4508888), ("3", 0.9808291)],
    );
    close(
        json!({"query": "quick fox", "type": "most_fields"}),
        &[("2", 1.9591966), ("1", 1.6694658), ("3", 0.9808291)],
    );
    close(
        json!({"query": "quick dog", "operator": "and"}),
        &[("2", 1.6466458)],
    );
    close(
        json!({"query": "brown fox", "type": "phrase"}),
        &[("3", 1.4508328), ("1", 1.2199391)],
    );
    close(
        json!({"query": "fox brown", "type": "phrase", "slop": 2}),
        &[("3", 0.6938765), ("1", 0.55451775)],
    );
    close(
        json!({"query": "brown f", "type": "phrase_prefix"}),
        &[("3", 1.4508328), ("1", 1.2199391)],
    );
    close(
        json!({"query": "quick", "fields": ["title", "tag"]}),
        &[("1", 1.0594962), ("4", 0.2876821)],
    );
    close(
        json!({"query": "quick", "fields": ["body"]}),
        &[("2", 1.2330425)],
    );
    assert_eq!(
        count(
            &idx,
            &parse_query(
                &json!({"multi_match": {"query": "quick fox", "fields": ["title", "body"]}})
            )
            .unwrap()
        ),
        3
    );
    let err = |q: Value| run(q).unwrap_err().to_string();
    assert_eq!(
        err(json!({"query": "x", "type": "bogus"})),
        "failed to parse [multi_match] query type [bogus]. unknown type."
    );
    assert_eq!(
        err(json!({"query": "x", "foo": 1})),
        "[multi_match] query does not support [foo]"
    );
    assert_eq!(err(json!({})), "No text specified for multi_match query");
}

#[test]
fn multi_match_scores_an_opened_index_like_memory() {
    let mut idx = index();
    let dir = std::env::temp_dir().join(format!("esq-multi-match-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    es_index::disk::save(&dir, &es_index::Indices::from([("idx".into(), index())])).unwrap();
    let mut opened = es_index::disk::open(&dir).unwrap().remove("idx").unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    for idx in [&mut idx, &mut opened] {
        idx.put("5", json!({"title": "quick pet", "tag": "quick"}));
    }
    let q = json!({"query": {"multi_match": {"query": "quick", "fields": ["title", "tag"], "tie_breaker": 0.5}}});
    let req = SearchRequest::parse(&q).unwrap();
    let scores = |idx: &Index| {
        let mut v: Vec<_> = (search(idx, &req).hits.iter())
            .map(|h| (h.id.to_owned(), h.score))
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    };
    assert_eq!(scores(&idx).len(), 3);
    assert_eq!(scores(&opened), scores(&idx));
    assert_eq!(count(&opened, &req.query), 3);
}

/// Hits and scores are Elasticsearch 9.5.5's for the same documents, in one segment, with
/// `default_field: title`.
#[test]
fn query_string_parses_lucene_syntax() {
    let mut idx = Index::default();
    let props = &mut idx.mappings.properties;
    props.insert(
        "tag".into(),
        FieldMapping::Keyword {
            ignore_above: None,
            split_queries_on_whitespace: false,
        },
    );
    props.insert("n".into(), FieldMapping::Integer);
    idx.put("1", json!({"title": "quick brown fox", "tag": "a", "n": 1}));
    idx.put("2", json!({"title": "lazy brown dog", "tag": "b", "n": 5}));
    idx.put(
        "3",
        json!({"title": "quick fox jumps over the lazy dog", "tag": "c", "n": 10}),
    );
    idx.put("4", json!({"title": "foo-bar baz", "tag": "a:b", "n": 20}));
    idx.put("5", json!({"title": "slow (turtle)", "n": 7}));
    let run = |q: &str, op: &str| {
        let body = json!({"query": {"query_string": {
            "query": q, "default_field": "title", "default_operator": op}}});
        let req = SearchRequest::parse(&body)?;
        es_query::search_many(&[("", 0, &idx, None)], &req).map(|r| {
            (r.hits.iter())
                .map(|h| (h.id.to_owned(), h.score.unwrap()))
                .collect::<Vec<_>>()
        })
    };
    let check = |q: &str, op: &str, want: &[(&str, f32)]| {
        let got = run(q, op).unwrap_or_else(|e| panic!("{q}: {e:?}"));
        let mut got_ids: Vec<_> = got.iter().map(|(id, _)| id.as_str()).collect();
        let mut want_ids: Vec<_> = want.iter().map(|(id, _)| *id).collect();
        got_ids.sort();
        want_ids.sort();
        assert_eq!(got_ids, want_ids, "{q}: {got:?}");
        for (id, s) in &got {
            let w = want.iter().find(|(w, _)| w == id).unwrap().1;
            assert!((s - w).abs() < 1e-5, "{q}: {got:?} vs {want:?}");
        }
    };
    let (q1, q3, d2) = (0.9395274, 0.6314857, 0.9395274);
    let cases: &[(&str, &[(&str, f32)])] = &[
        ("quick", &[("1", q1), ("3", q3)]),
        ("quick AND fox", &[("1", 1.8790548), ("3", 1.2629714)]),
        ("quick OR dog", &[("3", 1.2629714), ("1", q1), ("2", d2)]),
        ("quick || dog", &[("3", 1.2629714), ("1", q1), ("2", d2)]),
        ("quick -fox", &[]),
        ("+quick fox", &[("1", 1.8790548), ("3", 1.2629714)]),
        ("NOT fox", &[("2", 0.0), ("4", 0.0), ("5", 0.0)]),
        ("(-fox)", &[("2", 0.0), ("4", 0.0), ("5", 0.0)]),
        ("-fox -dog", &[("4", 0.0), ("5", 0.0)]),
        ("quick && !jumps", &[("1", q1)]),
        (
            "(quick OR lazy) AND dog",
            &[("3", 1.8944571), ("2", 1.8790548)],
        ),
        (
            "quick AND fox OR dog",
            &[("3", 1.8944571), ("1", 1.8790548)],
        ),
        ("quick dog -lazy", &[("1", q1)]),
        ("quick +(lazy OR fox) -dog", &[("1", 1.8790548)]),
        (
            "title:(quick lazy)",
            &[("3", 1.2629714), ("1", q1), ("2", d2)],
        ),
        ("title:(+quick -fox) OR tag:b", &[("2", 1.2039728)]),
        ("\"brown fox\"", &[("1", 1.8790548)]),
        ("\"quick \\\"brown\"", &[("1", 1.8790548)]),
        ("\"fox quick\"~2", &[("3", 0.53874993)]),
        ("\"fox quick\"~1", &[]),
        ("n:[5 TO 10]", &[("2", 1.0), ("3", 1.0), ("5", 1.0)]),
        ("n:{5 TO 10]", &[("3", 1.0), ("5", 1.0)]),
        ("n:[* TO 5}", &[("1", 1.0)]),
        (
            "n:[5 TO *]",
            &[("2", 1.0), ("3", 1.0), ("4", 1.0), ("5", 1.0)],
        ),
        ("n:>=7", &[("3", 1.0), ("4", 1.0), ("5", 1.0)]),
        ("n:>7", &[("3", 1.0), ("4", 1.0)]),
        ("n:<5", &[("1", 1.0)]),
        ("n:<=5", &[("1", 1.0), ("2", 1.0)]),
        ("n:[1 TO 5] OR n:20", &[("1", 1.0), ("2", 1.0), ("4", 1.0)]),
        ("n:[1 TO 5] AND quick", &[("1", 1.9395274)]),
        (
            "title:\"brown dog\" OR n:>=20",
            &[("2", 1.8790548), ("4", 1.0)],
        ),
        ("tag:[a TO b]", &[("1", 1.0), ("2", 1.0), ("4", 1.0)]),
        ("tag:{a TO b}", &[("4", 1.0)]),
        ("tag:a\\:b", &[("4", 1.2039728)]),
        ("tag:\"a:b\"", &[("4", 1.2039728)]),
        ("foo\\-bar", &[("4", 2.975461)]),
        ("foo-bar", &[("4", 2.975461)]),
        ("\\(turtle\\)", &[("5", 1.6943598)]),
        ("quick\\ fox", &[("1", 1.8790548), ("3", 1.2629714)]),
        ("qu*", &[("1", 1.0), ("3", 1.0)]),
        ("QU*", &[("1", 1.0), ("3", 1.0)]),
        ("qu?ck", &[("1", 1.0), ("3", 1.0)]),
        ("tag:a*", &[("1", 1.0), ("4", 1.0)]),
        (
            "_exists_:tag",
            &[("1", 1.0), ("2", 1.0), ("3", 1.0), ("4", 1.0)],
        ),
        ("!_exists_:tag", &[("5", 0.0)]),
        (
            "n:*",
            &[("1", 1.0), ("2", 1.0), ("3", 1.0), ("4", 1.0), ("5", 1.0)],
        ),
        (
            "*:*",
            &[("1", 1.0), ("2", 1.0), ("3", 1.0), ("4", 1.0), ("5", 1.0)],
        ),
        ("", &[]),
    ];
    for (q, want) in cases {
        check(q, "or", want);
    }
    let and_cases: &[(&str, &[(&str, f32)])] = &[
        ("quick fox", &[("1", 1.8790548), ("3", 1.2629714)]),
        ("quick OR fox", &[("1", 1.8790548), ("3", 1.2629714)]),
        ("quick fox -lazy", &[("1", 1.8790548)]),
        ("quick OR fox AND dog", &[("3", 1.8944571)]),
    ];
    for (q, want) in and_cases {
        check(q, "and", want);
    }
    for q in [
        "quick AND",
        "\"unterminated",
        "quick)",
        "(quick",
        "tag:a\\\\:b",
    ] {
        let e = *run(q, "or").unwrap_err().cause.unwrap();
        assert!(
            e.msg.starts_with(&format!("Cannot parse '{q}'")),
            "{q}: {e:?}"
        );
    }
}

#[test]
fn match_on_a_keyword_splitting_queries_on_whitespace_matches_any_word() {
    let mut idx = Index::default();
    idx.mappings.properties.insert(
        "t".into(),
        FieldMapping::Keyword {
            ignore_above: None,
            split_queries_on_whitespace: true,
        },
    );
    idx.put("1", json!({"t": "foo"}));
    idx.put("2", json!({"t": "foo bar"}));
    let req = SearchRequest::parse(&json!({"query": {"match": {"t": "foo bar"}}})).unwrap();
    let hits = search(&idx, &req).hits;
    assert_eq!(
        hits.iter().map(|h| h.id.to_owned()).collect::<Vec<_>>(),
        ["1"]
    );
}

/// es-mini matches objects at a text or keyword leaf (Elasticsearch rejects them), so disk
/// saves keep such a field out of the segment instead of dropping those values.
#[test]
fn an_opened_index_matches_objects_at_text_and_keyword_leaves_like_memory() {
    let mapped = || {
        let mut i = Index::default();
        let p = &mut i.mappings.properties;
        p.insert(
            "t".into(),
            FieldMapping::Text {
                store: false,
                fields: Default::default(),
            },
        );
        let keyword = FieldMapping::Keyword {
            ignore_above: None,
            split_queries_on_whitespace: false,
        };
        p.insert("k".into(), keyword);
        i.put("1", json!({"t": "five", "k": "five"}));
        i.put(
            "2",
            json!({"t": {"a": "five"}, "k": [{"a": "five"}, "six"]}),
        );
        i
    };
    let memory = mapped();
    let dir = std::env::temp_dir().join(format!("esq-leaf-objects-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    es_index::disk::save(&dir, &es_index::Indices::from([("idx".into(), mapped())])).unwrap();
    let opened = es_index::disk::open(&dir).unwrap().remove("idx").unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    let run = |idx: &Index, body: &Value| {
        let r = search(idx, &SearchRequest::parse(body).unwrap());
        let mut hits: Vec<_> = (r.hits.iter())
            .map(|h| (h.id.to_owned(), h.score))
            .collect();
        hits.sort_by(|a, b| a.0.cmp(&b.0));
        (hits, r.aggregations)
    };
    for body in [
        json!({"query": {"match": {"t": "five"}}}),
        json!({"size": 0, "aggs": {"k": {"terms": {"field": "k"}}}}),
    ] {
        assert_eq!(run(&opened, &body), run(&memory, &body), "{body}");
    }
}

#[test]
fn mapping_validation_queries_on_an_empty_object_match_nothing() {
    let mut i = Index::default();
    i.put("1", json!({"e": {}}));
    i.map_dynamic("1", &json!({"e": {}})).unwrap();
    assert_eq!(
        i.mappings.properties["e"],
        FieldMapping::Object { dynamic: None }
    );
    for q in [json!({"term": {"e": "x"}}), json!({"match": {"e": "x"}})] {
        let req = SearchRequest::parse(&json!({"query": q})).unwrap();
        assert!(search(&i, &req).hits.is_empty(), "{q}");
    }
}

#[test]
fn query_dsl_gaps_term_and_prefix_case_insensitive() {
    let q = |q: Value| sorted(ids(json!({ "query": q })));
    let term = |v: Value| q(json!({"term": {"tag": v}}));
    assert_eq!(
        term(json!({"value": "PET", "case_insensitive": true})),
        ["3", "4"]
    );
    assert_eq!(
        term(json!({"value": "pet", "case_insensitive": false})),
        ["3"]
    );
    // the value is a literal, not a pattern
    assert!(term(json!({"value": "p*", "case_insensitive": true})).is_empty());
    let prefix = |v: Value| q(json!({"prefix": {"tag": v}}));
    assert_eq!(
        prefix(json!({"value": "PE", "case_insensitive": true})),
        ["3", "4"]
    );
    assert_eq!(prefix(json!({"value": "Pe"})), ["4"]);
    // ES scores a case-insensitive term as a constant-score automaton query
    let idx = index();
    let req = json!({"query": {"term": {"tag": {"value": "Pet", "case_insensitive": true}}}});
    let r = search(&idx, &SearchRequest::parse(&req).unwrap());
    assert!(r.hits.iter().all(|h| h.score == Some(1.0)));
}

#[test]
fn query_dsl_gaps_case_insensitive_term_on_id_matches_ids() {
    let mut idx = Index::default();
    idx.put("Ab", json!({}));
    idx.put("cd", json!({}));
    let hits = |v: &str| {
        let q = json!({"query": {"term": {"_id": {"value": v, "case_insensitive": true}}}});
        let r = search(&idx, &SearchRequest::parse(&q).unwrap());
        r.hits.iter().map(|h| h.id.to_owned()).collect::<Vec<_>>()
    };
    assert_eq!(hits("Ab"), ["Ab"]);
    assert_eq!(hits("aB"), ["Ab"]);
}

#[test]
fn query_dsl_gaps_range_last_bound_wins() {
    let r = |q: Value| sorted(ids(json!({"query": {"range": {"n": q}}})));
    // n: 5, 10, 1, 7; ES keeps the last of gt/gte and of lt/lte it parses
    assert_eq!(r(json!({"gte": 5, "gt": 5})), ["2", "4"]);
    assert_eq!(r(json!({"gt": 5, "gte": 5})), ["1", "2", "4"]);
    assert_eq!(r(json!({"lte": 7, "lt": 7})), ["1", "3"]);
    assert_eq!(r(json!({"lt": 7, "lte": 7})), ["1", "3", "4"]);
}

#[test]
fn query_dsl_gaps_prefix_and_wildcard_need_keyword_or_text() {
    let mut i = index();
    let date = FieldMapping::Date {
        format: None,
        locale: None,
        fields: Default::default(),
    };
    i.mappings.properties.insert("d".into(), date);
    i.put("5", json!({"d": "2020-01-01"}));
    let run = |q: Value| {
        let req = SearchRequest::parse(&json!({ "query": q })).unwrap();
        es_query::search_many(&[("", 0, &i, None)], &req).map(|r| r.total)
    };
    let err = run(json!({"prefix": {"d": "2020"}})).unwrap_err();
    assert_eq!(
        err.msg,
        "Can only use prefix queries on keyword, text and wildcard fields - not on [d] which is of type [date]"
    );
    let err = run(json!({"wildcard": {"n": "1*"}})).unwrap_err();
    assert_eq!(
        err.msg,
        "Can only use wildcard queries on keyword, text and wildcard fields - not on [n] which is of type [long]"
    );
    assert_eq!(run(json!({"prefix": {"tag": "pe"}})).unwrap(), 1);
    assert_eq!(run(json!({"wildcard": {"title": "do?"}})).unwrap(), 2);
}

/// The Lucene ParseException a malformed query_string fails a shard with.
fn query_string_error(q: &str) -> es_query::QueryError {
    let req = SearchRequest::parse(&json!({"query": {"query_string": {"query": q}}}));
    let e = es_query::search_many(&[("", 0, &index(), None)], &req.unwrap()).unwrap_err();
    *e.cause.unwrap()
}

#[test]
fn query_dsl_gaps_query_string_unicode_escapes() {
    let qs = |q: &str| json!({"query": {"query_string": {"query": q}}});
    assert_eq!(sorted(ids(qs(r"tag:\u0070et"))), ["3"]);
    assert_eq!(sorted(ids(qs(r#"title:"\u0044OG""#))), ["3", "4"]);
    for (q, msg) in [
        (r"tag:p\u00", "Truncated Unicode escape sequence."),
        (
            r"tag:\u00zz",
            "Non-hex character in Unicode escape sequence: z",
        ),
    ] {
        let err = query_string_error(q);
        assert_eq!(err.msg, format!("Cannot parse '{q}': {msg}"));
    }
}

#[test]
fn query_dsl_gaps_query_string_keeps_keyword_case() {
    let qs = |q: &str| sorted(ids(json!({"query": {"query_string": {"query": q}}})));
    // tag is a keyword: "animal", "animal", "pet", "Pet"
    assert_eq!(qs("tag:Pe*"), ["4"]);
    assert_eq!(qs("tag:P?t"), ["4"]);
    assert_eq!(qs("tag:pe*"), ["3"]);
    // text fields' prefixes and wildcards are lowercased like their terms
    assert_eq!(qs("title:QUI*"), ["1", "2"]);
    assert_eq!(qs("title:L?ZY"), ["3"]);
}

#[test]
fn query_dsl_gaps_query_string_boosts() {
    let idx = index();
    let scores = |q: &str| {
        let body = json!({"query": {"query_string": {"query": q}}});
        let r = search(&idx, &SearchRequest::parse(&body).unwrap());
        let mut s: Vec<_> = (r.hits.iter())
            .map(|h| (h.id.to_owned(), h.score.unwrap()))
            .collect();
        s.sort_by(|a, b| a.0.cmp(&b.0));
        s
    };
    let base = scores("title:quick");
    let times = |q: &str, k: f32| {
        let got = scores(q);
        assert_eq!(got.len(), base.len(), "{q}");
        for ((id, s), (_, b)) in got.iter().zip(&base) {
            assert!((s - b * k).abs() < 1e-5, "{q} {id}: {s} vs {b} * {k}");
        }
    };
    times("title:quick^2", 2.0);
    times("(title:quick)^0.5", 0.5);
    times("title:(quick)^3", 3.0);
    let pet = scores("tag:pet")[0].1;
    assert_eq!(scores("tag:pet^4"), [("3".to_owned(), pet * 4.0)]);
    for q in ["quick^", "quick^x"] {
        let err = query_string_error(q);
        assert!(
            err.msg.starts_with(&format!("Cannot parse '{q}'")),
            "{q}: {err:?}"
        );
    }
}

#[test]
fn query_dsl_gaps_ranges_on_text_compare_terms() {
    let q = |q: Value| sorted(ids(json!({ "query": q })));
    // a text field's range matches its analyzed terms, here "rabbits"
    assert_eq!(
        q(json!({"range": {"title": {"gte": "r", "lt": "s"}}})),
        ["2"]
    );
    // query_string normalizes text fields' bounds, but not keyword fields'
    let qs = |s: &str| q(json!({"query_string": {"query": s}}));
    assert_eq!(qs("title:[R TO S}"), ["2"]);
    assert_eq!(qs("title:>=RAB"), ["1", "2"]);
    assert_eq!(qs("tag:[P TO Q]"), ["4"]);
}

#[test]
fn query_dsl_gaps_multi_match_field_boosts() {
    let idx = index();
    let scores = |q: Value| {
        let r = search(&idx, &SearchRequest::parse(&json!({ "query": q })).unwrap());
        let mut s: Vec<_> = (r.hits.iter())
            .map(|h| (h.id.to_owned(), h.score.unwrap()))
            .collect();
        s.sort_by(|a, b| a.0.cmp(&b.0));
        s
    };
    let base = scores(json!({"match": {"title": "quick"}}));
    let triple: Vec<_> = base.iter().map(|(id, s)| (id.clone(), s * 3.0)).collect();
    let mm = |fields: Value, want: &[(String, f32)]| {
        let got = scores(json!({"multi_match": {"query": "quick", "fields": fields}}));
        assert_eq!(got.len(), want.len());
        for ((id, s), (wid, w)) in got.iter().zip(want) {
            assert!(id == wid && (s - w).abs() < 1e-5, "{got:?} vs {want:?}");
        }
    };
    mm(json!(["title^3"]), &triple);
    mm(json!(["title^3", "tag"]), &triple);
    mm(json!(["title^1"]), &base);
    let err = parse_query(&json!({"multi_match": {"query": "q", "fields": ["title^x"]}}));
    assert!(err.is_err());
}

#[test]
fn query_dsl_gaps_simple_query_string_field_boosts() {
    let idx = index();
    let scores = |q: Value| {
        let r = search(&idx, &SearchRequest::parse(&json!({ "query": q })).unwrap());
        let mut s: Vec<_> = (r.hits.iter())
            .map(|h| (h.id.to_owned(), h.score.unwrap()))
            .collect();
        s.sort_by(|a, b| a.0.cmp(&b.0));
        s
    };
    let base = scores(json!({"match": {"title": "quick"}}));
    let got =
        scores(json!({"simple_query_string": {"query": "quick", "fields": ["title^2", "tag"]}}));
    assert_eq!(got.len(), base.len());
    for ((id, s), (bid, b)) in got.iter().zip(&base) {
        assert!(
            id == bid && (s - b * 2.0).abs() < 1e-5,
            "{got:?} vs {base:?}"
        );
    }
}

/// Lucene's SloppyPhraseMatcher keeps a repeated phrase term off positions its repeats hold.
#[test]
fn query_dsl_gaps_sloppy_phrase_repeated_terms() {
    let mut idx = index();
    idx.put("5", json!({"title": "quick brown quick"}));
    let hits = |slop: u32| {
        let q =
            json!({"query": {"match_phrase": {"title": {"query": "quick quick", "slop": slop}}}});
        let r = search(&idx, &SearchRequest::parse(&q).unwrap());
        let mut v: Vec<_> = (r.hits.iter())
            .map(|h| (h.id.to_owned(), h.score.unwrap()))
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    };
    // one "quick" (document 1) cannot stand for both
    let two = hits(2);
    assert_eq!(
        two.iter().map(|h| h.0.as_str()).collect::<Vec<_>>(),
        ["2", "5"]
    );
    assert!(hits(0).iter().all(|h| h.0 == "2"));
    // positions 0 and 2 match one apart, no further
    assert_eq!(hits(1).len(), 2);
    assert!(two[1].1 < two[0].1, "{two:?}");
}

/// ES's `resolveMappingFields`: a field pattern queries every mapped field it matches, and no
/// fields means `index.query.default_field`, `*`, queried leniently.
#[test]
fn query_dsl_gaps_field_patterns_and_all_fields() {
    let mut idx = Index::default();
    for (id, doc) in [
        (
            "1",
            json!({"title": "quick fox", "body": "lazy", "d": "2020-01-01"}),
        ),
        ("2", json!({"title": "dog", "body": "quick quick", "n": 3})),
        ("3", json!({"other": "quick", "ok": true})),
    ] {
        idx.map_dynamic(id, &doc).unwrap();
        idx.put(id, doc);
    }
    let run = |q: Value| {
        let r = search(&idx, &SearchRequest::parse(&json!({ "query": q })).unwrap());
        let mut v: Vec<_> = (r.hits.iter())
            .map(|h| (h.id.to_owned(), h.score.unwrap()))
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    };
    let ids = |q: Value| run(q).into_iter().map(|h| h.0).collect::<Vec<_>>();
    let mm = |fields: Value| json!({"multi_match": {"query": "quick", "fields": fields}});
    assert_eq!(ids(mm(json!(["t*"]))), ["1"]);
    assert_eq!(ids(mm(json!(["nothing*"]))), Vec::<String>::new());
    // `title.keyword` holds "quick fox", so only the text fields match
    assert_eq!(ids(mm(json!(["title*", "b*"]))), ["1", "2"]);
    let body = run(json!({"match": {"body": "quick"}}));
    let boosted = run(mm(json!(["title", "bo*^2"])));
    assert!((boosted[1].1 - 2.0 * body[0].1).abs() < 1e-5, "{boosted:?}");
    // a field two patterns match takes both boosts
    let twice = run(mm(json!(["bo*^2", "*dy^3"])));
    assert!((twice[0].1 - 6.0 * body[0].1).abs() < 1e-5, "{twice:?}");
    // all fields, leniently: "quick" is no date, number or boolean
    let all = json!({"multi_match": {"query": "quick"}});
    assert_eq!(ids(all), ["1", "2", "3"]);
    assert_eq!(
        ids(json!({"query_string": {"query": "quick"}})),
        ["1", "2", "3"]
    );
    assert_eq!(
        ids(json!({"query_string": {"query": "*:quick"}})),
        ["1", "2", "3"]
    );
    assert_eq!(
        ids(json!({"query_string": {"query": "qu*"}})),
        ["1", "2", "3"]
    );
    assert_eq!(
        ids(json!({"query_string": {"query": "\"quick fox\""}})),
        ["1"]
    );
    assert_eq!(
        ids(json!({"query_string": {"query": "n:3 AND quick"}})),
        ["2"]
    );
}

#[test]
fn error_shapes_a_query_failing_on_some_indices_fails_only_their_shards() {
    let (d, plain) = (dates(), index());
    let req = SearchRequest::parse(&json!({"query": {"range": {"date": {"gte": "bogus"}}}}));
    let req = req.unwrap();
    let r = es_query::search_many(&[("d", 0, &d, None), ("p", 0, &plain, None)], &req).unwrap();
    let failed: Vec<_> = r.failures.iter().map(|(n, _, e)| (*n, e.kind)).collect();
    assert_eq!(failed, [("d", es_query::ErrorKind::Parse)]);
    // failing everywhere, the error lists each index's failure
    let e = es_query::search_many(&[("d", 0, &d, None), ("e", 0, &d, None)], &req).unwrap_err();
    let shards: Vec<_> = e
        .shards
        .iter()
        .map(|(n, _, s)| (n.as_str(), s.msg == e.msg))
        .collect();
    assert_eq!(shards, [("d", true), ("e", true)]);
}

#[test]
fn search_after_needs_one_value_per_sort_field() {
    let msg = |body| SearchRequest::parse(&body).unwrap_err().msg;
    assert_eq!(
        msg(json!({"search_after": [1]})),
        "Sort must contain at least one field."
    );
    assert_eq!(
        msg(json!({"sort": ["n"], "search_after": [1, 2]})),
        "search_after has 2 value(s) but sort has 1."
    );
}

#[test]
fn search_after_keeps_hits_sorted_after_its_values() {
    let idx = index();
    let req = SearchRequest::parse(&json!({"sort": ["n"], "search_after": [5]})).unwrap();
    let r = search(&idx, &req);
    let ids: Vec<_> = r.hits.iter().map(|h| h.id).collect();
    assert_eq!((ids, r.total), (vec!["4", "2"], 4));
}

#[test]
fn doc_sort_values_are_doc_ids() {
    let idx = index();
    let req = SearchRequest::parse(&json!({"sort": [{"_doc": "desc"}, "_shard_doc"]})).unwrap();
    let r = search(&idx, &req);
    let hits: Vec<_> = r.hits.iter().map(|h| (h.id, h.sort.clone())).collect();
    let want = [("4", 3), ("3", 2), ("2", 1), ("1", 0)];
    assert_eq!(hits, want.map(|(id, d)| (id, vec![json!(d), json!(d)])));
}

#[test]
fn max_score_only_for_a_sort_by_score_alone() {
    let idx = index();
    let max = |sort| {
        let req =
            SearchRequest::parse(&json!({"query": {"match": {"title": "dog"}}, "sort": sort}));
        search(&idx, &req.unwrap()).max_score.is_some()
    };
    assert!(max(json!(["_score"])));
    assert!(!max(json!(["_score", "n"])));
    assert!(!max(json!([{"_score": "asc"}])));
}

#[test]
fn point_in_time_sorts_break_ties_by_shard_doc() {
    let sorts = |body: Value| -> Vec<String> {
        let req = SearchRequest::parse(&body).unwrap();
        req.sort.into_iter().map(|f| f.field).collect()
    };
    let pit = json!({"id": "x"});
    assert_eq!(
        sorts(json!({"pit": pit, "sort": ["n"]})),
        ["n", "_shard_doc"]
    );
    assert_eq!(
        sorts(json!({"pit": pit, "sort": ["n"], "search_after": [1, 2]})),
        ["n", "_shard_doc"]
    );
    // search_after without a tiebreak value, an explicit _shard_doc, or no sort: unchanged
    assert_eq!(
        sorts(json!({"pit": pit, "sort": ["n"], "search_after": [1]})),
        ["n"]
    );
    assert_eq!(
        sorts(json!({"pit": pit, "sort": ["_shard_doc"]})),
        ["_shard_doc"]
    );
    assert!(sorts(json!({"pit": pit})).is_empty());
}

#[test]
fn an_aggregation_without_a_field_is_an_illegal_argument() {
    // As ES 9.5.5's AggregatorFactories parser reports it (recorded via _security/_query/api_key).
    let e = SearchRequest::parse(&json!({"aggs": {"x": {"terms": {}}}})).unwrap_err();
    let reason = "Required one of fields [field, script], but none were specified.";
    assert_eq!(
        (e.kind, e.msg.as_str()),
        (es_query::ErrorKind::IllegalArgument, reason)
    );
}

/// Keyword values over `ignore_above` are not indexed, so queries answer the same before and
/// after a save and reopen, as Elasticsearch 9.5.5 answers them (recorded by hand).
#[test]
fn ignore_above_values_are_not_indexed_in_memory_or_on_disk() {
    let keyword = || FieldMapping::Keyword {
        ignore_above: Some(3),
        split_queries_on_whitespace: false,
    };
    let build = || {
        let mut idx = Index::default();
        let props = &mut idx.mappings.properties;
        props.insert("k".into(), keyword());
        let fields = [("keyword".to_owned(), keyword())].into();
        props.insert(
            "s".into(),
            FieldMapping::Text {
                store: false,
                fields,
            },
        );
        idx.put("1", json!({"k": ["abc", "abcd"], "s": "long text"}));
        idx.put("2", json!({"k": "ab", "s": "ab"}));
        idx.put("3", json!({"k": "abcde", "s": "abcde"}));
        idx.put("4", json!({"k": "abc"}));
        idx
    };
    let idx = build();
    let dir = std::env::temp_dir().join(format!("esq-ignore-above-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    es_index::disk::save(&dir, &es_index::Indices::from([("idx".into(), build())])).unwrap();
    let opened = es_index::disk::open(&dir).unwrap().remove("idx").unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    let hits = |idx: &Index, q: Value| {
        let req = SearchRequest::parse(&json!({"query": q})).unwrap();
        let mut hits: Vec<_> = (search(idx, &req).hits.iter())
            .map(|h| (h.id.to_owned(), (h.score.unwrap() * 1e5).round() / 1e5))
            .collect();
        hits.sort_by(|a, b| a.partial_cmp(b).unwrap());
        hits
    };
    let h = |hits: &[(&str, f32)]| -> Vec<_> {
        hits.iter().map(|(id, s)| (id.to_string(), *s)).collect()
    };
    for (q, want) in [
        (json!({"term": {"k": "abcd"}}), h(&[])),
        (json!({"match": {"k": "abcde"}}), h(&[])),
        (
            json!({"term": {"k": "abc"}}),
            h(&[("1", 0.47), ("4", 0.47)]),
        ),
        (
            json!({"bool": {"should": [{"term": {"k": "abc"}}, {"term": {"k": "ab"}}]}}),
            h(&[("1", 0.47), ("2", 0.98083), ("4", 0.47)]),
        ),
        (json!({"match": {"s.keyword": "ab"}}), h(&[("2", 0.28768)])),
        (json!({"match": {"s.keyword": "abcde"}}), h(&[])),
        (json!({"prefix": {"k": "abcd"}}), h(&[])),
        (json!({"wildcard": {"k": "abcd*"}}), h(&[])),
        (json!({"range": {"k": {"gte": "abcd"}}}), h(&[])),
        (
            json!({"exists": {"field": "k"}}),
            h(&[("1", 1.0), ("2", 1.0), ("4", 1.0)]),
        ),
        // `_ignored` lists the fields that left a value out
        (
            json!({"term": {"_ignored": "k"}}),
            h(&[("1", 0.22920), ("3", 0.22920)]),
        ),
        (
            json!({"match": {"_ignored": "s.keyword"}}),
            h(&[("1", 0.22920), ("3", 0.22920)]),
        ),
        (
            json!({"terms": {"_ignored": ["k"]}}),
            h(&[("1", 1.0), ("3", 1.0)]),
        ),
        (
            json!({"exists": {"field": "_ignored"}}),
            h(&[("1", 1.0), ("3", 1.0)]),
        ),
    ] {
        assert_eq!(hits(&idx, q.clone()), want, "in memory: {q}");
        assert_eq!(hits(&opened, q.clone()), want, "reopened: {q}");
    }
    let agg = json!({"size": 0, "aggs": {"a": {"terms": {"field": "_ignored"}}}});
    let req = SearchRequest::parse(&agg).unwrap();
    let want = json!({"a": {"doc_count_error_upper_bound": 0, "sum_other_doc_count": 0,
        "buckets": [{"key": "k", "doc_count": 2}, {"key": "s.keyword", "doc_count": 2}]}});
    assert_eq!(search(&idx, &req).aggregations, Some(want.clone()));
    assert_eq!(search(&opened, &req).aggregations, Some(want));
}

/// Sorts and aggregations read doc values, which leave keyword values over `ignore_above`
/// (counted in UTF-16 units, as Java's `String.length`) out, before and after a reopen.
#[test]
fn ignore_above_values_have_no_doc_values_in_memory_or_on_disk() {
    let build = || {
        let mut idx = Index::default();
        let keyword = FieldMapping::Keyword {
            ignore_above: Some(3),
            split_queries_on_whitespace: false,
        };
        idx.mappings.properties.insert("k".into(), keyword);
        idx.put("1", json!({"k": ["abc", "abcd"]}));
        idx.put("2", json!({"k": "ab"}));
        idx.put("3", json!({"k": "abcde"}));
        // two chars, four UTF-16 units
        idx.put("4", json!({"k": "😀😀"}));
        idx
    };
    let dir = std::env::temp_dir().join(format!("esq-ignore-above-dv-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    es_index::disk::save(&dir, &es_index::Indices::from([("idx".into(), build())])).unwrap();
    let opened = es_index::disk::open(&dir).unwrap().remove("idx").unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    let sorted = |idx: &Index, order: &str| {
        let req = SearchRequest::parse(&json!({"sort": [{"k": order}]})).unwrap();
        (search(idx, &req).hits.iter())
            .map(|h| (h.id.to_owned(), h.sort.clone()))
            .collect::<Vec<_>>()
    };
    let hits = |hits: [(&str, Value); 4]| hits.map(|(id, v)| (id.to_owned(), vec![v]));
    // as Elasticsearch 9.5.5 answers (checked live)
    let (ab, abc) = (json!("ab"), json!("abc"));
    for idx in [&build(), &opened] {
        let asc = [
            ("2", ab.clone()),
            ("1", abc.clone()),
            ("3", Value::Null),
            ("4", Value::Null),
        ];
        assert_eq!(sorted(idx, "asc"), hits(asc));
        let desc = [
            ("1", abc.clone()),
            ("2", ab.clone()),
            ("3", Value::Null),
            ("4", Value::Null),
        ];
        assert_eq!(sorted(idx, "desc"), hits(desc));
        let req =
            SearchRequest::parse(&json!({"size": 0, "aggs": {"a": {"terms": {"field": "k"}}}}))
                .unwrap();
        let want = json!({"a": {"doc_count_error_upper_bound": 0, "sum_other_doc_count": 0,
            "buckets": [{"key": "ab", "doc_count": 1}, {"key": "abc", "doc_count": 1}]}});
        assert_eq!(search(idx, &req).aggregations, Some(want));
    }
}

#[test]
fn error_positions_case_insensitive_term_needs_a_term_based_field() {
    let mut i = index();
    let date = FieldMapping::Date {
        format: None,
        locale: None,
        fields: Default::default(),
    };
    i.mappings.properties.insert("d".into(), date);
    i.put("5", json!({"d": "2020-01-01"}));
    let run = |f: &str, v: Value| {
        let q = json!({"term": {f: {"value": v, "case_insensitive": true}}});
        let req = SearchRequest::parse(&json!({ "query": q })).unwrap();
        es_query::search_many(&[("", 0, &i, None)], &req).map(|r| r.total)
    };
    for (f, v, ty) in [("n", json!(5), "long"), ("d", json!("2020-01-01"), "date")] {
        assert_eq!(
            run(f, v).unwrap_err().msg,
            format!(
                "[{f}] field which is of type [{ty}], does not support case insensitive term queries"
            ),
        );
    }
    assert_eq!(run("ok", json!(true)).unwrap(), 1);
    assert_eq!(run("tag", json!("PET")).unwrap(), 2);
    assert_eq!(run("nope", json!("x")).unwrap(), 0);
}

#[test]
fn error_positions_malformed_decimal_patterns_fail_the_shard() {
    let mut i = index();
    i.mappings.properties.insert("n".into(), FieldMapping::Long);
    let run = |aggs: Value| {
        let req = SearchRequest::parse(&json!({"size": 0, "aggs": aggs})).unwrap();
        es_query::search_many(&[("", 0, &i, None)], &req).map(|r| r.total)
    };
    let range = |field: &str, p: &str| json!({"a": {"range": {"field": field, "ranges": [{"to": 3}], "format": p}}});
    let histogram = |field: &str, p: &str| json!({"a": {"histogram": {"field": field, "interval": 1, "format": p}}});
    let top_hits = |field: &str, p: &str| json!({"a": {"top_hits": {"docvalue_fields": [{"field": field, "format": p}]}}});
    // reasons as Elasticsearch 9.5.5 (Java's DecimalFormat.applyPattern) gives them
    for (aggs, reason) in [
        (
            range("n", "#.#.#"),
            "Multiple decimal separators in pattern \"#.#.#\"",
        ),
        (range("nope", "0#"), "Malformed pattern \"0#\""),
        (range("n", "#.0#0"), "Unexpected '0' in pattern \"#.0#0\""),
        (
            range("n", "%%#"),
            "Too many percent/per mille characters in pattern \"%%#\"",
        ),
        (range("n", "'abc"), "Malformed pattern \"'abc\""),
        (range("n", "#,#.#,#"), "Malformed pattern \"#,#.#,#\""),
        (range("n", "#,"), "Malformed pattern \"#,\""),
        (range("n", "#E"), "Malformed exponential pattern \"#E\""),
        (
            range("n", "#;#;#"),
            "Unquoted special character ';' in pattern \"#;#;#\"",
        ),
        (
            range("n", ";#"),
            "Unquoted special character ';' in pattern \";#\"",
        ),
        (range("n", "#;'abc"), "Malformed pattern \"#;'abc\""),
        (
            range("n", "0E0#E0"),
            "Multiple exponential symbols in pattern \"0E0#E0\"",
        ),
        (range("n", "##0#.#"), "Malformed pattern \"##0#.#\""),
        (histogram("n", "#.#,#"), "Malformed pattern \"#.#,#\""),
        (
            histogram("nope", "#E"),
            "Malformed exponential pattern \"#E\"",
        ),
        (top_hits("n", "#,"), "Malformed pattern \"#,\""),
    ] {
        let got = run(aggs.clone()).map_err(|e| (e.shards.len(), e.msg));
        assert_eq!(got, Err((1, reason.into())), "{aggs}");
    }
    let sub = json!({"t": {"terms": {"field": "tag"}, "aggs": histogram("n", "#,")}});
    assert_eq!(run(sub).unwrap_err().msg, "Malformed pattern \"#,\"");
    for p in ["#E0E0", "#;", "#.##E0#", "x#y#", ".#"] {
        assert_eq!(run(range("n", p)).unwrap(), 4, "{p}");
    }
    assert_eq!(run(top_hits("nope", "#.#.#")).unwrap(), 4);
}
