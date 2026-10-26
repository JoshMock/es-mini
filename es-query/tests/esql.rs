use es_index::{FieldMapping, Index};
use es_query::esql::{EsqlError, Response, run};
use serde_json::{Value, json};
use std::collections::BTreeMap;

fn indices() -> BTreeMap<String, Index> {
    let mut i = Index::default();
    i.mappings
        .properties
        .insert("name".into(), FieldMapping::Keyword { ignore_above: None });
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
    more.mappings
        .properties
        .insert("name".into(), FieldMapping::Keyword { ignore_above: None });
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
fn text_with_keyword_subfield_keeps_both_columns() {
    let mut i = Index::default();
    i.map_dynamic(&json!({"t": "Hello"}));
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
    b.mappings
        .properties
        .insert("age".into(), FieldMapping::Keyword { ignore_above: None });
    b.mappings
        .properties
        .insert("name".into(), FieldMapping::Keyword { ignore_above: None });
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
    ] {
        let r = run(&all, query).unwrap();
        assert_eq!(
            (r.documents_found, r.values_loaded),
            (docs, values),
            "{query}"
        );
    }
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
    match run(&indices(), "FROM people METADATA _foo") {
        Err(EsqlError::Parse(m)) => assert_eq!(m, "line 1:22: unsupported metadata field [_foo]"),
        other => panic!("{other:?}"),
    }
}
