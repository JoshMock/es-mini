use es_index::{FieldMapping, Index};
use es_query::{SearchRequest, parse_query, search, search_many};
use serde_json::{Value, json};

fn ids(idx: &Index, query: Value) -> Result<Vec<String>, String> {
    let req = SearchRequest::parse(&json!({ "query": query })).map_err(|e| e.msg)?;
    let r = search_many(&[("i", idx, None)], &req).map_err(|e| e.msg)?;
    if let Some((_, e)) = r.failures.first() {
        return Err(e.msg.clone());
    }
    let mut ids: Vec<String> = r.hits.iter().map(|h| h.id.to_owned()).collect();
    ids.sort();
    Ok(ids)
}

fn ips() -> Index {
    let mut i = Index::default();
    i.mappings.properties.insert("ip".into(), FieldMapping::Ip);
    i.put("1", json!({"ip": "192.168.1.5"}));
    i.put("2", json!({"ip": "::ffff:10.0.0.1"}));
    i.put("3", json!({"ip": ["2001:db8::1", "8.8.8.8"]}));
    i
}

#[test]
fn ip_and_range_fields_ip_queries_normalize_addresses_and_match_cidr() {
    let i = ips();
    let q = |q: Value| ids(&i, q);
    assert_eq!(
        q(json!({"term": {"ip": "::ffff:192.168.1.5"}})).unwrap(),
        ["1"]
    );
    assert_eq!(q(json!({"term": {"ip": "10.0.0.1"}})).unwrap(), ["2"]);
    assert_eq!(q(json!({"term": {"ip": "2001:db8:0::1"}})).unwrap(), ["3"]);
    assert_eq!(q(json!({"term": {"ip": "192.168.0.0/16"}})).unwrap(), ["1"]);
    assert_eq!(q(json!({"term": {"ip": "2001:db8::/32"}})).unwrap(), ["3"]);
    assert_eq!(
        q(json!({"term": {"ip": "0.0.0.0/0"}})).unwrap(),
        ["1", "2", "3"]
    );
    assert_eq!(
        q(json!({"terms": {"ip": ["10.0.0.0/8", "8.8.8.8"]}})).unwrap(),
        ["2", "3"]
    );
    assert_eq!(q(json!({"match": {"ip": "8.8.8.8"}})).unwrap(), ["3"]);
    let range = json!({"range": {"ip": {"gte": "10.0.0.0", "lt": "192.168.1.5"}}});
    assert_eq!(q(range).unwrap(), ["2"]);
    let range = json!({"range": {"ip": {"gt": "10.0.0.1", "lte": "::ffff:192.168.1.5"}}});
    assert_eq!(q(range).unwrap(), ["1"]);
    assert_eq!(
        q(json!({"range": {"ip": {"gt": "::"}}})).unwrap(),
        ["1", "2", "3"]
    );
    assert_eq!(
        q(json!({"range": {"ip": {"gte": "::1"}}})).unwrap(),
        ["1", "2", "3"]
    );
    assert_eq!(
        q(json!({"range": {"ip": {"gt": "1.0.0.0", "lt": "2.0.0.0"}}})).unwrap(),
        Vec::<String>::new()
    );
    assert_eq!(
        q(json!({"term": {"ip": "foo"}})).unwrap_err(),
        "'foo' is not an IP string literal."
    );
    assert_eq!(
        q(json!({"term": {"ip": "10.0.0.0/33"}})).unwrap_err(),
        "Illegal prefix length [33] in [10.0.0.0/33]. Must be 0-32 for IPv4 ranges, 0-128 for IPv6 ranges"
    );
    // an ip term is a point query, scored as a constant
    let req = SearchRequest::parse(&json!({"query": {"term": {"ip": "8.8.8.8"}}})).unwrap();
    assert_eq!(search(&i, &req).hits[0].score, Some(1.0));
}

fn ranges() -> Index {
    let mut i = Index::default();
    let props = &mut i.mappings.properties;
    props.insert("l".into(), FieldMapping::LongRange);
    props.insert("d".into(), FieldMapping::DateRange { format: None });
    i.put(
        "a",
        json!({"l": {"gte": 10, "lte": 20}, "d": {"gte": "2020-01-01", "lte": "2020-12-31"}}),
    );
    i.put(
        "b",
        json!({"l": {"gt": 5, "lt": 30}, "d": {"gte": "2019-06-01", "lt": "2020-02-01"}}),
    );
    i.put("c", json!({"l": [{"gte": 15}, {"lte": -5}]}));
    i
}

