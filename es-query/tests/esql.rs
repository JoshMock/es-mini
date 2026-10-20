use es_index::{FieldMapping, Index};
use es_query::esql::{EsqlError, Response, run};
use serde_json::{Value, json};
use std::collections::BTreeMap;

fn indices() -> BTreeMap<String, Index> {
    let mut i = Index::default();
    i.mappings
        .properties
        .insert("name".into(), FieldMapping::Keyword);
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
