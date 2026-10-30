use es_index::{FieldMapping, Index};
use es_query::esql::{EsqlError, Response, run};
use serde_json::{Value, json};
use std::collections::BTreeMap;

fn indices() -> BTreeMap<String, Index> {
    let mut i = Index::default();
    i.mappings.properties.insert(
        "name".into(),
        FieldMapping::Keyword {
            ignore_above: None,
            split_queries_on_whitespace: false,
        },
    );
    i.mappings
        .properties
        .insert("age".into(), FieldMapping::Long);
    i.put("1", json!({"name": "ann", "age": 30, "city": "Oslo"}));
    i.put("2", json!({"name": "bob", "age": 25}));
    i.put("3", json!({"name": "cid", "age": 41, "city": "Rome"}));
    BTreeMap::from([("people".to_owned(), i)])
}

fn q(query: &str) -> Response {
    run(&indices(), query).unwrap()
}

fn col_names(r: &Response) -> Vec<&str> {
    r.columns.iter().map(|c| c.name.as_str()).collect()
}

#[test]
fn from_returns_all_columns_sorted_with_types() {
    let r = q("FROM people");
    assert_eq!(col_names(&r), ["age", "city", "city.keyword", "name"]);
    let types: Vec<&str> = r.columns.iter().map(|c| c.r#type.as_str()).collect();
    assert_eq!(types, ["long", "text", "keyword", "keyword"]);
    assert_eq!(
        r.values[0],
        vec![json!(30), json!("Oslo"), json!("Oslo"), json!("ann")]
    );
    assert_eq!(r.values[1][1], Value::Null);
}

#[test]
fn where_keep_sort_limit() {
    let r = q(
        "from people | where age > 26 and city is not null | keep name, age | sort age desc | limit 1",
    );
    assert_eq!(col_names(&r), ["name", "age"]);
    assert_eq!(r.values, vec![vec![json!("cid"), json!(41)]]);
}

#[test]
fn where_operators() {
    let names = |query: &str| -> Vec<Value> {
        q(query)
            .values
            .into_iter()
            .map(|row| row[3].clone())
            .collect()
    };
    assert_eq!(names("FROM people | WHERE name == \"bob\""), [json!("bob")]);
    assert_eq!(
        names("FROM people | WHERE name != \"bob\" AND age <= 30"),
        [json!("ann")]
    );
    assert_eq!(
        names("FROM people | WHERE NOT (age >= 30) OR city == \"Rome\""),
        [json!("bob"), json!("cid")]
    );
    assert_eq!(names("FROM people | WHERE city IS NULL"), [json!("bob")]);
    // null comparisons are not true
    assert_eq!(
        names("FROM people | WHERE city != \"Oslo\""),
        [json!("cid")]
    );
}

#[test]
fn drop_and_wildcards() {
    assert_eq!(col_names(&q("FROM people | DROP city*")), ["age", "name"]);
    assert_eq!(
        col_names(&q("FROM people | KEEP c*, name")),
        ["city", "city.keyword", "name"]
    );
}

#[test]
fn sort_nulls_and_multiple_keys() {
    let r = q("FROM people | SORT city NULLS FIRST, age | KEEP name");
    assert_eq!(
        r.values,
        vec![vec![json!("bob")], vec![json!("ann")], vec![json!("cid")]]
    );
    let r = q("FROM people | SORT city DESC | KEEP name");
    // Nulls count as larger than any value, so they come first when descending.
    assert_eq!(
        r.values,
        vec![vec![json!("bob")], vec![json!("cid")], vec![json!("ann")]]
    );
}

/// As a live Elasticsearch 9.5.5 answered: a missing index is a verification problem only when
/// the whole expression matches nothing.
#[test]
fn missing_indices_match_elasticsearch() {
    let err = |query| run(&indices(), query).unwrap_err();
    assert_eq!(
        err("FROM nope, nope2"),
        EsqlError::Verification("Unknown index [nope,nope2]".into())
    );
    assert_eq!(
        err("FROM nope*, nope"),
        EsqlError::Verification("Unknown index [nope*,nope]".into())
    );
    assert_eq!(
        err("FROM nope2, people, nope"),
        EsqlError::IndexNotFound("nope2".into())
    );
    assert!(run(&indices(), "FROM nope*, people").is_ok());
    // Wildcards that match nothing give no rows and a placeholder column.
    let r = q("FROM nope*");
    assert_eq!(
        (col_names(&r), r.columns[0].r#type.as_str()),
        (vec!["<no-fields>"], "null")
    );
    assert!(r.values.is_empty());
    assert_eq!(
        err("FROM nope* | KEEP x"),
        EsqlError::Verification("Found 1 problem\nline 1:19: Unknown column [x]".into())
    );
}

#[test]
fn errors() {
    for bad in [
        "",
        "FROM",
        "FROM people | WHERE",
        "FROM people | LIMIT x",
        "FROM people | FOO",
        "FROM people | WHERE age >",
        "FROM people | WHERE \"x",
    ] {
        assert!(
            matches!(run(&indices(), bad), Err(EsqlError::Parse(_))),
            "{bad}"
        );
    }
    assert!(matches!(
        run(&indices(), "FROM nope"),
        Err(EsqlError::Verification(_))
    ));
    assert_eq!(
        run(&indices(), "FROM people | KEEP zzz").unwrap_err(),
        EsqlError::Verification("Found 1 problem\nline 1:20: Unknown column [zzz]".into())
    );
    assert!(matches!(
        run(&indices(), "FROM people | WHERE zzz > 1"),
        Err(EsqlError::Verification(_))
    ));
}

#[test]
fn from_aliases_and_multiple_sources_with_filters() {
    use es_index::aliases::Alias;
    let mut all = indices();
    let mut more = Index::default();
    more.mappings.properties.insert(
        "name".into(),
        FieldMapping::Keyword {
            ignore_above: None,
            split_queries_on_whitespace: false,
        },
    );
    more.put("9", json!({"name": "dee", "age": 50}));
    more.aliases.insert(
        "crowd".into(),
        Alias::parse(&json!({"filter": {"range": {"age": {"gte": 45}}}})).unwrap(),
    );
    all.get_mut("people")
        .unwrap()
        .aliases
        .insert("crowd".into(), Alias::default());
    all.insert("more".into(), more);
    let names = |q: &str| -> Vec<Value> {
        run(&all, q)
            .unwrap()
            .values
            .into_iter()
            .map(|r| r[0].clone())
            .collect()
    };
    assert_eq!(
        names("FROM crowd | KEEP name | SORT name"),
        [json!("ann"), json!("bob"), json!("cid"), json!("dee")]
    );
    assert_eq!(
        names("FROM people, more | WHERE age > 40 | KEEP name | SORT name"),
        [json!("cid"), json!("dee")]
    );
}

#[test]
fn filtered_alias_matches_in_every_shard() {
    use es_index::aliases::Alias;
    let mut i = Index::default();
    (i.settings).merge(&json!({"index.number_of_shards": "3"}));
    for n in 0..12 {
        i.map_dynamic(&n.to_string(), &json!({"t": "a"})).unwrap();
        i.put(&n.to_string(), json!({"t": "a"}));
    }
    assert!(i.shards().len() > 1);
    let filter = json!({"filter": {"term": {"t.keyword": "a"}}});
    i.aliases.insert("f".into(), Alias::parse(&filter).unwrap());
    let all = BTreeMap::from([("i".to_owned(), i)]);
    let r = run(&all, "FROM f | STATS c = COUNT(*)").unwrap();
    assert_eq!(r.values, vec![vec![json!(12)]]);
}

#[test]
fn text_with_keyword_subfield_keeps_both_columns() {
    let mut i = Index::default();
    i.map_dynamic("1", &json!({"t": "Hello"})).unwrap();
    i.put("1", json!({"t": "Hello"}));
    let all = BTreeMap::from([("idx".to_owned(), i)]);
    let r = run(&all, "FROM idx | KEEP t, t.keyword").unwrap();
    let types: Vec<&str> = r.columns.iter().map(|c| c.r#type.as_str()).collect();
    assert_eq!(types, ["text", "keyword"]);
    assert_eq!(r.values, vec![vec![json!("Hello"), json!("Hello")]]);
}

#[test]
fn every_declared_multi_field_is_a_column_of_its_own_type() {
    let mut i = Index::default();
    i.mappings.properties.insert(
        "d".into(),
        FieldMapping::Date {
            format: None,
            locale: None,
            fields: BTreeMap::from([("n".into(), FieldMapping::Long)]),
        },
    );
    i.put("1", json!({"d": 5}));
    let all = BTreeMap::from([("idx".to_owned(), i)]);
    let r = run(&all, "FROM idx | KEEP d.n").unwrap();
    assert_eq!(r.columns[0].r#type, "long");
    assert_eq!(r.values, vec![vec![json!(5)]]);
}

#[test]
fn reports_documents_found_and_values_loaded() {
    let r = q("FROM people | KEEP name | LIMIT 2");
    assert_eq!(r.documents_found, 2);
    assert_eq!(r.values_loaded, 2);
}

#[test]
fn implicit_limit_is_1000_and_explicit_limit_up_to_10000() {
    let mut i = Index::default();
    for n in 0..12000 {
        i.put(&n.to_string(), json!({"n": n}));
    }
    let idx = BTreeMap::from([("big".to_owned(), i)]);
    assert_eq!(run(&idx, "FROM big").unwrap().values.len(), 1000);
    assert_eq!(
        run(&idx, "FROM big | LIMIT 5000").unwrap().values.len(),
        5000
    );
    assert_eq!(
        run(&idx, "FROM big | LIMIT 20000").unwrap().values.len(),
        10000
    );
}

fn conflicting() -> BTreeMap<String, Index> {
    let mut all = indices();
    let mut b = Index::default();
    b.mappings.properties.insert(
        "age".into(),
        FieldMapping::Keyword {
            ignore_above: None,
            split_queries_on_whitespace: false,
        },
    );
    b.mappings.properties.insert(
        "name".into(),
        FieldMapping::Keyword {
            ignore_above: None,
            split_queries_on_whitespace: false,
        },
    );
    b.put("1", json!({"name": "dan", "age": "old"}));
    let mut c = Index::default();
    c.mappings.properties.insert(
        "age".into(),
        FieldMapping::Date {
            format: None,
            locale: None,
            fields: BTreeMap::new(),
        },
    );
    all.insert("b".into(), b);
    all.insert("c".into(), c);
    all
}

#[test]
fn conflicting_types_are_unsupported_with_null_values() {
    let r = run(&conflicting(), "FROM people, b | KEEP age, name").unwrap();
    let cols = serde_json::to_value(&r.columns).unwrap();
    assert_eq!(
        cols,
        json!([
            {"name": "age", "type": "unsupported", "original_types": ["keyword", "long"], "suggested_cast": "keyword"},
            {"name": "name", "type": "keyword"},
        ])
    );
    assert!(r.values.iter().all(|row| row[0].is_null()));
    assert!(r.values.iter().any(|row| row[1] == json!("ann")));
}

#[test]
fn using_a_conflicting_field_fails_verification() {
    let all = conflicting();
    for (q, at) in [
        ("FROM people, b, c | WHERE age > 1", "1:27"),
        ("FROM people, b, c | SORT age", "1:26"),
    ] {
        assert_eq!(
            run(&all, q).unwrap_err(),
            EsqlError::Verification(format!(
                "Found 1 problem\nline {at}: Cannot use field [age] due to ambiguities being \
                 mapped as [3] incompatible types: [datetime] in [c], [keyword] in [b], [long] in \
                 [people]"
            )),
            "{q}"
        );
    }
}

#[test]
fn eval_replaces_a_conflicting_field() {
    let r = run(
        &conflicting(),
        "FROM people, b | EVAL age = 1 | SORT age | KEEP age",
    )
    .unwrap();
    let cols = serde_json::to_value(&r.columns).unwrap();
    assert_eq!(cols, json!([{"name": "age", "type": "integer"}]));
    assert!(r.values.iter().all(|row| row[0] == json!(1)));
}

/// Counts Elasticsearch 9.5.5 reported for the same documents and queries.
#[test]
fn documents_found_and_values_loaded_follow_the_plan() {
    let mut all = indices();
    let mut other = Index::default();
    other.put("1", json!({"n": 1}));
    all.insert("pets".into(), other);
    for (query, docs, values) in [
        ("FROM people", 3, 10),
        ("FROM people | LIMIT 2", 2, 6),
        // Pushed filters load nothing; filters on two fields load them for every document.
        ("FROM people | WHERE age > 26 | KEEP name", 2, 2),
        ("FROM people | WHERE age > age | KEEP name", 3, 3),
        (
            "FROM people | WHERE age > 26 OR age > age | KEEP name",
            3,
            5,
        ),
        (
            "FROM people | WHERE age > 26 AND age > age | KEEP name",
            2,
            2,
        ),
        // Text sorts are not pushed: the key loads for every match, the rest after the top N.
        ("FROM people | SORT city | KEEP name | LIMIT 1", 3, 3),
        (
            "FROM people | SORT city.keyword | KEEP name | LIMIT 1",
            1,
            2,
        ),
        (
            "FROM people | SORT age | WHERE name == \"ann\" | KEEP city",
            1,
            2,
        ),
        (
            "FROM people | WHERE age > age OR city IS NULL | SORT name | KEEP city | LIMIT 1",
            3,
            6,
        ),
        // Everything after the first LIMIT loads for the rows it keeps.
        ("FROM people | LIMIT 2 | WHERE age > 26 | KEEP name", 2, 4),
        ("FROM people | LIMIT 2 | SORT city | KEEP name", 2, 3),
        (
            "FROM people | SORT name | LIMIT 2 | SORT age | KEEP age",
            2,
            4,
        ),
        // A pushed LIMIT is shared across indices; a pushed top N runs per index.
        ("FROM people, pets | LIMIT 1", 1, 4),
        (
            "FROM people, pets | SORT city.keyword | KEEP city | LIMIT 1",
            2,
            2,
        ),
        ("FROM people, pets | SORT city | KEEP city | LIMIT 1", 4, 2),
        // Renamed columns load under their old names.
        ("FROM people | RENAME name AS who | KEEP who", 3, 3),
    ] {
        let r = run(&all, query).unwrap();
        assert_eq!(
            (r.documents_found, r.values_loaded),
            (docs, values),
            "{query}"
        );
    }
}

/// A column the query only names as an output of EVAL or STATS is not loaded from the documents.
#[test]
fn values_loaded_skips_a_source_column_only_named_as_an_output() {
    let mut t = Index::default();
    t.put("1", json!({"k": 1, "n": 1}));
    t.put("2", json!({"k": 2, "n": 2}));
    let all = BTreeMap::from([("t".to_owned(), t)]);
    for (query, values) in [
        ("FROM t | KEEP k | EVAL n = k", 2),
        ("FROM t | STATS n = COUNT(*)", 0),
    ] {
        assert_eq!(run(&all, query).unwrap().values_loaded, values, "{query}");
    }
}

/// The counts' plan keeps only the last SORT, so ties fall in document order there even where
/// an earlier SORT ordered the query's own rows.
#[test]
fn documents_found_and_values_loaded_drop_an_overridden_sort() {
    let mut t = Index::default();
    t.put("1", json!({"k": 1, "c": "x"}));
    t.put("2", json!({"k": 1}));
    let all = BTreeMap::from([("t".to_owned(), t)]);
    let r = run(
        &all,
        "FROM t | SORT c NULLS FIRST | SORT k | KEEP c | LIMIT 1",
    )
    .unwrap();
    assert_eq!(r.values, [[Value::Null]]);
    assert_eq!((r.documents_found, r.values_loaded), (1, 2));
}

/// Messages a live Elasticsearch 9.5.5 returned for the same queries: every problem in the first
/// command that has any, with its line and column.
#[test]
fn verification_errors_match_elasticsearch() {
    let mut all = indices();
    let people = all.remove("people").unwrap();
    all.insert("idx".into(), people);
    for (query, problems) in [
        (
            "FROM idx | KEEP zzz, yyy",
            "Found 2 problems\nline 1:17: Unknown column [zzz]\nline 1:22: Unknown column [yyy]",
        ),
        (
            "from idx | where zzz > 1 and qq < 2 | sort ww",
            "Found 2 problems\nline 1:18: Unknown column [zzz]\nline 1:30: Unknown column [qq]",
        ),
        (
            "FROM idx | DROP zzz*, yyy",
            "Found 2 problems\nline 1:17: No matches found for pattern [zzz*]\n\
             line 1:23: Unknown column [yyy]",
        ),
        (
            "FROM idx | KEEP zzz*, name | KEEP age",
            "Found 1 problem\nline 1:17: No matches found for pattern [zzz*]",
        ),
        (
            "FROM idx\n| WHERE\n  zzz > 1",
            "Found 1 problem\nline 3:3: Unknown column [zzz]",
        ),
        (
            "FROM idx | WHERE `zzz` > 1",
            "Found 1 problem\nline 1:18: Unknown column [zzz]",
        ),
        (
            "FROM idx | WHERE zzz > zzz",
            "Found 1 problem\nline 1:18: Unknown column [zzz]",
        ),
        (
            "FROM idx | SORT zzz, yyy | KEEP qqq",
            "Found 2 problems\nline 1:17: Unknown column [zzz]\nline 1:22: Unknown column [yyy]",
        ),
        (
            "FROM idx | DROP zzz, name | KEEP name",
            "Found 1 problem\nline 1:17: Unknown column [zzz]",
        ),
        (
            "FROM idx | KEEP name, zzz | WHERE age > 1",
            "Found 1 problem\nline 1:23: Unknown column [zzz]",
        ),
    ] {
        assert_eq!(
            run(&all, query).unwrap_err(),
            EsqlError::Verification(problems.into()),
            "{query}"
        );
    }
}

/// Type errors as a live Elasticsearch 9.5.5 reported them for the same queries and mappings.
#[test]
fn where_type_errors_match_elasticsearch() {
    for (query, problem) in [
        (
            "FROM people | WHERE age > \"x\"",
            "line 1:21: first argument of [age > \"x\"] is [numeric] so second argument must also be [numeric] but was [keyword]",
        ),
        (
            "FROM people | WHERE name > 1",
            "line 1:21: first argument of [name > 1] is [keyword] so second argument must also be [keyword] but was [integer]",
        ),
        (
            "FROM people | WHERE city > 1",
            "line 1:21: first argument of [city > 1] is [text] so second argument must also be [text] but was [integer]",
        ),
        (
            "FROM people | WHERE age == true",
            "line 1:21: first argument of [age == true] is [numeric] so second argument must also be [numeric] but was [boolean]",
        ),
        (
            "FROM people | WHERE age",
            "line 1:21: Condition expression needs to be boolean, found [LONG]",
        ),
        (
            "FROM people | WHERE name",
            "line 1:21: Condition expression needs to be boolean, found [KEYWORD]",
        ),
        (
            "FROM people | WHERE 1",
            "line 1:21: Condition expression needs to be boolean, found [INTEGER]",
        ),
        (
            "FROM people | WHERE NOT age",
            "line 1:21: argument of [NOT age] must be [boolean], found value [age] type [long]",
        ),
        (
            "FROM people | WHERE age AND name",
            "line 1:21: first argument of [age AND name] must be [boolean], found value [age] type [long]",
        ),
        (
            "FROM people | WHERE name IS NULL AND age",
            "line 1:21: second argument of [name IS NULL AND age] must be [boolean], found value [age] type [long]",
        ),
    ] {
        assert_eq!(
            run(&indices(), query).unwrap_err(),
            EsqlError::Verification(format!("Found 1 problem\n{problem}")),
            "{query}"
        );
    }
    for ok in [
        "FROM people | WHERE age > 1.5",
        "FROM people | WHERE city == \"Oslo\"",
        "FROM people | WHERE null",
        "FROM people | WHERE age > null",
    ] {
        assert!(run(&indices(), ok).is_ok(), "{ok}");
    }
}

/// Parse errors as a live Elasticsearch 9.5.5 reported them for the same queries.
#[test]
fn parse_errors_match_elasticsearch() {
    const COMMANDS: &str = "{'change_point', 'enrich', 'completion', 'dissect', 'eval', 'grok', \
        'limit', 'rerank', 'sample', 'sort', STATS, 'where', 'uri_parts', 'metrics_info', \
        'registered_domain', 'ts_info', 'user_agent', 'ts_collapse', 'ip_location', 'fork', \
        'fuse', 'inline', 'inlinestats', 'lookup', 'mmr', 'mv_expand', 'drop', 'keep', 'rename'}";
    let mut all = indices();
    let people = all.remove("people").unwrap();
    all.insert("idx".into(), people);
    for (query, message) in [
        (
            "FROM",
            "line 1:5: mismatched input '<EOF>' expecting {QUOTED_STRING, '(', UNQUOTED_SOURCE}",
        ),
        (
            "idx",
            "line 1:1: mismatched input 'idx' expecting {, 'row', 'from', 'ts', 'promql', 'set', 'show'}",
        ),
        (
            "FROM idx | WHERE",
            "line 1:17: mismatched input '<EOF>' expecting {QUOTED_STRING, INTEGER_LITERAL, \
             DECIMAL_LITERAL, 'false', 'first', 'last', 'not', 'null', '?', 'true', '+', '-', \
             '??', NAMED_OR_POSITIONAL_PARAM, NAMED_OR_POSITIONAL_DOUBLE_PARAMS, '[', '(', \
             UNQUOTED_IDENTIFIER, QUOTED_IDENTIFIER}",
        ),
        (
            "FROM idx | LIMIT x",
            "line 1:18: mismatched input 'x' expecting {QUOTED_STRING, INTEGER_LITERAL, \
             DECIMAL_LITERAL, 'false', 'null', '?', 'true', '+', '-', NAMED_OR_POSITIONAL_PARAM, '['}",
        ),
        (
            "FROM idx | FOO",
            &format!("line 1:12: mismatched input 'FOO' expecting {COMMANDS}"),
        ),
        (
            "FROM idx |",
            &format!("line 1:11: mismatched input '<EOF>' expecting {COMMANDS}"),
        ),
        (
            "FROM idx | KEEP",
            "line 1:16: mismatched input '<EOF>' expecting {'?', '??', NAMED_OR_POSITIONAL_PARAM, \
             NAMED_OR_POSITIONAL_DOUBLE_PARAMS, '[', ID_PATTERN}",
        ),
        (
            "FROM idx | WHERE age IS",
            "line 1:24: mismatched input '<EOF>' expecting {'not', 'null'}",
        ),
        (
            "FROM idx | SORT age NULLS",
            "line 1:26: missing {'first', 'last'} at '<EOF>'",
        ),
        (
            "FROM idx | WHERE age >",
            "line 1:23: no viable alternative at input 'age >'",
        ),
        (
            "FROM idx | WHERE (age > 1",
            "line 1:26: no viable alternative at input '(age > 1'",
        ),
        (
            "FROM idx extra",
            "line 1:10: extraneous input 'extra' expecting <EOF>",
        ),
        (
            "FROM idx | WHERE age > 1 1",
            "line 1:26: extraneous input '1' expecting <EOF>",
        ),
        (
            "FROM idx | WHERE \"x",
            "line 1:18: token recognition error at: '\"x'",
        ),
        (
            "FROM idx | WHERE age # 1",
            "line 1:22: token recognition error at: '#'",
        ),
        (
            "FROM idx | LIMIT -1",
            "line 1:12: value of [LIMIT -1] must be a non negative integer, found value [-1] type [integer]",
        ),
        (
            "FROM idx | LIMIT 1.5",
            "line 1:12: value of [LIMIT 1.5] must be a non negative integer, found value [1.5] type [double]",
        ),
        (
            "FROM idx | LIMIT 99999999999999999999",
            "line 1:12: value of [LIMIT 99999999999999999999] must be a non negative integer, \
             found value [99999999999999999999] type [double]",
        ),
        (
            "FROM idx | LIMIT 3000000000",
            "line 1:12: value of [LIMIT 3000000000] must be a non negative integer, \
             found value [3000000000] type [long]",
        ),
        (
            "FROM idx | LIMIT 9223372036854775808",
            "line 1:12: value of [LIMIT 9223372036854775808] must be a non negative integer, \
             found value [9223372036854775808] type [unsigned_long]",
        ),
        (
            "FROM idx extra stuff",
            "line 1:10: mismatched input 'extra' expecting {<EOF>, '|', '::', ',', 'metadata'}",
        ),
        (
            "FROM idx, | LIMIT 1",
            "line 1:11: mismatched input '|' expecting {QUOTED_STRING, '(', UNQUOTED_SOURCE}",
        ),
        (
            "FROM idx | WHERE age > 1 1 1",
            "line 1:26: mismatched input '1' expecting \
             {<EOF>, '|', 'and', '::', 'or', '+', '-', '*', '/', '%'}",
        ),
        (
            "FROM idx | WHERE age = 1",
            "line 1:22: mismatched input '=' expecting \
             {<EOF>, '|', 'and', '::', '.', 'or', '+', '-', '*', '/', '%'}",
        ),
        (
            "FROM idx | WHERE age =",
            "line 1:22: extraneous input '=' expecting <EOF>",
        ),
        (
            "FROM idx | WHERE 1.2.3 > 1",
            "line 1:21: mismatched input '.3' expecting \
             {<EOF>, '|', 'and', '::', 'or', '+', '-', '*', '/', '%'}",
        ),
        (
            "FROM idx | WHERE age IS NULL x y",
            "line 1:30: mismatched input 'x' expecting {<EOF>, '|', 'and', 'or'}",
        ),
        (
            "FROM idx | WHERE (age > 1) x y",
            "line 1:28: mismatched input 'x' expecting \
             {'and', '::', 'or', '+', '-', '*', '/', '%', ')'}",
        ),
        (
            "FROM idx | WHERE age IS NOT",
            "line 1:28: missing 'null' at '<EOF>'",
        ),
        (
            "FROM idx | WHERE age IS NOT | LIMIT 1",
            "line 1:29: missing 'null' at '|'",
        ),
        (
            "FROM idx | WHERE age IS NOT age",
            "line 1:29: mismatched input 'age' expecting 'null'",
        ),
        (
            "FROM idx | WHERE (age > 1 AND",
            "line 1:30: no viable alternative at input '(age > 1 AND'",
        ),
        (
            "FROM idx | WHERE (age > 1 | LIMIT 1",
            "line 1:27: no viable alternative at input '(age > 1 |'",
        ),
        (
            "FROM idx | WHERE age >  | LIMIT 1",
            "line 1:25: no viable alternative at input 'age >  |'",
        ),
        (
            "FROM idx | WHERE age > 1 AND (name",
            "line 1:35: no viable alternative at input '(name'",
        ),
        (
            "FROM idx | WHERE age > 1 AND name >",
            "line 1:36: no viable alternative at input 'name >'",
        ),
        (
            "FROM idx\n| WHERE (",
            "line 2:10: no viable alternative at input '('",
        ),
        (
            "FROM idx | WHERE age > 1 OR",
            "line 1:28: mismatched input '<EOF>' expecting {QUOTED_STRING, INTEGER_LITERAL, \
             DECIMAL_LITERAL, 'false', 'first', 'last', 'not', 'null', '?', 'true', '+', '-', \
             '??', NAMED_OR_POSITIONAL_PARAM, NAMED_OR_POSITIONAL_DOUBLE_PARAMS, '[', '(', \
             UNQUOTED_IDENTIFIER, QUOTED_IDENTIFIER}",
        ),
        (
            "FROM idx | KEEP a b c",
            "line 1:19: mismatched input 'b' expecting {<EOF>, '|', ',', '.'}",
        ),
        (
            "FROM idx | KEEP a b",
            "line 1:19: extraneous input 'b' expecting <EOF>",
        ),
        (
            "FROM idx | KEEP name,",
            "line 1:22: mismatched input '<EOF>' expecting {'?', '??', NAMED_OR_POSITIONAL_PARAM, \
             NAMED_OR_POSITIONAL_DOUBLE_PARAMS, '[', ID_PATTERN}",
        ),
        (
            "FROM idx | SORT age x y",
            "line 1:21: mismatched input 'x' expecting {<EOF>, '|', 'and', 'asc', '::', ',', \
             'desc', '.', 'nulls', 'or', '+', '-', '*', '/', '%'}",
        ),
        (
            "FROM idx | SORT age DESC x y",
            "line 1:26: mismatched input 'x' expecting {<EOF>, '|', ',', 'nulls'}",
        ),
        (
            "FROM idx | SORT age NULLS LAST x y",
            "line 1:32: mismatched input 'x' expecting {<EOF>, '|', ','}",
        ),
        (
            "FROM idx | SORT age NULLS x",
            "line 1:27: mismatched input 'x' expecting {'first', 'last'}",
        ),
        (
            "FROM idx | SORT age NULLS | LIMIT 1",
            "line 1:27: missing {'first', 'last'} at '|'",
        ),
        (
            "FROM idx | WHERE `abc",
            "line 1:18: token recognition error at: '`abc'",
        ),
        (
            "FROM idx | WHERE \"x\ny\"",
            "line 1:18: token recognition error at: '\"x\\n'",
        ),
    ] {
        assert_eq!(
            run(&all, query).unwrap_err(),
            EsqlError::Parse(message.into()),
            "{query}"
        );
    }
}

#[test]
fn row_and_show_info_are_sources() {
    let r = q(r#"ROW a = 1, b = "two", c = true, d = -1.5, e = null, f = [1, 2], 3000000000"#);
    assert_eq!(col_names(&r), ["a", "b", "c", "d", "e", "f", "3000000000"]);
    let types: Vec<&str> = r.columns.iter().map(|c| c.r#type.as_str()).collect();
    assert_eq!(
        types,
        [
            "integer", "keyword", "boolean", "double", "null", "integer", "long"
        ]
    );
    assert_eq!(
        r.values,
        [[
            json!(1),
            json!("two"),
            json!(true),
            json!(-1.5),
            Value::Null,
            json!([1, 2]),
            json!(3000000000u64)
        ]]
    );
    assert_eq!((r.documents_found, r.values_loaded), (0, 0));
    assert_eq!(q("ROW a = 1, b = 2, a = 3 | KEEP a").values, [[json!(3)]]);
    assert_eq!(q("ROW a = 1 < 2").values, [[json!(true)]]);
    let r = q("SHOW INFO | KEEP version");
    assert_eq!(r.values, [[json!("9.5.5")]]);
    match run(&indices(), "ROW a = x") {
        Err(EsqlError::Verification(m)) => {
            assert_eq!(m, "Found 1 problem\nline 1:9: Unknown column [x]")
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn from_metadata_adds_id_index_and_version_columns() {
    let r = q(
        "FROM people METADATA _version, _id, _index | KEEP _id, _index, _version, name | SORT _id DESC | LIMIT 1",
    );
    let types: Vec<&str> = r.columns.iter().map(|c| c.r#type.as_str()).collect();
    assert_eq!(types, ["keyword", "keyword", "long", "keyword"]);
    assert_eq!(
        r.values,
        [[json!("3"), json!("people"), json!(1), json!("cid")]]
    );
    let r = q("FROM people METADATA _index");
    assert_eq!(
        col_names(&r),
        ["age", "city", "city.keyword", "name", "_index"]
    );
}

/// Elasticsearch 9.5.5's verification failures for unknown metadata fields.
#[test]
fn esql_es_semantics_unknown_metadata_fails_verification() {
    let err = |q: &str| match run(&indices(), q) {
        Err(EsqlError::Verification(m)) => m,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        err("FROM people METADATA _foo"),
        "Found 2 problems\nline 1:1: unresolved metadata fields: [?_foo]\n\
         line 1:22: Unresolved metadata pattern [_foo]"
    );
    assert_eq!(
        err("  FROM people METADATA _a, _b | WHERE x > 1"),
        "Found 3 problems\nline 1:3: unresolved metadata fields: [?_a, ?_b]\n\
         line 1:24: Unresolved metadata pattern [_a]\nline 1:28: Unresolved metadata pattern [_b]"
    );
    assert_eq!(err("FROM nope METADATA _a"), "Unknown index [nope]");
}

#[test]
fn arithmetic_widens_like_elasticsearch_and_overflow_is_null() {
    let r = q(
        "ROW a = 1 + 1.5, b = 1 + 3000000000, c = 7 - 2 * 3, d = (7 - 2) * 3, e = 7 / 2, \
               f = -7 % 3, g = 5 / 0, h = 2147483647 + 1, i = 1 + null, j = 7.0 / 0",
    );
    let types: Vec<&str> = r.columns.iter().map(|c| c.r#type.as_str()).collect();
    assert_eq!(
        types,
        [
            "double", "long", "integer", "integer", "integer", "integer", "integer", "integer",
            "integer", "double"
        ]
    );
    assert_eq!(
        r.values,
        [[
            json!(2.5),
            json!(3000000001u64),
            json!(1),
            json!(15),
            json!(3),
            json!(-1),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null
        ]]
    );
    let r = q("FROM people | WHERE age * 2 > 60 | KEEP name");
    assert_eq!(r.values, [[json!("cid")]]);
    for (query, problem) in [
        (
            r#"ROW a = "x" + 1"#,
            r#"line 1:9: first argument of ["x" + 1] must be [date_nanos, datetime, numeric or dense_vector], found value ["x"] type [keyword]"#,
        ),
        (
            "ROW a = 1 * true",
            "line 1:9: second argument of [1 * true] must be [numeric or dense_vector], found value [true] type [boolean]",
        ),
        (
            "ROW a = 1 % true",
            "line 1:9: second argument of [1 % true] must be [numeric], found value [true] type [boolean]",
        ),
    ] {
        match run(&indices(), query) {
            Err(EsqlError::Verification(m)) => assert_eq!(m, format!("Found 1 problem\n{problem}")),
            other => panic!("{query}: {other:?}"),
        }
    }
}

#[test]
fn conversion_functions_match_elasticsearch() {
    let r = q(
        r#"ROW a = TO_STRING(42), b = TO_STRING(true), c = TO_STRING(1.5), d = to_string(100000000000000000000.0)"#,
    );
    let types: Vec<&str> = r.columns.iter().map(|c| c.r#type.as_str()).collect();
    assert_eq!(types, ["keyword"; 4]);
    assert_eq!(
        r.values,
        [[json!("42"), json!("true"), json!("1.5"), json!("1.0E20")]]
    );
    let r = q(
        r#"ROW a = TO_INTEGER("42"), b = TO_INTEGER(3.9), c = TO_INTEGER(true), d = TO_INTEGER("x"), e = TO_INTEGER(-2.5), f = TO_INTEGER(" 7.5 "), g = TO_INTEGER(3000000000)"#,
    );
    assert_eq!(r.columns[0].r#type, "integer");
    assert_eq!(
        r.values,
        [[
            json!(42),
            json!(4),
            json!(1),
            Value::Null,
            json!(-2),
            json!(8),
            Value::Null
        ]]
    );
    let r = q(
        r#"ROW a = TO_LONG("9000000000"), b = TO_LONG(2.5), c = TO_LONG(["1.5", "x"]), d = TO_LONG(null)"#,
    );
    assert_eq!(r.columns[3].r#type, "long");
    assert_eq!(
        r.values,
        [[json!(9000000000u64), json!(3), json!(2), Value::Null]]
    );
    let r = q(
        r#"ROW a = TO_DOUBLE("1.25"), b = TO_DOUBLE(3), c = TO_DOUBLE(true), d = TO_DOUBLE("2d")"#,
    );
    assert_eq!(
        r.values,
        [[json!(1.25), json!(3.0), json!(1.0), json!(2.0)]]
    );
    let r = q(
        r#"ROW a = TO_BOOLEAN("TRUE"), b = TO_BOOLEAN("no"), c = TO_BOOLEAN(0), d = TO_BOOLEAN(2), e = TO_BOOLEAN([0, 1])"#,
    );
    assert_eq!(
        r.values,
        [[
            json!(true),
            json!(false),
            json!(false),
            json!(true),
            json!([false, true])
        ]]
    );
    let r = q("FROM people | WHERE TO_STRING(age) == \"30\" | KEEP name");
    assert_eq!(r.values, [[json!("ann")]]);
    match run(&indices(), "FROM people | WHERE UNKNOWN_FN(age)") {
        Err(EsqlError::Verification(m)) => {
            assert_eq!(
                m,
                "Found 1 problem\nline 1:21: Unknown function [UNKNOWN_FN]"
            )
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn eval_adds_or_replaces_columns_in_order() {
    let r = q(
        "FROM people | EVAL older = age + 1, twice = older * 2, name = 1 | KEEP name, older, twice | SORT older",
    );
    assert_eq!(col_names(&r), ["name", "older", "twice"]);
    let types: Vec<&str> = r.columns.iter().map(|c| c.r#type.as_str()).collect();
    assert_eq!(types, ["integer", "long", "long"]);
    assert_eq!(r.values[0], [json!(1), json!(26), json!(52)]);
    let r = q("ROW a = 7 | EVAL a + 1, a = 2");
    assert_eq!(col_names(&r), ["a + 1", "a"]);
    assert_eq!(r.values, [[json!(8), json!(2)]]);
    match run(&indices(), "FROM people | EVAL x = nope + 1") {
        Err(EsqlError::Verification(m)) => {
            assert_eq!(m, "Found 1 problem\nline 1:24: Unknown column [nope]")
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn substring_and_trim_match_elasticsearch() {
    let r = q(
        r#"ROW a = SUBSTRING("hello", 2), b = SUBSTRING("héllo", 2, 3), c = SUBSTRING("hello", -3, 2), d = SUBSTRING("hello", 0, 1), e = SUBSTRING("hi", 9), f = SUBSTRING(["ab", "cd"], 1), g = SUBSTRING(null, 1)"#,
    );
    let types: Vec<&str> = r.columns.iter().map(|c| c.r#type.as_str()).collect();
    assert_eq!(types, ["keyword"; 7]);
    assert_eq!(
        r.values,
        [[
            json!("ello"),
            json!("éll"),
            json!("ll"),
            json!("h"),
            json!(""),
            Value::Null,
            Value::Null
        ]]
    );
    // Java's Character.isWhitespace: not U+00A0, but U+001F and U+2003.
    let r =
        q("ROW a = TRIM(\"  padded \\t \"), b = TRIM(\"\u{a0}x\u{1f}\u{2003}\"), c = TRIM(null)");
    let types: Vec<&str> = r.columns.iter().map(|c| c.r#type.as_str()).collect();
    assert_eq!(types, ["keyword", "keyword", "null"]);
    assert_eq!(r.values, [[json!("padded"), json!("\u{a0}x"), Value::Null]]);
    let r = q("FROM people | EVAL t = TRIM(name), s = SUBSTRING(name, 1, 1) | KEEP t, s | SORT t");
    assert_eq!(r.values[0], [json!("ann"), json!("a")]);
    for (query, problem) in [
        (
            "FROM people | EVAL x = SUBSTRING(name, 1.5)",
            "line 1:24: second argument of [SUBSTRING(name, 1.5)] must be [integer], found value [1.5] type [double]",
        ),
        (
            "FROM people | EVAL y = TRIM(age)",
            "line 1:24: argument of [TRIM(age)] must be [string], found value [age] type [long]",
        ),
    ] {
        match run(&indices(), query) {
            Err(EsqlError::Verification(m)) => assert_eq!(m, format!("Found 1 problem\n{problem}")),
            other => panic!("{other:?}"),
        }
    }
}

#[test]
fn math_functions_match_elasticsearch() {
    let types =
        |r: &Response| -> Vec<String> { r.columns.iter().map(|c| c.r#type.clone()).collect() };
    let r = q(
        "ROW a = ABS(-3), b = ABS(-2.5), c = ROUND(1.23456, 2), d = ROUND(2.5), e = FLOOR(2.7), f = CEIL(2.1), g = POW(2, 10), h = POW(2.0, 0.5)",
    );
    assert_eq!(
        types(&r),
        [
            "integer", "double", "double", "double", "double", "double", "double", "double"
        ]
    );
    assert_eq!(
        r.values,
        [[
            json!(3),
            json!(2.5),
            json!(1.23),
            json!(3.0),
            json!(2.0),
            json!(3.0),
            json!(1024.0),
            json!(std::f64::consts::SQRT_2)
        ]]
    );
    // Overflow and non-finite results are null; ROUND of a negative double rounds half away
    // from zero; whole numbers round to tens with negative decimals; FLOOR of a whole number
    // passes multi-values through, while ABS of one is null.
    let r = q(
        "ROW a = ABS(-2147483648), b = ROUND(-2.5), c = ROUND(1250, -2), d = ROUND(2147483647, -1), e = POW(0, -1), f = FLOOR([1, 2]), g = ABS([1, -2]), h = ROUND(-1249, -2), i = ROUND(null)",
    );
    assert_eq!(
        types(&r),
        [
            "integer", "double", "integer", "integer", "double", "integer", "integer", "integer",
            "null"
        ]
    );
    assert_eq!(
        r.values,
        [[
            Value::Null,
            json!(-3.0),
            json!(1300),
            Value::Null,
            Value::Null,
            json!([1, 2]),
            Value::Null,
            json!(-1200),
            Value::Null
        ]]
    );
    let r = q("FROM people | EVAL a = ABS(age - 30), r = ROUND(age, -1) | KEEP a, r | SORT a");
    assert_eq!(types(&r), ["long", "long"]);
    assert_eq!(r.values[0], [json!(0), json!(30)]);
    for (query, problem) in [
        (
            "FROM people | EVAL x = ROUND(age, 1.5)",
            "line 1:24: second argument of [ROUND(age, 1.5)] must be [whole number except unsigned_long or counter types], found value [1.5] type [double]",
        ),
        (
            "FROM people | EVAL y = ABS(name)",
            "line 1:24: argument of [ABS(name)] must be [numeric], found value [name] type [keyword]",
        ),
        (
            "FROM people | EVAL z = POW(2, name)",
            "line 1:24: second argument of [POW(2, name)] must be [numeric], found value [name] type [keyword]",
        ),
    ] {
        match run(&indices(), query) {
            Err(EsqlError::Verification(m)) => assert_eq!(m, format!("Found 1 problem\n{problem}")),
            other => panic!("{other:?}"),
        }
    }
}

#[test]
fn mv_count_min_max_match_elasticsearch() {
    let r = q(
        r#"ROW a = MV_MIN([3, 1, 2]), b = MV_MAX(["b", "c", "a"]), c = MV_COUNT([1, 2, 3]), d = MV_COUNT("x"), e = MV_MAX(2.5), f = MV_MIN([true, false]), g = MV_COUNT(null)"#,
    );
    let types: Vec<&str> = r.columns.iter().map(|c| c.r#type.as_str()).collect();
    assert_eq!(
        types,
        [
            "integer", "keyword", "integer", "integer", "double", "boolean", "integer"
        ]
    );
    assert_eq!(
        r.values,
        [[
            json!(1),
            json!("c"),
            json!(3),
            json!(1),
            json!(2.5),
            json!(false),
            Value::Null
        ]]
    );
    let r = q("FROM people | EVAL c = MV_COUNT(city), m = MV_MAX(age) | KEEP c, m | SORT m");
    assert_eq!(r.values[0], [json!(null), json!(25)]);
}

#[test]
fn to_datetime_and_now_match_elasticsearch() {
    let r = q(
        r#"ROW a = TO_DATETIME("2024-02-29T12:00:00Z"), b = TO_DATETIME(0), c = TO_DATETIME("nope"), d = NOW() > TO_DATETIME("2025-01-01T00:00:00Z")"#,
    );
    let types: Vec<&str> = r.columns.iter().map(|c| c.r#type.as_str()).collect();
    assert_eq!(types, ["date", "date", "date", "boolean"]);
    assert_eq!(
        r.values,
        [[
            json!("2024-02-29T12:00:00.000Z"),
            json!("1970-01-01T00:00:00.000Z"),
            Value::Null,
            json!(true)
        ]]
    );
}

#[test]
fn date_extract_and_date_format_match_elasticsearch() {
    let r = q(
        r#"ROW t = TO_DATETIME("2024-02-29T08:15:42.123Z") | EVAL y = DATE_EXTRACT("year", t), m = DATE_EXTRACT("MONTH_OF_YEAR", t), dw = DATE_EXTRACT("day_of_week", t), dy = DATE_EXTRACT("day_of_year", t), ms = DATE_EXTRACT("milli_of_second", t), f = DATE_FORMAT("yyyy/MM/dd HH:mm", t), d = DATE_FORMAT(t), n = DATE_EXTRACT("year", null) | DROP t"#,
    );
    let types: Vec<&str> = r.columns.iter().map(|c| c.r#type.as_str()).collect();
    assert_eq!(
        types,
        [
            "long", "long", "long", "long", "long", "keyword", "keyword", "long"
        ]
    );
    assert_eq!(
        r.values,
        [[
            json!(2024),
            json!(2),
            json!(4),
            json!(60),
            json!(123),
            json!("2024/02/29 08:15"),
            json!("2024-02-29T08:15:42.123Z"),
            Value::Null
        ]]
    );
    match run(&indices(), r#"ROW x = DATE_EXTRACT("year", 1)"#) {
        Err(EsqlError::Verification(m)) => assert_eq!(
            m,
            "Found 1 problem\nline 1:9: second argument of [DATE_EXTRACT(\"year\", 1)] must be \
             [datetime or date_nanos], found value [1] type [integer]"
        ),
        other => panic!("{other:?}"),
    }
}

#[test]
fn date_trunc_rounds_down_to_time_spans() {
    let r = q(
        r#"ROW t = TO_DATETIME("2024-02-29T08:15:42.123Z") | EVAL y = DATE_TRUNC(1 year, t), q = DATE_TRUNC(1 QUARTER, t), m = DATE_TRUNC(1 mo, t), w = DATE_TRUNC(1 week, t), d = DATE_TRUNC(1 day, t), h = DATE_TRUNC(2 hours, t), s = DATE_TRUNC(15 s, t), n = DATE_TRUNC(1 year, null) | DROP t"#,
    );
    assert!(r.columns.iter().all(|c| c.r#type == "date"));
    assert_eq!(
        r.values,
        [[
            json!("2024-01-01T00:00:00.000Z"),
            json!("2024-01-01T00:00:00.000Z"),
            json!("2024-02-01T00:00:00.000Z"),
            json!("2024-02-26T00:00:00.000Z"),
            json!("2024-02-29T00:00:00.000Z"),
            json!("2024-02-29T08:00:00.000Z"),
            json!("2024-02-29T08:15:30.000Z"),
            Value::Null
        ]]
    );
    match run(&indices(), "ROW x = DATE_TRUNC(1, NOW())") {
        Err(EsqlError::Verification(m)) => {
            assert!(
                m.contains("first argument of [DATE_TRUNC(1, NOW())] must be"),
                "{m}"
            )
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn rlike_matches_lucene_regular_expressions() {
    let names =
        |query: &str| -> Vec<Value> { q(query).values.into_iter().map(|r| r[0].clone()).collect() };
    assert_eq!(
        names(r#"FROM people | WHERE name RLIKE "[a-b].*" | KEEP name | SORT name"#),
        [json!("ann"), json!("bob")]
    );
    assert_eq!(
        names(r#"FROM people | WHERE name NOT RLIKE "(an+|c[^a]d)" | KEEP name"#),
        [json!("bob")]
    );
    assert_eq!(
        names(r#"FROM people | WHERE city RLIKE "R.{2}e|@x" | KEEP name"#),
        [json!("cid")]
    );
    assert!(matches!(
        run(&indices(), r#"FROM people | WHERE name RLIKE "(a""#),
        Err(EsqlError::Parse(_))
    ));
}

#[test]
fn stats_count_star_by_groups() {
    let r = q("FROM people METADATA _index | STATS c = COUNT(*) BY _index");
    assert_eq!(col_names(&r), ["c", "_index"]);
    assert_eq!(r.columns[0].r#type, "long");
    assert_eq!(r.values, vec![vec![json!(3), json!("people")]]);
    let r = q(
        "FROM people | WHERE age > 26 | STATS COUNT(*) BY has_city = city IS NOT NULL | SORT has_city",
    );
    assert_eq!(col_names(&r), ["COUNT(*)", "has_city"]);
    assert_eq!(r.values, vec![vec![json!(2), json!(true)]]);
    assert_eq!(
        q("ROW a = 1 | STATS c = COUNT(*)").values,
        vec![vec![json!(1)]]
    );
    let r = q("FROM people | WHERE age > 99 | STATS c = COUNT(*)");
    assert_eq!(r.values, vec![vec![json!(0)]]);
    assert!(matches!(
        run(&indices(), "FROM people | STATS c = COUNT(*) BY nope"),
        Err(EsqlError::Verification(m)) if m.contains("Unknown column [nope]")
    ));
}

#[test]
fn stats_min_max_by_groups() {
    let r = q(
        "FROM people | STATS m = MAX(age), n = MIN(age) BY has_city = city IS NOT NULL | EVAL older = m > 35 | SORT has_city",
    );
    assert_eq!(col_names(&r), ["m", "n", "has_city", "older"]);
    let types: Vec<&str> = r.columns.iter().map(|c| c.r#type.as_str()).collect();
    assert_eq!(types, ["long", "long", "boolean", "boolean"]);
    assert_eq!(
        r.values,
        [
            [json!(25), json!(25), json!(false), json!(false)],
            [json!(41), json!(30), json!(true), json!(true)]
        ]
    );
    // Multi-values count value by value; a group without values is null.
    let r = q("ROW a = [3, 9, 1] | STATS hi = MAX(a), lo = MIN(a)");
    assert_eq!(r.values, [[json!(9), json!(1)]]);
    let r = q("FROM people | WHERE age > 99 | STATS m = MAX(age)");
    assert_eq!(r.values, [[Value::Null]]);
    assert!(matches!(
        run(&indices(), "FROM people | STATS m = MAX(nope)"),
        Err(EsqlError::Verification(m)) if m.contains("Unknown column [nope]")
    ));
}

#[test]
fn stats_count_sum_avg() {
    let r = q("FROM people | STATS c = COUNT(city), s = SUM(age), a = AVG(age)");
    let types: Vec<&str> = r.columns.iter().map(|c| c.r#type.as_str()).collect();
    assert_eq!(types, ["long", "long", "double"]);
    assert_eq!(r.values, [[json!(2), json!(96), json!(32.0)]]);
    let r = q("FROM people | STATS a = AVG(age) BY has_city = city IS NOT NULL | SORT has_city");
    assert_eq!(
        r.values,
        [[json!(25.0), json!(false)], [json!(35.5), json!(true)]]
    );
    // Multi-values count value by value; doubles sum to a double.
    let r = q("ROW a = [1.5, 2.5] | STATS s = SUM(a), c = COUNT(a), m = AVG(a)");
    assert_eq!(r.columns[0].r#type, "double");
    assert_eq!(r.values, [[json!(4.0), json!(2), json!(2.0)]]);
    let r = q("FROM people | WHERE age > 99 | STATS c = COUNT(age), s = SUM(age), a = AVG(age)");
    assert_eq!(r.values, [[json!(0), Value::Null, Value::Null]]);
    assert!(matches!(
        run(&indices(), "FROM people | STATS s = SUM(name)"),
        Err(EsqlError::Verification(m)) if m.contains(
            "argument of [SUM(name)] must be [aggregate_metric_double, exponential_histogram, \
             tdigest or numeric except unsigned_long or counter types], found value [name] type \
             [keyword]"
        )
    ));
}

#[test]
fn stats_by_multi_value_joins_a_group_per_value() {
    let r = q("ROW a = [1, 2, 1], b = [\"x\", \"y\"] | STATS c = COUNT(*) BY a, b | SORT a, b");
    assert_eq!(
        r.values,
        [
            [json!(1), json!(1), json!("x")],
            [json!(1), json!(1), json!("y")],
            [json!(1), json!(2), json!("x")],
            [json!(1), json!(2), json!("y")]
        ]
    );
}

#[test]
fn unary_minus_negates_expressions() {
    let r = q(
        "FROM people | EVAL neg = -age, n2 = -(age + 1), d = -TO_DOUBLE(age), p = +age | KEEP neg, n2, d, p | SORT neg",
    );
    let types: Vec<&str> = r.columns.iter().map(|c| c.r#type.as_str()).collect();
    assert_eq!(types, ["long", "long", "double", "long"]);
    assert_eq!(
        r.values[0],
        [json!(-41), json!(-42), json!(-41.0), json!(41)]
    );
    // Math.negateExact overflows to null.
    let r = q("ROW a = -2147483648 | EVAL b = -a, -a");
    assert_eq!(col_names(&r), ["a", "b", "-a"]);
    assert_eq!(r.values, [[json!(-2147483648), Value::Null, Value::Null]]);
    match run(&indices(), "FROM people | EVAL x = -name") {
        Err(EsqlError::Verification(m)) => assert_eq!(
            m,
            "Found 1 problem\nline 1:24: argument of [-name] must be [numeric, date_period or time_duration], found value [name] type [keyword]"
        ),
        other => panic!("{other:?}"),
    }
}

/// An EVAL that redefines a document's column replaces it for every command after.
#[test]
fn eval_redefines_a_column() {
    let r = q(
        "FROM people | EVAL age = age * 2, age = age + 1, x = age | RENAME age AS a \
               | SORT a | KEEP name, a, x",
    );
    assert_eq!(
        r.values,
        [
            [json!("bob"), json!(51), json!(51)],
            [json!("ann"), json!(61), json!(61)],
            [json!("cid"), json!(83), json!(83)],
        ]
    );
}

/// A later EVAL sees and redefines what an earlier one added, past a WHERE that drops rows.
#[test]
fn evals_chain_through_a_where() {
    let r = q(
        "FROM people | EVAL a = age + 1 | WHERE a > 26 | EVAL a = a * 2, b = a \
               | RENAME a AS c | SORT c | KEEP name, b, c",
    );
    assert_eq!(
        r.values,
        [
            [json!("ann"), json!(62), json!(62)],
            [json!("cid"), json!(84), json!(84)],
        ]
    );
}

#[test]
fn rename_moves_columns_in_place() {
    let r = q("FROM people | RENAME name AS who, age AS years | SORT years");
    assert_eq!(col_names(&r), ["years", "city", "city.keyword", "who"]);
    assert_eq!(
        r.values[0],
        [json!(25), Value::Null, Value::Null, json!("bob")]
    );
    // A renamed column replaces one of the same name; renames chain; `new = old` works too.
    let r = q("ROW a = 1, b = 2, c = 3 | RENAME a AS b, b AS d, e = c, x AS x");
    assert_eq!(col_names(&r), ["d", "e"]);
    assert_eq!(r.values, [[json!(1), json!(3)]]);
    for (query, problem) in [
        (
            "FROM people | RENAME nope AS x",
            "line 1:22: Unknown column [nope]",
        ),
        (
            "ROW a = 1 | RENAME a AS b, a AS c",
            "line 1:28: Column [a] renamed to [b] and is no longer available [a AS c]",
        ),
    ] {
        match run(&indices(), query) {
            Err(EsqlError::Verification(m)) => assert_eq!(m, format!("Found 1 problem\n{problem}")),
            other => panic!("{other:?}"),
        }
    }
}

#[test]
fn string_functions_match_elasticsearch() {
    let r = q(
        r#"ROW a = CONCAT("x", "-", "é"), b = LENGTH("Héloïse"), c = TO_LOWER("HéLOÏse"), d = TO_UPPER("Héloïse"), e = STARTS_WITH("hello", "he"), f = ENDS_WITH("hello", "x"), g = CONCAT("x", null), h = LENGTH(["a", "b"]), i = TO_UPPER(null)"#,
    );
    let types: Vec<&str> = r.columns.iter().map(|c| c.r#type.as_str()).collect();
    assert_eq!(
        types,
        [
            "keyword", "integer", "keyword", "keyword", "boolean", "boolean", "keyword", "integer",
            "keyword"
        ]
    );
    assert_eq!(
        r.values,
        [[
            json!("x-é"),
            json!(7),
            json!("héloïse"),
            json!("HÉLOÏSE"),
            json!(true),
            json!(false),
            Value::Null,
            Value::Null,
            Value::Null
        ]]
    );
    let r = q(
        r#"FROM people | WHERE STARTS_WITH(name, "a") OR ENDS_WITH(city, "me") | EVAL n = CONCAT(name, "/", city) | KEEP n | SORT n"#,
    );
    assert_eq!(r.values, [[json!("ann/Oslo")], [json!("cid/Rome")]]);
    for (query, problem) in [
        (
            "FROM people | EVAL x = CONCAT(name, age)",
            "line 1:24: argument of [CONCAT(name, age)] must be [string], found value [age] type [long]",
        ),
        (
            "FROM people | EVAL x = STARTS_WITH(name, age)",
            "line 1:24: second argument of [STARTS_WITH(name, age)] must be [string], found value [age] type [long]",
        ),
        (
            "FROM people | EVAL x = LENGTH(age)",
            "line 1:24: argument of [LENGTH(age)] must be [string], found value [age] type [long]",
        ),
    ] {
        match run(&indices(), query) {
            Err(EsqlError::Verification(m)) => assert_eq!(m, format!("Found 1 problem\n{problem}")),
            other => panic!("{other:?}"),
        }
    }
}

#[test]
fn case_and_coalesce_match_elasticsearch() {
    let r = q(
        r#"FROM people | EVAL level = CASE(age >= 40, "senior", age >= 30, "mid", "junior"), c = COALESCE(city, name), x = CASE(age > 99, 1), d = COALESCE(null, age, 2), e = CASE(age < 30, 1.5, age) | KEEP name, level, c, x, d, e | SORT name"#,
    );
    let types: Vec<&str> = r.columns.iter().map(|c| c.r#type.as_str()).collect();
    assert_eq!(
        types,
        ["keyword", "keyword", "keyword", "integer", "long", "double"]
    );
    assert_eq!(
        r.values,
        [
            [
                json!("ann"),
                json!("mid"),
                json!("Oslo"),
                Value::Null,
                json!(30),
                json!(30.0)
            ],
            [
                json!("bob"),
                json!("junior"),
                json!("bob"),
                Value::Null,
                json!(25),
                json!(1.5)
            ],
            [
                json!("cid"),
                json!("senior"),
                json!("Rome"),
                Value::Null,
                json!(41),
                json!(41.0)
            ],
        ]
    );
    // A multi-valued condition is false; multi-valued values pass through.
    let r = q("ROW a = CASE([true, false], 1, 2), b = COALESCE(null, [1, 2]), c = CASE(null, 1)");
    assert_eq!(r.values, [[json!(2), json!([1, 2]), Value::Null]]);
    for (query, problem) in [
        (
            "FROM people | EVAL x = CASE(age, 1)",
            "line 1:24: first argument of [CASE(age, 1)] must be [boolean], found value [age] type [long]",
        ),
        (
            r#"FROM people | EVAL x = CASE(age > 1, "a", 2)"#,
            r#"line 1:24: third argument of [CASE(age > 1, "a", 2)] must be [keyword], found value [2] type [integer]"#,
        ),
        // Numbers only widen to the first numeric argument's type.
        (
            "FROM people | EVAL x = COALESCE(1, age)",
            "line 1:24: second argument of [COALESCE(1, age)] must be [integer], found value [age] type [long]",
        ),
    ] {
        match run(&indices(), query) {
            Err(EsqlError::Verification(m)) => assert_eq!(m, format!("Found 1 problem\n{problem}")),
            other => panic!("{other:?}"),
        }
    }
}

#[test]
fn from_reads_fields_like_doc_values() {
    let mut i = Index::default();
    let p = &mut i.mappings.properties;
    p.insert(
        "d".into(),
        FieldMapping::Date {
            format: None,
            locale: None,
            fields: BTreeMap::new(),
        },
    );
    p.insert("n".into(), FieldMapping::Long);
    p.insert(
        "k".into(),
        FieldMapping::Keyword {
            ignore_above: None,
            split_queries_on_whitespace: false,
        },
    );
    i.put(
        "1",
        json!({"d": "2019-03-15T09:30:00Z", "n": [9, 1, 9], "k": ["b", "a", "b"], "t": ["y", "x"]}),
    );
    let all = BTreeMap::from([("x".to_owned(), i)]);
    let r = run(&all, "FROM x | KEEP d, n, k, t").unwrap();
    assert_eq!(
        r.values,
        [[
            json!("2019-03-15T09:30:00.000Z"),
            json!([1, 9, 9]),
            json!(["a", "b"]),
            json!(["y", "x"])
        ]]
    );
}

#[test]
fn like_matches_wildcards() {
    let names =
        |query: &str| -> Vec<Value> { q(query).values.into_iter().map(|r| r[0].clone()).collect() };
    assert_eq!(
        names(r#"FROM people | WHERE name LIKE "?o*" | KEEP name"#),
        [json!("bob")]
    );
    assert_eq!(
        names(r#"FROM people | WHERE city NOT LIKE "*m*" | KEEP name"#),
        [json!("ann")]
    );
    let r = q(
        r#"ROW a = "x*y", b = ["x"] | EVAL e = a LIKE "x\\*y", s = a LIKE "x\\*", m = b LIKE "x""#,
    );
    assert_eq!(r.values[0][2..], [json!(true), json!(false), Value::Null]);
    assert_eq!(
        run(&indices(), r#"FROM people | WHERE age LIKE "3*""#).unwrap_err(),
        EsqlError::Verification(
            "Found 1 problem\nline 1:21: argument of [age LIKE \"3*\"] must be [string], found value [age] type [long]".into()
        )
    );
}

#[test]
fn in_matches_any_listed_value() {
    let names =
        |query: &str| -> Vec<Value> { q(query).values.into_iter().map(|r| r[0].clone()).collect() };
    assert_eq!(
        names(r#"FROM people | WHERE name IN ("bob", "zed", name) | KEEP name | SORT name"#),
        [json!("ann"), json!("bob"), json!("cid")]
    );
    assert_eq!(
        names("FROM people | WHERE age NOT IN (30, 41) | KEEP name"),
        [json!("bob")]
    );
    let r = q("ROW a = 1 | EVAL t = a IN (2, 1), n = a IN (2, null), m = [1, 2] IN (1)");
    assert_eq!(r.values[0][1..], [json!(true), Value::Null, Value::Null]);
}

#[test]
fn stats_median_percentile() {
    let r =
        q("FROM people | STATS m = MEDIAN(age), p = PERCENTILE(age, 75), p0 = PERCENTILE(age, 0)");
    let types: Vec<&str> = r.columns.iter().map(|c| c.r#type.as_str()).collect();
    assert_eq!(types, ["double", "double", "double"]);
    assert_eq!(r.values, [[json!(30.0), json!(35.5), json!(25.0)]]);
    // Out-of-range percentiles and empty groups are null.
    let r = q(
        "FROM people | STATS p = PERCENTILE(age, 101), m = MEDIAN(age) BY name | SORT name | LIMIT 1",
    );
    assert_eq!(r.values, [[Value::Null, json!(30.0), json!("ann")]]);
    let r = q("FROM people | WHERE age > 99 | STATS m = MEDIAN(age)");
    assert_eq!(r.values, [[Value::Null]]);
    assert!(matches!(
        run(&indices(), "FROM people | STATS m = MEDIAN(name)"),
        Err(EsqlError::Verification(m)) if m.contains(
            "argument of [MEDIAN(name)] must be [exponential_histogram, tdigest or numeric except \
             unsigned_long or counter types], found value [name] type [keyword]"
        )
    ));
    assert!(matches!(
        run(&indices(), "FROM people | STATS p = PERCENTILE(age, name)"),
        Err(EsqlError::Verification(m)) if m.contains(
            "second argument of [PERCENTILE(age, name)] must be [numeric except unsigned_long], \
             found value [name] type [keyword]"
        )
    ));
}

#[test]
fn stats_count_distinct() {
    let r = q("FROM people | STATS n = COUNT_DISTINCT(name), c = COUNT_DISTINCT(city)");
    assert_eq!(r.columns[0].r#type, "long");
    assert_eq!(r.values, [[json!(3), json!(2)]]);
    // Multi-values count value by value, of every type.
    let r = q(
        "ROW i = [1, 2, 2, 3], d = [1.5, 1.5], b = [true, false, true] \
               | STATS i = COUNT_DISTINCT(i), d = COUNT_DISTINCT(d), b = COUNT_DISTINCT(b)",
    );
    assert_eq!(r.values, [[json!(3), json!(1), json!(2)]]);
    let r = q("FROM people | WHERE age > 99 | STATS d = COUNT_DISTINCT(age)");
    assert_eq!(r.values, [[json!(0)]]);
}

#[test]
fn stats_values() {
    let r = q("FROM people | STATS v = VALUES(name), a = VALUES(age)");
    let types: Vec<&str> = r.columns.iter().map(|c| c.r#type.as_str()).collect();
    assert_eq!(types, ["keyword", "long"]);
    assert_eq!(
        r.values,
        [[json!(["ann", "bob", "cid"]), json!([30, 25, 41])]]
    );
    // Distinct values in the order they come; one value is not a list, none is null.
    let r = q("ROW a = [3, 1, 3] | STATS v = VALUES(a)");
    assert_eq!(r.values, [[json!([3, 1])]]);
    let r = q("FROM people | WHERE name == \"bob\" | STATS v = VALUES(name)");
    assert_eq!(r.values, [[json!("bob")]]);
    let r = q("FROM people | WHERE age > 99 | STATS v = VALUES(name)");
    assert_eq!(r.values, [[Value::Null]]);
}

#[test]
fn stats_by_bucket() {
    let r = q("FROM people | STATS c = COUNT(*) BY b = BUCKET(age, 10) | SORT b");
    assert_eq!(r.columns[1].r#type, "double");
    let b: Vec<&Value> = r.values.iter().map(|v| &v[1]).collect();
    assert_eq!(b, [&json!(20.0), &json!(30.0), &json!(40.0)]);
    // Ten buckets over 20..60 round to a span of 5.
    let r = q("FROM people | STATS c = COUNT(*) BY b = BUCKET(age, 10, 20, 60) | SORT b");
    let b: Vec<&Value> = r.values.iter().map(|v| &v[1]).collect();
    assert_eq!(b, [&json!(25.0), &json!(30.0), &json!(40.0)]);
    let date = |by: &str| {
        let r = q(&format!(
            "ROW d = TO_DATETIME(\"2019-03-15T10:30:00Z\") | STATS c = COUNT(*) BY b = {by}"
        ));
        assert_eq!(r.columns[1].r#type, "date");
        r.values[0][1].clone()
    };
    assert_eq!(
        date("BUCKET(d, 1 month)"),
        json!("2019-03-01T00:00:00.000Z")
    );
    assert_eq!(
        date("BUCKET(d, 2 hours)"),
        json!("2019-03-15T10:00:00.000Z")
    );
    // The finest rounding giving at most that many buckets over the range: 3 hours, 12 hours.
    let day = "\"2019-03-15T00:00:00Z\", \"2019-03-16T00:00:00Z\"";
    assert_eq!(
        date(&format!("BUCKET(d, 8, {day})")),
        json!("2019-03-15T09:00:00.000Z")
    );
    assert_eq!(
        date(&format!("BUCKET(d, 7, {day})")),
        json!("2019-03-15T00:00:00.000Z")
    );
    // One bucket over a day is a day; when not even years fit, years.
    assert_eq!(
        date(&format!("BUCKET(d, 1, {day})")),
        json!("2019-03-15T00:00:00.000Z")
    );
    let year = "\"2010-01-01T00:00:00Z\", \"2025-01-01T00:00:00Z\"";
    assert_eq!(
        date(&format!("BUCKET(d, 5, {year})")),
        json!("2019-01-01T00:00:00.000Z")
    );
    assert!(matches!(
        run(&indices(), "FROM people | STATS c = COUNT(*) BY b = BUCKET(name, 10)"),
        Err(EsqlError::Verification(m)) if m.contains(
            "first argument of [BUCKET(name, 10)] must be [datetime or numeric], found value \
             [name] type [keyword]"
        )
    ));
    assert!(matches!(
        run(&indices(), "FROM people | STATS c = COUNT(*) BY b = BUCKET(age, 10, 20)"),
        Err(EsqlError::Verification(m)) if m.contains("function expects exactly")
    ));
}

#[test]
fn mapping_validation_empty_objects_are_not_columns() {
    let mut i = Index::default();
    let doc = json!({"e": {}, "n": 1});
    i.map_dynamic("1", &doc).unwrap();
    i.put("1", doc);
    let r = run(&BTreeMap::from([("i".to_owned(), i)]), "FROM i").unwrap();
    assert_eq!(col_names(&r), ["n"]);
}

/// Elasticsearch 9.5.5 fails a call with the wrong number of arguments while parsing, before
/// verification finds other problems.
#[test]
fn esql_es_semantics_arity_errors_are_parse_errors() {
    let err = |q: &str| match run(&indices(), q) {
        Err(EsqlError::Parse(m)) => m,
        other => panic!("{q}: {other:?}"),
    };
    for (call, msg) in [
        ("ABS()", "[abs]: expects exactly one argument"),
        ("ABS(1, 2)", "[abs]: expects exactly one argument"),
        (
            "SUBSTRING(\"a\")",
            "[substring]: expects two or three arguments",
        ),
        ("CONCAT(\"a\")", "[concat]: expects at least two arguments"),
        ("ROUND(1, 2, 3)", "[round]: expects one or two arguments"),
        ("COALESCE()", "[coalesce]: expects at least one argument"),
        ("NOW(1)", "[now]: expects no arguments"),
        (
            "DATE_EXTRACT(\"year\")",
            "[date_extract]: expects exactly two arguments",
        ),
        (
            "BUCKET(1)",
            "[bucket]: expects minimum two, maximum four arguments",
        ),
        ("CASE(true)", "[case]: expects at least two arguments"),
        ("TO_LONG()", "[to_long]: expects one or two arguments"),
        (
            "to_string(1, 2, 3)",
            "[to_string]: expects exactly one argument",
        ),
    ] {
        assert_eq!(
            err(&format!("ROW a = {call}")),
            format!("line 1:9: error building {msg}")
        );
    }
    assert_eq!(
        err("ROW a = nope | EVAL b = FOO(ABS())"),
        "line 1:29: error building [abs]: expects exactly one argument"
    );
}

/// The tokens Elasticsearch 9.5.5 expects after an expression depend on where it is: a list
/// item may be followed by `,`, a STATS aggregate by `where` and `by`, a field in parentheses
/// by `.`.
#[test]
fn esql_es_semantics_expected_tokens_after_expressions() {
    let err = |q: &str| match run(&indices(), q) {
        Err(EsqlError::Parse(m)) => m,
        other => panic!("{q}: {other:?}"),
    };
    let ops = "'or', '+', '-', '*', '/', '%'}";
    for (q, msg) in [
        ("ROW a = 1 2 3", format!("1:11: mismatched input '2' expecting {{<EOF>, '|', 'and', '::', ',', {ops}")),
        ("ROW a = ABS(1) 2 3", format!("1:16: mismatched input '2' expecting {{<EOF>, '|', 'and', '::', ',', {ops}")),
        ("ROW a = 1 days 2 hours", format!("1:16: mismatched input '2' expecting {{<EOF>, '|', 'and', '::', ',', {ops}")),
        ("ROW a = 1 | WHERE ABS(1) 2 3", format!("1:26: mismatched input '2' expecting {{<EOF>, '|', 'and', '::', {ops}")),
        ("ROW a = 1 | EVAL b = a 2 3", format!("1:24: mismatched input '2' expecting {{<EOF>, '|', 'and', '::', ',', '.', {ops}")),
        ("ROW a = 1 | STATS c = COUNT(*) BY a 2 3", format!("1:37: mismatched input '2' expecting {{<EOF>, '|', 'and', '::', ',', '.', {ops}")),
        ("ROW a = 1 | STATS c = COUNT(*) 2 3", format!("1:32: mismatched input '2' expecting {{<EOF>, 'where', '|', 'and', 'by', '::', ',', {ops}")),
        ("ROW a = 1 | EVAL b = a is null 2 3", "1:32: mismatched input '2' expecting {<EOF>, '|', 'and', ',', 'or'}".into()),
        ("ROW a = 1 | EVAL b = (a) 2 3", "1:26: mismatched input '2' expecting {'and', '::', '.', 'or', '+', '-', '*', '/', '%', ')'}".into()),
        ("ROW a = (1) 2 3", "1:13: mismatched input '2' expecting {'and', '::', 'or', '+', '-', '*', '/', '%', ')'}".into()),
    ] {
        assert_eq!(err(q), format!("line {msg}"), "{q}");
    }
}

/// Elasticsearch 9.5.5 reads an integer followed by an identifier as a time span, so an unknown
/// unit, or a span as a LIMIT, is a parse error of its own.
#[test]
fn esql_es_semantics_time_span_units() {
    let err = |q: &str| match run(&indices(), q) {
        Err(EsqlError::Parse(m)) => m,
        other => panic!("{q}: {other:?}"),
    };
    for (q, msg) in [
        ("ROW a = 1 foo", "1:9: Unexpected temporal unit: 'foo'"),
        ("ROW a = -1 foo", "1:9: Unexpected temporal unit: 'foo'"),
        ("ROW a = 1 as", "1:9: Unexpected temporal unit: 'as'"),
        (
            "ROW a = DATE_TRUNC(1 foo, NOW())",
            "1:20: Unexpected temporal unit: 'foo'",
        ),
        (
            "ROW a = 1 | LIMIT 1 x | WHERE b",
            "1:19: Unexpected temporal unit: 'x'",
        ),
        (
            "ROW a = 1 | LIMIT 1 day",
            "1:13: value of [LIMIT 1 day] must be a non negative integer, found value [1day] \
             type [date_period]",
        ),
        (
            "ROW a = 1 | LIMIT 2 HOURS",
            "1:13: value of [LIMIT 2 HOURS] must be a non negative integer, found value \
             [2HOURS] type [time_duration]",
        ),
        (
            "ROW a = 1 asc",
            "1:11: extraneous input 'asc' expecting <EOF>",
        ),
        (
            "ROW a = 1 where",
            "1:11: extraneous input 'where' expecting <EOF>",
        ),
        (
            "ROW a = 1 `days`",
            "1:11: extraneous input '`days`' expecting <EOF>",
        ),
        (
            "ROW a = 1.5 days",
            "1:13: extraneous input 'days' expecting <EOF>",
        ),
    ] {
        assert_eq!(err(q), format!("line {msg}"), "{q}");
    }
}

/// `*` multiplies in an expression even without spaces, and is a wildcard only in a pattern.
#[test]
fn esql_es_semantics_asterisk_multiplies_in_expressions() {
    let r =
        q("ROW a = 3 | EVAL b = a*2, c = a *2, d = a* 2, e = 2*a, f = a*-1, g = a*(2), h = a*2*a");
    assert_eq!(r.values, [[3, 6, 6, 6, 6, -3, 6, 18].map(|n| json!(n))]);
    let r = q("ROW ab = 1, ac = 2 | KEEP a*c");
    assert_eq!(col_names(&r), ["ac"]);
    let r = q("FROM people | STATS c = COUNT(*)");
    assert_eq!(r.values, [[json!(3)]]);
}

/// Elasticsearch 9.5.5 allows BUCKET only in STATS.
#[test]
fn esql_es_semantics_bucket_only_in_stats() {
    let err = |q: &str| match run(&indices(), q) {
        Err(EsqlError::Verification(m)) => m,
        other => panic!("{q}: {other:?}"),
    };
    let outside = |at: &str, call: &str| {
        format!(
            "\nline 1:{at}: cannot use grouping function [{call}] outside of a STATS or LIMIT BY \
             command"
        )
    };
    assert_eq!(
        err("ROW a = 5 | EVAL b = BUCKET(a, 2), c = BUCKET(a, 3)"),
        format!(
            "Found 2 problems{}{}",
            outside("22", "BUCKET(a, 2)"),
            outside("40", "BUCKET(a, 3)")
        )
    );
    for (q, at) in [
        ("ROW a = 5 | WHERE BUCKET(a, 2) > 0", "19"),
        ("ROW a = BUCKET(5, 2)", "9"),
        ("ROW a = 5 | EVAL b = ABS(BUCKET(a, 2))", "26"),
    ] {
        let call = &q[q.find("BUCKET").unwrap()..][..12];
        assert_eq!(
            err(q),
            format!("Found 1 problem{}", outside(at, call)),
            "{q}"
        );
    }
    assert_eq!(
        err("ROW a = 5 | EVAL b = BUCKET(nope, 2)"),
        "Found 1 problem\nline 1:29: Unknown column [nope]"
    );
    let r = q("ROW a = 5 | STATS c = COUNT(*) BY b = BUCKET(a, 2) + 1");
    assert_eq!(r.values, [[json!(1), json!(5.0)]]);
}

/// Elasticsearch 9.5.5's messages for DATE_TRUNC's arguments, which name the date types in the
/// other order than DATE_EXTRACT and DATE_FORMAT.
#[test]
fn esql_es_semantics_date_function_argument_messages() {
    let err = |q: &str| match run(&indices(), q) {
        Err(EsqlError::Verification(m)) => m,
        other => panic!("{q}: {other:?}"),
    };
    for (q, msg) in [
        (
            "ROW a = DATE_TRUNC(\"x\", NOW())",
            "1:9: first argument of [DATE_TRUNC(\"x\", NOW())] must be [dateperiod or \
             timeduration], found value [\"x\"] type [keyword]",
        ),
        (
            "ROW a = DATE_TRUNC(1, NOW())",
            "1:9: first argument of [DATE_TRUNC(1, NOW())] must be [dateperiod or \
             timeduration], found value [1] type [integer]",
        ),
        (
            "ROW a = 5 | EVAL b = DATE_TRUNC(1 day, a)",
            "1:22: second argument of [DATE_TRUNC(1 day, a)] must be [date_nanos or datetime], \
             found value [a] type [integer]",
        ),
        (
            "ROW a = 5 | EVAL b = DATE_EXTRACT(\"year\", a)",
            "1:22: second argument of [DATE_EXTRACT(\"year\", a)] must be [datetime or \
             date_nanos], found value [a] type [integer]",
        ),
        (
            "ROW a = 5 | EVAL b = DATE_FORMAT(a)",
            "1:22: first argument of [DATE_FORMAT(a)] must be [datetime or date_nanos], found \
             value [a] type [integer]",
        ),
    ] {
        assert_eq!(err(q), format!("Found 1 problem\nline {msg}"), "{q}");
    }
}

/// Elasticsearch 9.5.5's parse errors for a malformed RENAME.
#[test]
fn esql_es_semantics_rename_parse_errors() {
    let err = |q: &str| match run(&indices(), q) {
        Err(EsqlError::Parse(m)) => m,
        other => panic!("{q}: {other:?}"),
    };
    let id = "{'?', '??', NAMED_OR_POSITIONAL_PARAM, NAMED_OR_POSITIONAL_DOUBLE_PARAMS, '[', \
              ID_PATTERN}";
    for (q, msg) in [
        (
            "ROW a = 1 | RENAME a b",
            "1:22: no viable alternative at input 'a b'".into(),
        ),
        (
            "ROW a = 1 | RENAME a b c",
            "1:22: no viable alternative at input 'a b'".into(),
        ),
        (
            "ROW a = 1 | RENAME a",
            "1:21: no viable alternative at input 'a'".into(),
        ),
        (
            "ROW a = 1 | RENAME a, b",
            "1:21: no viable alternative at input 'a,'".into(),
        ),
        (
            "ROW a = 1 | RENAME a AS",
            format!("1:24: mismatched input '<EOF>' expecting {id}"),
        ),
        (
            "ROW a = 1 | RENAME a = ",
            format!("1:24: mismatched input '<EOF>' expecting {id}"),
        ),
        (
            "ROW a = 1 | RENAME a AS b c",
            "1:27: extraneous input 'c' expecting <EOF>".into(),
        ),
    ] {
        assert_eq!(err(q), format!("line {msg}"), "{q}");
    }
}

/// Like Elasticsearch 9.5.5 (which also warns), an overflowing long SUM is null.
#[test]
fn esql_es_semantics_overflowing_long_sum_is_null() {
    let mut i = Index::default();
    i.mappings.properties.insert("v".into(), FieldMapping::Long);
    i.put("1", json!({"v": i64::MAX}));
    i.put("2", json!({"v": 1}));
    let indices = BTreeMap::from([("i".to_owned(), i)]);
    let r = run(&indices, "FROM i | STATS s = SUM(v), m = MAX(v)").unwrap();
    assert_eq!(r.values, [[Value::Null, json!(i64::MAX)]]);
}

/// Elasticsearch 9.5.5 fails an invalid RLIKE pattern while parsing, with Lucene's reason.
#[test]
fn esql_es_semantics_invalid_rlike_patterns() {
    let err = |q: &str| match run(&indices(), q) {
        Err(EsqlError::Parse(m)) => m,
        other => panic!("{q}: {other:?}"),
    };
    for (p, reason) in [
        ("[a", "expected ']' at position 2"),
        ("a{2", "expected '}' at position 3"),
        ("(a", "expected ')' at position 2"),
        ("a\\\\", "unexpected end-of-string"),
        ("a{3,2}", "invalid repetition range(out of order): 3..2"),
        ("<1-a>", "interval syntax error at position 4"),
        ("a)", "end-of-string expected at position 1"),
        ("\\\\p", "invalid character class \\112"),
    ] {
        let shown = p.replace("\\\\", "\\");
        assert_eq!(
            err(&format!(
                "ROW a = \"x\" | WHERE a RLIKE \"{p}\" | WHERE nope"
            )),
            format!("line 1:21: Invalid regex pattern for RLIKE [{shown}]: [{reason}]")
        );
    }
}

/// Like Java's `Double.parseDouble`, string conversions read hexadecimal floats.
#[test]
fn esql_es_semantics_hexadecimal_floats() {
    let r = q(
        "ROW a = TO_DOUBLE(\"0x1p3\"), b = TO_DOUBLE(\"0x1.8p1\"), c = TO_DOUBLE(\"0x10\"), \
               d = TO_DOUBLE(\"0X1P-2d\"), e = TO_DOUBLE(\"-0x1p3\"), f = TO_DOUBLE(\"0x.8p1\"), \
               g = TO_INTEGER(\"0x1p3\"), h = TO_LONG(\"0x1p3\"), \
               i = TO_DOUBLE(\"0x1.fffffffffffff8p0\"), j = TO_DOUBLE(\"0x1.00000000000008000001p0\")",
    );
    assert_eq!(
        r.values,
        [[
            json!(8.0),
            json!(3.0),
            Value::Null,
            json!(0.25),
            json!(-8.0),
            json!(1.0),
            json!(8),
            json!(8),
            json!(2.0),
            json!(1.0000000000000002)
        ]]
    );
}

/// Like Elasticsearch 9.5.5's `DateTrunc.createRounding`: several months, quarters or years
/// round to intervals counted from year 1, and 7 days is a calendar week.
#[test]
fn esql_es_semantics_date_trunc_multiple_calendar_units() {
    let r = q(
        "ROW d = TO_DATETIME(\"2024-05-17T10:11:12Z\") | EVAL a = DATE_TRUNC(3 months, d), \
               b = DATE_TRUNC(2 years, d), c = DATE_TRUNC(2 quarters, d), e = DATE_TRUNC(2 weeks, d), \
               f = DATE_TRUNC(5 years, TO_DATETIME(\"1965-03-01T00:00:00Z\")), \
               g = DATE_TRUNC(7 months, TO_DATETIME(\"1969-12-31T00:00:00Z\")), \
               h = DATE_TRUNC(7 days, d) | DROP d",
    );
    let dates = [
        "2024-04-01",
        "2023-01-01",
        "2024-01-01",
        "2024-05-09",
        "1961-01-01",
        "1969-10-01",
        "2024-05-13",
    ];
    assert_eq!(
        r.values,
        [dates.map(|d| json!(format!("{d}T00:00:00.000Z")))]
    );
}

/// Elasticsearch 9.5.5 refuses spatial types in MV_MIN and MV_MAX; es-mini has no spatial types,
/// so its spatial conversions are unknown functions.
#[test]
fn esql_es_semantics_no_spatial_types() {
    assert_eq!(
        run(&indices(), "ROW a = MV_MIN(TO_GEOPOINT(\"POINT(1 2)\"))"),
        Err(EsqlError::Verification(
            "Found 1 problem\nline 1:16: Unknown function [TO_GEOPOINT]".into()
        ))
    );
}

/// Elasticsearch 9.5.5 fails the query on a negative SUBSTRING length (400
/// `illegal_argument_exception`).
#[test]
fn esql_es_semantics_negative_substring_length_fails() {
    assert_eq!(
        run(
            &indices(),
            "FROM people | EVAL n = -1 | EVAL s = SUBSTRING(name, 1, n)"
        ),
        Err(EsqlError::Failed {
            status: 400,
            kind: "illegal_argument_exception",
            reason: "Length parameter cannot be negative, found [-1]".into()
        })
    );
    assert_eq!(
        q("ROW s = SUBSTRING(\"abc\", 2), t = SUBSTRING(\"abc\", 1, 0)").values,
        [[json!("bc"), json!("")]]
    );
}

/// A WHERE right after an EVAL reads its values, with its own expressions' types resolved.
#[test]
fn where_after_eval_reads_its_values() {
    let r = q("FROM people | EVAL a2 = age * 2 | WHERE a2 - 50 > 0 | KEEP name, a2");
    assert_eq!(
        r.values,
        [[json!("ann"), json!(60)], [json!("cid"), json!(82)]]
    );
}

/// An EVAL evaluates every row before the WHERE after it, so its failure on a later row wins
/// over the WHERE's on an earlier one.
#[test]
fn eval_fails_before_the_where_after_it() {
    assert_eq!(
        run(
            &indices(),
            "FROM people | EVAL s = SUBSTRING(name, 1, TO_INTEGER(40 - age)) \
             | WHERE SUBSTRING(name, 1, TO_INTEGER(age - 27)) == \"a\""
        ),
        Err(EsqlError::Failed {
            status: 400,
            kind: "illegal_argument_exception",
            reason: "Length parameter cannot be negative, found [-1]".into()
        })
    );
}

/// The EVAL's failure on a later row still wins when the WHERE after it failed on several earlier
/// rows.
#[test]
fn eval_fails_before_the_where_after_it_that_failed_twice() {
    assert_eq!(
        run(
            &indices(),
            "FROM people | EVAL s = SUBSTRING(name, 1, TO_INTEGER(40 - age)) \
             | WHERE SUBSTRING(name, 1, TO_INTEGER(age - 41)) == \"a\""
        ),
        Err(EsqlError::Failed {
            status: 400,
            kind: "illegal_argument_exception",
            reason: "Length parameter cannot be negative, found [-1]".into()
        })
    );
}

/// Like Elasticsearch 9.5.5, ROUND past the long range fails the query (500
/// `arithmetic_exception`), while integers and unsigned longs round to null (with a warning).
#[test]
fn esql_es_semantics_round_overflow() {
    assert_eq!(
        run(
            &indices(),
            "ROW x = 9223372036854775807 | EVAL a = ROUND(x, -1)"
        ),
        Err(EsqlError::Failed {
            status: 500,
            kind: "arithmetic_exception",
            reason: "long overflow".into()
        })
    );
    let r = q(
        "ROW x = 2147483647 | EVAL a = ROUND(x, -1), b = ROUND(18446744073709551615, -1), \
               c = ROUND(9223372036854775807, -2)",
    );
    assert_eq!(
        r.values,
        [[
            json!(2147483647),
            Value::Null,
            Value::Null,
            json!(9223372036854775800i64)
        ]]
    );
}

/// Like Elasticsearch 9.5.5, CONCAT past 1MB fails the query (400 `esql_client_exception`).
#[test]
fn esql_es_semantics_concat_is_capped_at_1mb() {
    let s = "a".repeat(524_288);
    let r = q(&format!(
        "ROW s = \"{s}\" | EVAL n = LENGTH(CONCAT(s, s)) | KEEP n"
    ));
    assert_eq!(r.values, [[json!(1_048_576)]]);
    assert_eq!(
        run(
            &indices(),
            &format!("ROW s = \"{s}\" | EVAL n = LENGTH(CONCAT(s, s, \"é\"))")
        ),
        Err(EsqlError::Failed {
            status: 400,
            kind: "esql_client_exception",
            reason: "concatenating more than [1048576] bytes is not supported".into()
        })
    );
}

/// Past 2^63, Elasticsearch 9.5.5's ROUND divides in `BigDecimal`; these results (an ulp off the
/// input) are the ones it returns.
#[test]
fn esql_es_semantics_round_past_2_63() {
    let r = q(
        "ROW a = ROUND(-22612367103578135000000000000000000000000000000000000000000000.0, -27), \
               b = ROUND(-11199114434478000000000000000000000000000000000.0, -12), \
               c = ROUND(283768897543175020000000000000000000.0, 1), \
               d = ROUND(30068952992873390000.0, 6)",
    );
    assert_eq!(
        r.values,
        [[
            json!(-2.2612367103578138e61),
            json!(-1.1199114434478002e46),
            json!(2.83768897543175e35),
            json!(3.006895299287339e19)
        ]]
    );
}

/// Like Elasticsearch 9.5.5's `ImplicitCasting`, CASE and COALESCE cast whole numbers after an
/// unsigned_long to unsigned_long (a negative value is null), but not an unsigned_long after a
/// long or integer.
#[test]
fn esql_es_semantics_conditionals_widen_to_unsigned_long() {
    let r = q(
        "ROW x = [1, -2, 3] | EVAL a = COALESCE(18446744073709551615, 1), \
               b = CASE(false, 18446744073709551615, 7), c = CASE(false, 18446744073709551615, -1), \
               d = CASE(false, 18446744073709551615, x), e = COALESCE(1.5, 18446744073709551615), \
               f = CASE(false, 2.5, 18446744073709551615) | DROP x",
    );
    let types: Vec<&str> = r.columns.iter().map(|c| c.r#type.as_str()).collect();
    assert_eq!(
        types,
        [
            "unsigned_long",
            "unsigned_long",
            "unsigned_long",
            "unsigned_long",
            "double",
            "double"
        ]
    );
    assert_eq!(
        r.values,
        [[
            json!(18446744073709551615u64),
            json!(7),
            Value::Null,
            json!([1, 3]),
            json!(1.5),
            json!(1.8446744073709552e19)
        ]]
    );
    assert_eq!(
        run(&indices(), "ROW a = COALESCE(1, 18446744073709551615)"),
        Err(EsqlError::Verification(
            "Found 1 problem\nline 1:9: second argument of [COALESCE(1, 18446744073709551615)] \
             must be [integer], found value [18446744073709551615] type [unsigned_long]"
                .into()
        ))
    );
}

/// DATE_EXTRACT reads every `ChronoField`, as in Elasticsearch 9.5.5. An unknown part fails the
/// query when it is a literal, and is null when it comes from a field (where ES warns).
#[test]
fn esql_es_semantics_date_extract_parts() {
    const FIELDS: &str = "NANO_OF_SECOND NANO_OF_DAY MICRO_OF_SECOND MICRO_OF_DAY \
        MILLI_OF_SECOND MILLI_OF_DAY SECOND_OF_MINUTE SECOND_OF_DAY MINUTE_OF_HOUR MINUTE_OF_DAY \
        HOUR_OF_AMPM CLOCK_HOUR_OF_AMPM HOUR_OF_DAY CLOCK_HOUR_OF_DAY AMPM_OF_DAY DAY_OF_WEEK \
        ALIGNED_DAY_OF_WEEK_IN_MONTH ALIGNED_DAY_OF_WEEK_IN_YEAR DAY_OF_MONTH DAY_OF_YEAR \
        EPOCH_DAY ALIGNED_WEEK_OF_MONTH ALIGNED_WEEK_OF_YEAR MONTH_OF_YEAR PROLEPTIC_MONTH \
        YEAR_OF_ERA YEAR ERA INSTANT_SECONDS OFFSET_SECONDS";
    let extract = |millis: i64| {
        let cols: Vec<String> = FIELDS
            .split_whitespace()
            .map(|f| format!("{f} = DATE_EXTRACT(\"{}\", d)", f.to_lowercase()))
            .collect();
        let r = q(&format!(
            "ROW d = TO_DATETIME({millis}) | EVAL {} | DROP d",
            cols.join(", ")
        ));
        r.values[0].clone()
    };
    // 2024-12-31T00:05:06.789Z
    let late: [i64; 30] = [
        789000000,
        306789000000,
        789000,
        306789000,
        789,
        306789,
        6,
        306,
        5,
        5,
        0,
        12,
        0,
        24,
        0,
        2,
        3,
        2,
        31,
        366,
        20088,
        5,
        53,
        12,
        24299,
        2024,
        2024,
        1,
        1735603506,
        0,
    ];
    assert_eq!(extract(1_735_603_506_789), late.map(Value::from));
    // 0000-02-03T13:00:00.001Z
    let year_0: [i64; 30] = [
        1000000,
        46800001000000,
        1000,
        46800001000,
        1,
        46800001,
        0,
        46800,
        0,
        780,
        1,
        1,
        13,
        13,
        1,
        4,
        3,
        6,
        3,
        34,
        -719495,
        1,
        5,
        2,
        1,
        1,
        0,
        0,
        -62164321200,
        0,
    ];
    assert_eq!(extract(-62_164_321_199_999), year_0.map(Value::from));
    assert_eq!(
        run(
            &indices(),
            "ROW d = TO_DATETIME(0) | EVAL a = DATE_EXTRACT(\"bogus\", d)"
        ),
        Err(EsqlError::Failed {
            status: 400,
            kind: "invalid_argument_exception",
            reason: "invalid date field for [DATE_EXTRACT(\"bogus\", d)]: bogus".into()
        })
    );
    assert_eq!(
        run(
            &indices(),
            "ROW d = TO_DATETIME(0) | EVAL a = DATE_EXTRACT(\"bogus\", d) | KEEP nope"
        ),
        Err(EsqlError::Verification(
            "Found 1 problem\nline 1:67: Unknown column [nope]".into()
        ))
    );
    let r = q("FROM people | EVAL a = DATE_EXTRACT(name, TO_DATETIME(0)) | KEEP a | LIMIT 1");
    assert_eq!(r.values, [[Value::Null]]);
}

/// Like Elasticsearch 9.5.5's `DECIMAL_LITERAL`, a number may have an exponent; a number too
/// large for a double is a parse error.
#[test]
fn esql_es_semantics_decimal_exponents() {
    let r = q(
        "ROW a = 1e5, b = 1.5E-3, c = .5e2, d = 1.e2, e = 2e+3, f = -1e2 \
               | EVAL g = a * 1e-5",
    );
    let types: Vec<&str> = r.columns.iter().map(|c| c.r#type.as_str()).collect();
    assert_eq!(types, ["double"; 7]);
    assert_eq!(
        r.values,
        [[100000.0, 0.0015, 50.0, 100.0, 2000.0, -100.0, 1.0].map(|n| json!(n))]
    );
    let err = |q: &str| match run(&indices(), q) {
        Err(EsqlError::Parse(m)) => m,
        other => panic!("{q}: {other:?}"),
    };
    assert_eq!(
        err("ROW a = 1e400"),
        "line 1:9: Number [1e400] is too large"
    );
    let big = format!("1{}.0", "0".repeat(400));
    assert_eq!(
        err(&format!("ROW a = {big}")),
        format!("line 1:9: Number [{big}] is too large")
    );
    assert_eq!(err("ROW a = 1e"), "line 1:9: Unexpected temporal unit: 'e'");
    assert_eq!(
        err("ROW a = 1e1days"),
        "line 1:12: extraneous input 'days' expecting <EOF>"
    );
    assert_eq!(
        err("ROW a = 1 | LIMIT 1e1"),
        "line 1:13: value of [LIMIT 1e1] must be a non negative integer, found value [1e1] \
         type [double]"
    );
}

#[test]
fn esql_conversions_date_math_adds_and_subtracts_time_spans() {
    let r = q(
        r#"ROW d = TO_DATETIME("2024-01-31T10:00:00Z") | EVAL a = d + 1 month, b = d - 1 year, c = 2 hours + d, w = d - 1 week, q = d + 1 quarter, s = d + 90 seconds, n = d + null, o = d + 9223372036854775807 ms, y = d + 2000000000 years | DROP d"#,
    );
    assert!(
        r.columns.iter().all(|c| c.r#type == "date"),
        "{:?}",
        r.columns
    );
    assert_eq!(
        r.values,
        [[
            json!("2024-02-29T10:00:00.000Z"),
            json!("2023-01-31T10:00:00.000Z"),
            json!("2024-01-31T12:00:00.000Z"),
            json!("2024-01-24T10:00:00.000Z"),
            json!("2024-04-30T10:00:00.000Z"),
            json!("2024-01-31T10:01:30.000Z"),
            Value::Null,
            Value::Null,
            Value::Null
        ]]
    );
    for (query, problem) in [
        (
            "ROW d = TO_DATETIME(0) | EVAL x = 1 day - d",
            "line 1:35: [-] arguments are in unsupported order: cannot subtract a [DATETIME] value \
             [d] from a [DATE_PERIOD] amount [1 day]",
        ),
        (
            "ROW d = TO_DATETIME(0) | EVAL x = d + 1",
            "line 1:35: [+] has arguments with incompatible types [datetime] and [integer]",
        ),
        (
            "ROW d = TO_DATETIME(0) | EVAL x = d + d",
            "line 1:35: [+] has arguments with incompatible types [datetime] and [datetime]",
        ),
        (
            "ROW d = TO_DATETIME(0) | EVAL x = d + (1 hour + 1 day)",
            "line 1:40: [+] has arguments with incompatible types [time_duration] and [date_period]",
        ),
    ] {
        match run(&indices(), query) {
            Err(EsqlError::Verification(m)) => assert_eq!(m, format!("Found 1 problem\n{problem}")),
            other => panic!("{query}: {other:?}"),
        }
    }
}

#[test]
fn esql_conversions_count_distinct_hashes_ips_and_dates_as_values() {
    let mut i = Index::default();
    for (f, t) in [("ip", "ip"), ("n", "date_nanos")] {
        let m: FieldMapping = serde_json::from_value(json!({ "type": t })).unwrap();
        i.mappings.properties.insert(f.into(), m);
    }
    i.put("1", json!({"ip": "::1", "n": "2024-01-01T00:00:00Z"}));
    i.put(
        "2",
        json!({"ip": "0:0:0:0:0:0:0:1", "n": "2024-01-01T00:00:00.000000000Z"}),
    );
    i.put(
        "3",
        json!({"ip": "::ffff:10.0.0.1", "n": "2024-01-01T00:00:00.5Z"}),
    );
    i.put("4", json!({"ip": "10.0.0.1"}));
    let indices = BTreeMap::from([("t".to_owned(), i)]);
    let r = run(
        &indices,
        "FROM t | STATS i = COUNT_DISTINCT(ip), n = COUNT_DISTINCT(n)",
    )
    .unwrap();
    assert_eq!(r.values, [[json!(2), json!(2)]]);
}

#[test]
fn esql_conversions_convert_dates_ips_and_numbers_to_dates() {
    let mut i = Index::default();
    for (f, t) in [("d", "date"), ("n", "date_nanos"), ("ip", "ip")] {
        let m: FieldMapping = serde_json::from_value(json!({ "type": t })).unwrap();
        i.mappings.properties.insert(f.into(), m);
    }
    i.put(
        "1",
        json!({"d": "2024-01-02T03:04:05.678Z", "n": "2024-01-02T03:04:05.123456700Z",
               "ip": "::ffff:10.0.0.1"}),
    );
    let indices = BTreeMap::from([("t".to_owned(), i)]);
    let r = run(
        &indices,
        "FROM t | EVAL a = TO_STRING(d), b = TO_LONG(d), c = TO_DOUBLE(d), e = TO_INTEGER(d), \
         f = TO_STRING(n), g = TO_LONG(n), h = TO_DATETIME(n), j = TO_STRING(ip), \
         k = TO_DATETIME(1.5), l = TO_DATETIME(18446744073709551615), m = TO_DATETIME(d) \
         | DROP d, n, ip",
    )
    .unwrap();
    assert_eq!(
        r.values,
        [[
            json!("2024-01-02T03:04:05.678Z"),
            json!(1_704_164_645_678i64),
            json!(1_704_164_645_678.0),
            Value::Null,
            json!("2024-01-02T03:04:05.1234567Z"),
            json!(1_704_164_645_123_456_700i64),
            json!("2024-01-02T03:04:05.123Z"),
            json!("10.0.0.1"),
            json!("1970-01-01T00:00:00.002Z"),
            Value::Null,
            json!("2024-01-02T03:04:05.678Z"),
        ]]
    );
    let types: Vec<&str> = r.columns.iter().map(|c| c.r#type.as_str()).collect();
    assert_eq!(types[..4], ["keyword", "long", "double", "integer"]);
    for (query, problem) in [
        (
            "ROW a = TO_DATETIME(true)",
            "line 1:9: argument of [TO_DATETIME(true)] must be [date_nanos or datetime or \
             numeric or string], found value [true] type [boolean]",
        ),
        (
            "FROM t | EVAL a = TO_BOOLEAN(d)",
            "line 1:19: argument of [TO_BOOLEAN(d)] must be [boolean or numeric or string], \
             found value [d] type [datetime]",
        ),
        (
            "FROM t | EVAL a = TO_INTEGER(n)",
            "line 1:19: argument of [TO_INTEGER(n)] must be [boolean or counter_integer or \
             datetime or numeric or string], found value [n] type [date_nanos]",
        ),
        (
            "FROM t | EVAL a = TO_DOUBLE(ip)",
            "line 1:19: argument of [TO_DOUBLE(ip)] must be [boolean or counter_double or \
             counter_integer or counter_long or datetime or numeric or string], found value \
             [ip] type [ip]",
        ),
        (
            "FROM t | EVAL a = TO_LONG(ip)",
            "line 1:19: argument of [TO_LONG(ip)] must be [boolean or counter_integer or \
             counter_long or date_nanos or datetime or geohash or geohex or geotile or numeric \
             or string], found value [ip] type [ip]",
        ),
    ] {
        match run(&indices, query) {
            Err(EsqlError::Verification(m)) => assert_eq!(m, format!("Found 1 problem\n{problem}")),
            other => panic!("{query}: {other:?}"),
        }
    }
}

#[test]
fn esql_conversions_compare_dates_ips_and_booleans_with_string_literals() {
    let mut i = Index::default();
    for (f, t) in [("d", "date"), ("n", "date_nanos"), ("ip", "ip")] {
        let m: FieldMapping = serde_json::from_value(json!({ "type": t })).unwrap();
        i.mappings.properties.insert(f.into(), m);
    }
    i.put(
        "1",
        json!({"d": "2024-01-02T03:04:05.678Z", "n": "2024-01-02T03:04:05.123456700Z",
               "ip": "::ffff:10.0.0.1"}),
    );
    let indices = BTreeMap::from([("t".to_owned(), i)]);
    let r = run(
        &indices,
        "FROM t | EVAL a = d == \"2024-01-02T03:04:05.678Z\", b = d > \"2024-01-02\", \
         c = \"2025\" > d, e = ip == \"10.0.0.1\", f = ip < \"10.0.0.2\", \
         g = n > \"2024-01-02T03:04:05.1234Z\", h = n < d, j = TRUE == \"True\" \
         | DROP d, n, ip",
    )
    .unwrap();
    assert_eq!(r.values, [vec![json!(true); 8]]);
    for (query, problem) in [
        (
            "FROM t | EVAL a = d > \"foo\"",
            "line 1:23: Cannot convert string [foo] to [DATETIME], error [failed to parse date \
             field [foo] with format [strict_date_optional_time]]",
        ),
        (
            "FROM t | EVAL a = \"x\" == ip",
            "line 1:19: Cannot convert string [x] to [IP], error ['x' is not an IP string \
             literal.]",
        ),
        (
            "FROM t | EVAL a = d == TO_STRING(d)",
            "line 1:19: first argument of [d == TO_STRING(d)] is [datetime] so second argument \
             must also be [datetime] but was [keyword]",
        ),
        (
            "FROM t | EVAL a = ip > d",
            "line 1:19: first argument of [ip > d] is [ip] so second argument must also be [ip] \
             but was [datetime]",
        ),
    ] {
        match run(&indices, query) {
            Err(EsqlError::Verification(m)) => assert_eq!(m, format!("Found 1 problem\n{problem}")),
            other => panic!("{query}: {other:?}"),
        }
    }
}

#[test]
fn esql_conversions_to_ip_reads_strings() {
    let r = q(
        r#"ROW a = TO_IP("::ffff:10.0.0.1"), b = TO_IP("1.1.010.1"), c = TO_IP(["::1", "x"]), d = TO_IP("10.0.0.1") == "::ffff:10.0.0.1""#,
    );
    let types: Vec<&str> = r.columns.iter().map(|c| c.r#type.as_str()).collect();
    assert_eq!(types, ["ip", "ip", "ip", "boolean"]);
    assert_eq!(
        r.values,
        [[json!("10.0.0.1"), Value::Null, json!("::1"), json!(true)]]
    );
    match run(&indices(), "ROW a = TO_IP(1)") {
        Err(EsqlError::Verification(m)) => assert_eq!(
            m,
            "Found 1 problem\nline 1:9: argument of [TO_IP(1)] must be [ip or string], found \
             value [1] type [integer]"
        ),
        other => panic!("{other:?}"),
    }
}