#[test]
fn ip_and_range_fields_range_fields_answer_term_and_range_queries_by_relation() {
    let i = ranges();
    let q = |q: Value| ids(&i, q).unwrap();
    assert_eq!(q(json!({"term": {"l": 12}})), ["a", "b"]);
    assert_eq!(q(json!({"term": {"l": 29}})), ["b", "c"]);
    assert_eq!(q(json!({"term": {"l": 30}})), ["c"]);
    assert_eq!(q(json!({"term": {"l": -5}})), ["c"]);
    let rel = |r: &str| q(json!({"range": {"l": {"gte": 10, "lte": 20, "relation": r}}}));
    assert_eq!(rel("intersects"), ["a", "b", "c"]);
    assert_eq!(rel("WITHIN"), ["a"]);
    assert_eq!(rel("contains"), ["a", "b"]);
    assert_eq!(q(json!({"range": {"l": {"gt": 20, "lt": 22}}})), ["b", "c"]);
    assert_eq!(q(json!({"range": {"l": {"lt": -10}}})), ["c"]);
    assert_eq!(
        q(json!({"range": {"l": {"gt": 20, "lt": 21}}})),
        Vec::<String>::new()
    );
    assert_eq!(q(json!({"exists": {"field": "d"}})), ["a", "b"]);
    assert_eq!(q(json!({"term": {"d": "2020-06-01"}})), ["a"]);
    assert_eq!(q(json!({"term": {"d": "2020-01-15"}})), ["a", "b"]);
    let d = |r: &str| q(json!({"range": {"d": {"gte": "2020-01", "lt": "2021", "relation": r}}}));
    assert_eq!(d("intersects"), ["a", "b"]);
    assert_eq!(d("within"), ["a"]);
    // a's range ends as 2020-12-31 rounds up, so it holds exactly the query's
    assert_eq!(d("contains"), ["a"]);
    let err = |r: &str| {
        parse_query(&json!({"range": {"l": {"gte": 1, "relation": r}}}))
            .unwrap_err()
            .msg
    };
    assert_eq!(
        err("disjoint"),
        "[range] query does not support relation [disjoint]"
    );
    assert_eq!(err("near"), "near is not a valid relation");
    assert_eq!(
        ids(&i, json!({"range": {"l": {"gte": 3, "lte": 2}}})).unwrap_err(),
        "Range query `from` value (3) is greater than `to` value (2)"
    );
}

fn more_ranges() -> Index {
    let mut i = Index::default();
    i.mappings = serde_json::from_value(json!({"properties": {
        "i": {"type": "integer_range"}, "f": {"type": "float_range"},
        "x": {"type": "double_range"}, "ip": {"type": "ip_range"},
    }}))
    .unwrap();
    i.put(
        "a",
        json!({"i": {"gte": 10, "lte": 20}, "f": {"gte": 1.5, "lt": 2.5},
            "x": {"gt": 0.1, "lte": 0.3}, "ip": "10.0.0.0/8"}),
    );
    i.put(
        "b",
        json!({"i": {"gt": 20}, "f": {"lte": 1.5}, "x": {"gte": 1e300},
            "ip": {"gte": "::ffff:192.168.0.1", "lt": "192.168.0.10"}}),
    );
    i
}

#[test]
fn ip_and_range_fields_integer_float_double_and_ip_ranges_map_and_match() {
    let i = more_ranges();
    let mapped: Value = serde_json::to_value(&i.mappings).unwrap();
    assert_eq!(mapped["properties"]["ip"]["type"], "ip_range");
    let q = |q: Value| ids(&i, q).unwrap();
    let none = Vec::<String>::new();
    assert_eq!(q(json!({"term": {"i": 20}})), ["a"]);
    assert_eq!(q(json!({"term": {"i": 21}})), ["b"]);
    assert_eq!(q(json!({"term": {"i": 2147483647}})), ["b"]);
    assert_eq!(
        q(json!({"range": {"i": {"gte": 15, "lte": 30, "relation": "within"}}})),
        none
    );
    assert_eq!(
        q(json!({"range": {"i": {"gte": 15, "lte": 30, "relation": "intersects"}}})),
        ["a", "b"]
    );
    // floats compare at float precision: 1.5 sits in both, 2.5 in neither
    assert_eq!(q(json!({"term": {"f": 1.5}})), ["a", "b"]);
    assert_eq!(q(json!({"term": {"f": 2.5}})), none);
    assert_eq!(q(json!({"term": {"f": -1e30}})), ["b"]);
    assert_eq!(
        q(json!({"range": {"f": {"gt": 1.5, "lt": 3, "relation": "contains"}}})),
        none
    );
    assert_eq!(
        q(json!({"range": {"f": {"gte": 1.5, "lt": 2.5, "relation": "contains"}}})),
        ["a"]
    );
    assert_eq!(q(json!({"term": {"x": 0.1}})), none);
    assert_eq!(q(json!({"term": {"x": 0.3}})), ["a"]);
    assert_eq!(q(json!({"range": {"x": {"gte": 1e301}}})), ["b"]);
    // a CIDR string is the block's range, and addresses normalize
    assert_eq!(q(json!({"term": {"ip": "10.255.255.255"}})), ["a"]);
    assert_eq!(q(json!({"term": {"ip": "192.168.0.9"}})), ["b"]);
    assert_eq!(q(json!({"term": {"ip": "192.168.0.10"}})), none);
    assert_eq!(
        q(
            json!({"range": {"ip": {"gte": "10.0.0.0", "lte": "10.255.255.255", "relation": "within"}}})
        ),
        ["a"]
    );
    assert_eq!(
        q(
            json!({"range": {"ip": {"gte": "::ffff:192.168.0.2", "lte": "192.168.0.3", "relation": "contains"}}})
        ),
        ["b"]
    );
    let err = |q: Value| ids(&i, q).unwrap_err();
    assert_eq!(
        err(json!({"range": {"f": {"gte": 3, "lte": 2}}})),
        "Range query `from` value (3.0) is greater than `to` value (2.0)"
    );
    assert_eq!(
        err(json!({"range": {"ip": {"gte": "10.0.0.2", "lte": "10.0.0.1"}}})),
        "Range query `from` value (/10.0.0.2) is greater than `to` value (/10.0.0.1)"
    );
    assert_eq!(
        err(json!({"term": {"i": 2147483648_i64}})),
        "Value [2147483648] is out of range for an integer"
    );
}
