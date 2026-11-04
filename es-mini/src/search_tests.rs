use crate::{app, indices_tests::call};
use axum::http::{Method, StatusCode};
use serde_json::json;

async fn seeded() -> axum::Router {
    let app = app();
    call(
        &app,
        Method::PUT,
        "/idx/_doc/1",
        Some(json!({"t": "red apple", "n": 2})),
    )
    .await;
    call(
        &app,
        Method::PUT,
        "/idx/_doc/2",
        Some(json!({"t": "green apple", "n": 1})),
    )
    .await;
    call(
        &app,
        Method::PUT,
        "/idx/_doc/3",
        Some(json!({"t": "pear", "n": 3})),
    )
    .await;
    app
}

#[tokio::test]
async fn search_envelope() {
    let app = seeded().await;
    let (s, r) = call(
        &app,
        Method::POST,
        "/idx/_search",
        Some(json!({"query": {"match": {"t": "apple"}}})),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(r["timed_out"], false);
    assert!(r["took"].is_u64());
    assert_eq!(
        r["_shards"],
        json!({"total": 1, "successful": 1, "skipped": 0, "failed": 0})
    );
    assert_eq!(r["hits"]["total"], json!({"value": 2, "relation": "eq"}));
    assert!(r["hits"]["max_score"].as_f64().unwrap() > 0.0);
    let hit = &r["hits"]["hits"][0];
    assert_eq!(hit["_index"], "idx");
    assert!(hit["_score"].as_f64().unwrap() > 0.0);
    assert!(hit["_source"]["t"].as_str().unwrap().contains("apple"));
}

#[tokio::test]
async fn get_without_body_and_sorted_hits() {
    let app = seeded().await;
    let (s, r) = call(&app, Method::GET, "/idx/_search", None).await;
    assert_eq!(
        (s, &r["hits"]["total"]["value"]),
        (StatusCode::OK, &json!(3))
    );

    let (_, r) = call(
        &app,
        Method::POST,
        "/idx/_search",
        Some(json!({"sort": [{"n": "desc"}], "size": 2})),
    )
    .await;
    assert_eq!(r["hits"]["max_score"], json!(null));
    let hits = r["hits"]["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 2);
    assert_eq!(
        (&hits[0]["_id"], &hits[0]["_score"], &hits[0]["sort"]),
        (&json!("3"), &json!(null), &json!([3]))
    );
}

#[tokio::test]
async fn count_agrees_with_total() {
    let app = seeded().await;
    let q = json!({"query": {"range": {"n": {"gte": 2}}}});
    let (s, r) = call(&app, Method::POST, "/idx/_count", Some(q.clone())).await;
    assert_eq!((s, &r["count"]), (StatusCode::OK, &json!(2)));
    assert_eq!(r["_shards"]["total"], 1);
    let (_, r) = call(&app, Method::POST, "/idx/_search", Some(q)).await;
    assert_eq!(r["hits"]["total"]["value"], 2);
    let (_, r) = call(&app, Method::GET, "/idx/_count", None).await;
    assert_eq!(r["count"], 3);
}

#[tokio::test]
async fn errors() {
    let app = seeded().await;
    let (s, r) = call(&app, Method::POST, "/nope/_search", Some(json!({}))).await;
    assert_eq!(
        (s, &r["error"]["type"]),
        (StatusCode::NOT_FOUND, &json!("index_not_found_exception"))
    );
    let (s, r) = call(
        &app,
        Method::POST,
        "/idx/_search",
        Some(json!({"query": {"bogus": {}}})),
    )
    .await;
    assert_eq!(
        (s, &r["error"]["type"]),
        (StatusCode::BAD_REQUEST, &json!("parsing_exception"))
    );
    assert_eq!(r["error"]["reason"], "unknown query [bogus]");
    let (s, _) = call(
        &app,
        Method::POST,
        "/idx/_count",
        Some(json!({"query": {"bogus": {}}})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn esql_query() {
    let app = seeded().await;
    let body = json!({"query": "FROM idx | WHERE n >= 2 | SORT n | KEEP t, n"});
    let (s, r) = call(&app, Method::POST, "/_query", Some(body)).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        r["columns"],
        json!([{"name": "t", "type": "text"}, {"name": "n", "type": "long"}])
    );
    assert_eq!(r["values"], json!([["red apple", 2], ["pear", 3]]));
    assert_eq!(r["is_partial"], false);
    assert!(r["took"].is_u64());

    let (s, r) = call(
        &app,
        Method::POST,
        "/_query",
        Some(json!({"query": "FROM idx | BOGUS"})),
    )
    .await;
    assert_eq!(
        (s, &r["error"]["type"]),
        (StatusCode::BAD_REQUEST, &json!("parsing_exception"))
    );
    let (s, r) = call(
        &app,
        Method::POST,
        "/_query",
        Some(json!({"query": "FROM idx | KEEP zz"})),
    )
    .await;
    assert_eq!(
        (s, &r["error"]["type"]),
        (StatusCode::BAD_REQUEST, &json!("verification_exception"))
    );
    let (s, _) = call(&app, Method::POST, "/_query", Some(json!({}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

/// An evaluation error fails the query with Elasticsearch 9.5.5's status, type and reason.
#[tokio::test]
async fn esql_es_semantics_evaluation_failure_response() {
    let query = "ROW n = -1 | EVAL s = SUBSTRING(\"abc\", 1, n)";
    let (s, r) = call(
        &app(),
        Method::POST,
        "/_query",
        Some(json!({"query": query})),
    )
    .await;
    let cause = json!({
        "type": "illegal_argument_exception",
        "reason": "Length parameter cannot be negative, found [-1]",
    });
    let mut error = cause.clone();
    error["root_cause"] = json!([cause]);
    assert_eq!(
        (s, r),
        (
            StatusCode::BAD_REQUEST,
            json!({"error": error, "status": 400})
        )
    );
}

#[tokio::test]
async fn profile_times_each_aggregation_per_shard() {
    let app = seeded().await;
    let aggs = json!({
        "n": {"cardinality": {"field": "n"}},
        "k": {"cardinality": {"field": "t.keyword"}},
        "d": {"cardinality": {"field": "t.keyword", "execution_hint": "direct"}},
        "a": {"avg": {"field": "n"}}
    });
    let body = json!({"profile": true, "size": 0, "aggs": aggs});
    let (s, r) = call(&app, Method::POST, "/idx/_search", Some(body)).await;
    assert_eq!(s, StatusCode::OK, "{r}");
    let shard = &r["profile"]["shards"][0];
    assert_eq!(shard["id"], "[es-mini-node][idx][0]");
    assert_eq!(shard["index"], "idx");
    assert_eq!(shard["shard_id"], 0);
    let profiled = shard["aggregations"].as_array().unwrap();
    let types: Vec<_> = profiled
        .iter()
        .map(|a| (&a["description"], &a["type"]))
        .collect();
    assert_eq!(
        types,
        [
            (&json!("n"), &json!("CardinalityAggregator")),
            (&json!("k"), &json!("GlobalOrdCardinalityAggregator")),
            (&json!("d"), &json!("CardinalityAggregator")),
            (&json!("a"), &json!("AvgAggregator")),
        ]
    );
    for a in profiled {
        let b = &a["breakdown"];
        for phase in [
            "initialize",
            "build_leaf_collector",
            "collect",
            "post_collection",
            "build_aggregation",
        ] {
            assert!(b[phase].as_u64().unwrap() > 0, "{phase} of {a}");
        }
        assert_eq!(b["collect_count"], 3);
        assert_eq!(b["reduce"], 0);
    }
    assert_eq!(profiled[2]["debug"]["string_hashing_collectors_used"], 1);
    assert_eq!(profiled[0]["debug"]["numeric_collectors_used"], 1);
    assert_eq!(r["aggregations"]["n"]["value"], 3);

    let (_, r) = call(
        &app,
        Method::POST,
        "/idx/_search",
        Some(json!({"aggs": aggs})),
    )
    .await;
    assert!(r.get("profile").is_none());
}

#[tokio::test]
async fn profile_numeric_terms_debug_counts_shard_buckets() {
    let app = seeded().await;
    let aggs = json!({"n": {"terms": {"field": "n", "size": 1, "shard_size": 1}}});
    let body = json!({"profile": true, "size": 0, "aggs": aggs});
    let (s, r) = call(&app, Method::POST, "/idx/_search", Some(body)).await;
    assert_eq!(s, StatusCode::OK, "{r}");
    let a = &r["profile"]["shards"][0]["aggregations"][0];
    assert_eq!(a["type"], "NumericTermsAggregator");
    // every collected value, not only the shard_size kept
    assert_eq!(
        a["debug"],
        json!({"result_strategy": "long_terms", "total_buckets": 3})
    );
}

#[tokio::test]
async fn profile_histogram_counts_collected_buckets() {
    let app = seeded().await;
    let aggs = json!({"h": {"histogram": {"field": "n", "interval": 1}}});
    let body = json!({"profile": true, "size": 0, "aggs": aggs});
    let (s, r) = call(&app, Method::POST, "/idx/_search", Some(body)).await;
    assert_eq!(s, StatusCode::OK, "{r}");
    let a = &r["profile"]["shards"][0]["aggregations"][0];
    assert_eq!(a["type"], "NumericHistogramAggregator");
    assert_eq!(a["debug"], json!({"total_buckets": 3}));
}

#[tokio::test]
async fn too_many_buckets_fails_the_search_like_es() {
    let app = seeded().await;
    let h = json!({"field": "n", "interval": 1, "extended_bounds": {"min": 0, "max": 1e9}});
    let body = json!({"size": 0, "aggs": {"h": {"histogram": h}}});
    let (s, r) = call(&app, Method::POST, "/idx/_search", Some(body)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{r}");
    assert_eq!(r["error"]["type"], "search_phase_execution_exception");
    assert_eq!(r["error"]["root_cause"], json!([]));
    let cause = &r["error"]["caused_by"];
    assert_eq!(cause["type"], "too_many_buckets_exception");
    assert_eq!(cause["max_buckets"], 65_536);
}

#[tokio::test]
async fn profile_map_terms_defers_sub_aggregations_not_sorted_on() {
    let app = seeded().await;
    let aggs = json!({"k": {
        "terms": {"field": "t.keyword", "execution_hint": "map", "order": {"s": "desc"}},
        "aggs": {"m": {"max": {"field": "n"}}, "s": {"sum": {"field": "n"}}}
    }});
    let body = json!({"profile": true, "size": 0, "aggs": aggs});
    let (s, r) = call(&app, Method::POST, "/idx/_search", Some(body)).await;
    assert_eq!(s, StatusCode::OK, "{r}");
    let a = &r["profile"]["shards"][0]["aggregations"][0];
    assert_eq!(a["type"], "MapStringTermsAggregator");
    assert_eq!(
        a["debug"],
        json!({
            "deferred_aggregators": ["m"],
            "total_buckets": 3,
            "collection_strategy": "from Field [t.keyword] of type [keyword]",
            "result_strategy": "terms",
        })
    );
    let children: Vec<_> = (a["children"].as_array().unwrap().iter())
        .map(|c| {
            (
                &c["type"],
                &c["description"],
                &c["breakdown"]["collect_count"],
            )
        })
        .collect();
    assert_eq!(
        children,
        [
            (&json!("MaxAggregator"), &json!("m"), &json!(3)),
            (&json!("SumAggregator"), &json!("s"), &json!(3)),
        ]
    );

    // depth_first defers nothing
    let aggs = json!({"k": {
        "terms": {"field": "n", "collect_mode": "depth_first"},
        "aggs": {"m": {"max": {"field": "n"}}}
    }});
    let body = json!({"profile": true, "size": 0, "aggs": aggs});
    let (_, r) = call(&app, Method::POST, "/idx/_search", Some(body)).await;
    let a = &r["profile"]["shards"][0]["aggregations"][0];
    assert!(a["debug"].get("deferred_aggregators").is_none(), "{a}");
}

#[tokio::test]
async fn profile_global_ordinals_terms_debug() {
    let app = seeded().await;
    let doc = json!({"t": ["plum", "fig"], "n": 4});
    call(&app, Method::PUT, "/idx/_doc/4", Some(doc)).await;
    let terms = json!({"field": "t.keyword", "collect_mode": "breadth_first"});
    let inner = json!({"terms": terms, "aggs": {"m": {"max": {"field": "n"}}}});
    let aggs = json!({"o": {"terms": {"field": "n"}, "aggs": {"k": inner}}});
    let body = json!({"profile": true, "size": 0, "aggs": aggs});
    let (s, r) = call(&app, Method::POST, "/idx/_search", Some(body)).await;
    assert_eq!(s, StatusCode::OK, "{r}");
    let a = &r["profile"]["shards"][0]["aggregations"][0]["children"][0];
    assert_eq!(a["type"], "GlobalOrdinalsStringTermsAggregator");
    assert_eq!(
        a["debug"],
        json!({
            "deferred_aggregators": ["m"],
            "collection_strategy": "remap using many bucket ords",
            "total_buckets": 5,
            "result_strategy": "terms",
            "segments_with_single_valued_ords": 0,
            "segments_with_multi_valued_ords": 1,
            "has_filter": false,
        })
    );

    // single-valued matches, an exclude
    let terms = json!({"field": "t.keyword", "exclude": ["pear"]});
    let aggs = json!({"o": {"terms": {"field": "n"}, "aggs": {"k": {"terms": terms}}}});
    let body = json!({"profile": true, "size": 0, "query": {"range": {"n": {"lt": 4}}},
        "aggs": aggs});
    let (_, r) = call(&app, Method::POST, "/idx/_search", Some(body)).await;
    let d = &r["profile"]["shards"][0]["aggregations"][0]["children"][0]["debug"];
    assert_eq!(d["segments_with_single_valued_ords"], 1, "{d}");
    assert_eq!(d["segments_with_multi_valued_ords"], 0);
    assert_eq!(d["has_filter"], true);
}

#[tokio::test]
async fn profile_top_level_keyword_terms_run_filter_by_filter() {
    let app = seeded().await;
    let terms = json!({"field": "t.keyword", "exclude": ["pear"]});
    let body = json!({"profile": true, "size": 0, "aggs": {"k": {"terms": terms}}});
    let (s, r) = call(&app, Method::POST, "/idx/_search", Some(body.clone())).await;
    assert_eq!(s, StatusCode::OK, "{r}");
    let a = &r["profile"]["shards"][0]["aggregations"][0];
    assert_eq!(a["type"], "StringTermsAggregatorFromFilters");
    assert_eq!(a["breakdown"]["collect_count"], 0);
    let filter = |q: &str| json!({"query": q, "segments_counted_in_constant_time": 1});
    assert_eq!(
        a["debug"],
        json!({
            "delegate": "FilterByFilterAggregator",
            "delegate_debug": {
                "filters": [filter("t.keyword:green apple"), filter("t.keyword:red apple")],
                "segments_counted": 1,
                "segments_collected": 0,
                "segments_with_deleted_docs": 0,
                "segments_with_doc_count_field": 0,
            },
        })
    );

    // a top-level query other than match_all keeps the native aggregator
    let mut body = body;
    body["query"] = json!({"range": {"n": {"lt": 4}}});
    let (_, r) = call(&app, Method::POST, "/idx/_search", Some(body)).await;
    let a = &r["profile"]["shards"][0]["aggregations"][0];
    assert_eq!(a["type"], "GlobalOrdinalsStringTermsAggregator");
}

#[tokio::test]
async fn value_type_mismatch_fails_only_that_shard() {
    let app = app();
    for (index, ty) in [("a", "keyword"), ("b", "ip")] {
        let mappings = json!({"mappings": {"properties": {"ip": {"type": ty}}}});
        call(&app, Method::PUT, &format!("/{index}"), Some(mappings)).await;
        let doc = json!({"ip": "127.0.0.1"});
        call(&app, Method::PUT, &format!("/{index}/_doc/1"), Some(doc)).await;
    }
    let aggs = json!({"aggs": {"t": {"terms": {"field": "ip", "value_type": "ip"}}}});
    let (s, r) = call(&app, Method::POST, "/a,b/_search", Some(aggs.clone())).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(r["_shards"]["successful"], 1);
    assert_eq!(r["_shards"]["failed"], 1);
    assert_eq!(
        r["_shards"]["failures"][0]["reason"],
        json!({"type": "illegal_argument_exception", "reason": "Field type [keyword] is incompatible with specified value_type [ip]"})
    );
    assert_eq!(r["_shards"]["failures"][0]["index"], "a");
    assert_eq!(r["hits"]["total"]["value"], 1);
    assert_eq!(
        r["aggregations"]["t"]["buckets"],
        json!([{"key": "127.0.0.1", "doc_count": 1}])
    );
    // with every shard failed, the search fails
    let (s, _) = call(&app, Method::POST, "/a/_search", Some(aggs)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn terms_lookup_reads_terms_from_a_document() {
    let app = seeded().await;
    call(
        &app,
        Method::PUT,
        "/lists/_doc/fruit",
        Some(json!({"wanted": {"n": [1, 3]}})),
    )
    .await;
    let lookup = |id: &str| {
        json!({"size": 0, "query": {"terms": {"n": {"index": "lists", "id": id, "path": "wanted.n"}}},
            "aggs": {"f": {"filter": {"terms": {"n": {"index": "lists", "id": id, "path": "wanted.n"}}}}}})
    };
    let (s, r) = call(&app, Method::POST, "/idx/_search", Some(lookup("fruit"))).await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(r["hits"]["total"]["value"], 2);
    assert_eq!(r["aggregations"]["f"]["doc_count"], 2);
    // A missing lookup document has no terms, so it matches nothing.
    let (_, r) = call(&app, Method::POST, "/idx/_search", Some(lookup("none"))).await;
    assert_eq!(r["hits"]["total"]["value"], 0);
    let (s, _) = call(
        &app,
        Method::POST,
        "/idx/_search",
        Some(json!({"query": {"terms": {"n": {"index": "nope", "id": "x", "path": "n"}}}})),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn hits_report_the_named_queries_they_match() {
    let app = seeded().await;
    let named = |q: &str| json!({"match": {"t": {"query": q, "_name": q}}});
    let body = json!({"query": {"bool": {"should": [named("red"), named("apple")]}},
        "sort": ["n"]});
    let (_, r) = call(&app, Method::POST, "/idx/_search", Some(body)).await;
    let hits = &r["hits"]["hits"];
    assert_eq!(hits[0]["matched_queries"], json!(["apple"]));
    assert_eq!(hits[1]["matched_queries"], json!(["red", "apple"]));
    assert_eq!(hits.as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn hits_fetch_the_requested_fields() {
    let app = seeded().await;
    let body = json!({"fields": ["t", "n"], "sort": ["n"]});
    let (_, r) = call(&app, Method::POST, "/idx/_search", Some(body)).await;
    let hit = &r["hits"]["hits"][0];
    assert_eq!(hit["fields"], json!({"t": ["green apple"], "n": [1]}));
}

#[tokio::test]
async fn runtime_fields_stringify_the_source_like_java() {
    let app = seeded().await;
    let body = json!({"size": 1, "sort": ["n"], "fields": ["rt"], "runtime_mappings": {"rt": {
        "type": "keyword", "script": {"source": "emit(params['_source'].toString())"}}}});
    let (s, r) = call(&app, Method::POST, "/idx/_search", Some(body)).await;
    assert_eq!(s, StatusCode::OK, "{r}");
    let rt = &r["hits"]["hits"][0]["fields"]["rt"][0];
    assert_eq!(rt, "{t=green apple, n=1}");
}

#[tokio::test]
async fn size_zero_has_no_max_score() {
    let app = seeded().await;
    let body = json!({"size": 0, "query": {"match": {"t": "apple"}}});
    let (_, r) = call(&app, Method::POST, "/idx/_search", Some(body)).await;
    assert_eq!(r["hits"]["total"]["value"], 2);
    assert_eq!(r["hits"]["max_score"], json!(null));
}

#[tokio::test]
async fn bare_typed_keys_flag_is_true() {
    let app = seeded().await;
    let body = json!({"size": 0, "aggs": {"m": {"max": {"field": "n"}}}});
    let (_, r) = call(&app, Method::POST, "/idx/_search?typed_keys", Some(body)).await;
    assert_eq!(r["aggregations"]["max#m"]["value"], 3.0);
}

#[test]
fn searches_do_not_wait_for_writes_to_other_indices() {
    let state = std::sync::Arc::new(crate::store::Store::new(es_index::Indices::from(
        ["a", "b"].map(|n| (n.to_owned(), es_index::Index::default())),
    )));
    let app = crate::router(state.clone(), None);
    let shared = state.read();
    let writing_a = shared.write("a").unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let search_b = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let (status, _) = rt.block_on(call(&app, Method::GET, "/b/_search", None));
        tx.send(status).unwrap();
    });
    let searched = rx.recv_timeout(std::time::Duration::from_secs(10));
    drop(writing_a);
    drop(shared);
    search_b.join().unwrap();
    assert_eq!(searched, Ok(StatusCode::OK));
}

#[tokio::test]
async fn search_response_keys_in_es_order() {
    let app = seeded().await;
    let body = json!({"size": 0, "aggs": {"n": {"max": {"field": "n"}}}});
    let (s, r) = call(&app, Method::POST, "/idx/_search", Some(body)).await;
    assert_eq!(s, StatusCode::OK);
    let keys: Vec<&str> = r.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        ["took", "timed_out", "_shards", "hits", "aggregations"]
    );
}

/// Elasticsearch 9.5.5's response to a range query whose date bound fails on every index
/// (each shard reason there also has a `caused_by`, which es-mini leaves out).
#[tokio::test]
async fn error_shapes_search_failing_on_every_index_is_a_search_phase_exception() {
    let app = app();
    for index in ["t", "u"] {
        let mappings = json!({"mappings": {"properties": {"d": {"type": "date"}}}});
        call(&app, Method::PUT, &format!("/{index}"), Some(mappings)).await;
    }
    let query = json!({"query": {"range": {"d": {"gte": "foo"}}}});
    let (s, r) = call(&app, Method::POST, "/t,u/_search", Some(query)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let f = "strict_date_optional_time||epoch_millis";
    let reason = format!("failed to parse date field [foo] with format [{f}]");
    let cause = json!({"type": "parse_exception", "reason": format!("{reason}: [{reason}]")});
    let shard = |index: &str| json!({"shard": 0, "index": index, "node": crate::nodes::NODE_ID, "reason": cause});
    assert_eq!(
        r["error"],
        json!({
            "root_cause": [cause, cause],
            "type": "search_phase_execution_exception",
            "reason": "all shards failed",
            "phase": "query",
            "grouped": true,
            "failed_shards": [shard("t"), shard("u")],
        })
    );
    // an index the bound does not fail on still answers
    call(
        &app,
        Method::PUT,
        "/k",
        Some(json!({"mappings": {"properties": {"d": {"type": "keyword"}}}})),
    )
    .await;
    let query = json!({"query": {"range": {"d": {"gte": "foo"}}}});
    let (s, r) = call(&app, Method::POST, "/t,k/_search", Some(query)).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(r["_shards"]["failed"], 1);
    assert_eq!(r["_shards"]["failures"][0], shard("t"));
}

/// A 3-shard index `t` (date `d`, long `n`) with a document routed by each of `a`, `b` and `c`,
/// and a one-shard index `k` whose keyword `d` takes any range bound.
async fn routed_shard_ids_indices() -> axum::Router {
    let app = app();
    let t = json!({"settings": {"number_of_shards": 3},
        "mappings": {"properties": {"d": {"type": "date"}, "n": {"type": "long"}}}});
    call(&app, Method::PUT, "/t", Some(t)).await;
    let k = json!({"mappings": {"properties": {"d": {"type": "keyword"}}}});
    call(&app, Method::PUT, "/k", Some(k)).await;
    call(
        &app,
        Method::PUT,
        "/k/_doc/1?refresh",
        Some(json!({"d": "2024"})),
    )
    .await;
    for (n, r) in ["a", "b", "c"].into_iter().enumerate() {
        let doc = Some(json!({"d": "2024", "n": n}));
        call(
            &app,
            Method::PUT,
            &format!("/t/_doc/{r}?routing={r}&refresh"),
            doc,
        )
        .await;
    }
    app
}

/// Elasticsearch 9.5.5 routes `b` to shard 2 of 3 and names that shard in the failure.
#[tokio::test]
async fn routed_shard_ids_name_the_failed_shard() {
    let app = routed_shard_ids_indices().await;
    let query = Some(json!({"query": {"range": {"d": {"gte": "foo"}}}}));
    for path in ["/t,k/_search?routing=b", "/t,k/_search?routing=b&scroll=1m"] {
        let (s, r) = call(&app, Method::POST, path, query.clone()).await;
        assert_eq!(s, StatusCode::OK, "{r}");
        assert_eq!(r["_shards"]["total"], 2, "{r}");
        let f = &r["_shards"]["failures"][0];
        assert_eq!((&f["index"], &f["shard"]), (&json!("t"), &json!(2)), "{r}");
    }
}

/// Elasticsearch 9.5.5 profiles shard 2, where `b` routes, as `[node][t][2]`.
#[tokio::test]
async fn routed_shard_ids_name_the_profiled_shard() {
    let app = routed_shard_ids_indices().await;
    let body = json!({"size": 0, "profile": true, "aggs": {"m": {"max": {"field": "n"}}}});
    let (s, r) = call(&app, Method::POST, "/t/_search?routing=b", Some(body)).await;
    assert_eq!(s, StatusCode::OK, "{r}");
    let shards = &r["profile"]["shards"];
    assert_eq!(shards.as_array().map(Vec::len), Some(1), "{r}");
    assert_eq!(shards[0]["shard_id"], 2);
    let id = format!("[{}][t][2]", crate::nodes::NODE_ID);
    assert_eq!(shards[0]["id"], id);
}
