use es_index::Index;
use es_query::esql::{EsqlError, run};
use serde_json::json;
use std::collections::BTreeMap;

fn indices() -> BTreeMap<String, Index> {
    let doc = |id: &str| {
        let mut i = Index::default();
        i.put(id, json!({ "n": id }));
        i
    };
    BTreeMap::from([("a1".into(), doc("x1")), ("a2".into(), doc("x2"))])
}

fn ids(query: &str) -> Result<Vec<String>, EsqlError> {
    let r = run(&indices(), query)?;
    Ok(r.values
        .iter()
        .map(|v| v[0].as_str().unwrap().into())
        .collect())
}

#[test]
fn from_exclusions_exclude_indices() {
    assert_eq!(ids("FROM a*,-a1").unwrap(), ["x2"]);
}

#[test]
fn from_exclusion_matching_nothing_keeps_everything() {
    assert_eq!(ids("FROM a*,-zzz").unwrap().len(), 2);
}

#[test]
fn from_exclusions_leaving_nothing_are_unknown_index_errors() {
    // Messages recorded from Elasticsearch 9.5.5.
    let err = |q| run(&indices(), q).unwrap_err();
    let unknown = |e: &str| EsqlError::Verification(format!("Unknown index [{e}]"));
    assert_eq!(err("FROM -a1"), unknown("-a1"));
    assert_eq!(err("FROM a*,-a1,-a2"), unknown("a*,-a1,-a2"));
    assert_eq!(err("FROM a1,-a1"), unknown("a1,-a1"));
}
