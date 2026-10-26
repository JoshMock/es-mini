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
        EsqlError::Verification("Unknown column [zzz]".into())
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
    assert!(matches!(
        run(&all, "FROM nope*"),
        Err(EsqlError::Verification(_))
    ));
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
    for q in [
        "FROM people, b, c | WHERE age > 1",
        "FROM people, b, c | SORT age",
    ] {
        assert_eq!(
            run(&all, q).unwrap_err(),
            EsqlError::Verification(
                "Cannot use field [age] due to ambiguities being mapped as [3] incompatible types: \
                 [datetime] in [c], [keyword] in [b], [long] in [people]"
                    .into()
            ),
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
