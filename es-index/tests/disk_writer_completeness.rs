//! Saved segments hold every mapped field the way Elasticsearch writes it, so Elasticsearch
//! reading an es-mini data directory finds them.

use es_codec::{SegmentInfos, SegmentReader, stored_fields::StoredValue};
use es_index::{FieldMapping, Index, Indices, disk};
use serde_json::json;

mod common;
use common::Scratch;

/// Saves `idx` and opens the one segment it writes.
fn saved_segment(name: &str, idx: Index) -> (Scratch, SegmentReader) {
    let root = Scratch::new(&format!("completeness-{name}"), None);
    let dir = root.join("data");
    disk::save(&dir, &Indices::from([("idx".to_owned(), idx)])).unwrap();
    let uuid = &es_codec::cluster_state::read_indices(&dir).unwrap()[0].uuid;
    let shard = dir.join("indices").join(uuid).join("0/index");
    let seg = SegmentInfos::read_latest(&shard)
        .unwrap()
        .segments
        .remove(0);
    let r = SegmentReader::open(&shard, seg).unwrap();
    (root, r)
}

#[test]
fn disk_writer_completeness_stores_text_fields_mapped_with_store() {
    let mut idx = Index::default();
    let props = &mut idx.mappings.properties;
    props.insert("st".into(), FieldMapping::Text { store: true });
    props.insert("t".into(), FieldMapping::Text { store: false });
    idx.put(
        "1",
        json!({"st": ["Quick fox", "lazy dog"], "t": "not stored"}),
    );
    let (_root, r) = saved_segment("store", idx);
    let st = r.field("st").unwrap().number;
    let got: Vec<_> = (r.document(0).unwrap().into_iter())
        .filter(|(n, _)| *n == st)
        .map(|(_, v)| v)
        .collect();
    let want = ["Quick fox", "lazy dog"].map(|s| StoredValue::String(s.into()));
    assert_eq!(got, want);
    let t = r.field("t").unwrap().number;
    assert!(r.document(0).unwrap().iter().all(|(n, _)| *n != t));
}

#[test]
fn disk_writer_completeness_writes_multi_fields_and_honors_ignore_above() {
    use es_codec::doc_values::DocValues;
    let mut idx = Index::default();
    let keyword = FieldMapping::Keyword {
        ignore_above: Some(5),
        split_queries_on_whitespace: false,
    };
    let s = FieldMapping::TextKeyword {
        fields: [("keyword".to_owned(), keyword)].into(),
    };
    idx.mappings.properties.insert("s".into(), s);
    idx.put("1", json!({"s": ["short", "much too long"]}));
    idx.put("2", json!({"s": "abc"}));
    let (_root, r) = saved_segment("multi", idx);
    let terms: Vec<_> = (r.terms("s.keyword").unwrap().into_iter())
        .map(|t| (String::from_utf8(t.term).unwrap(), t.docs))
        .collect();
    let want = [
        ("abc".to_owned(), vec![(1, 1)]),
        ("short".to_owned(), vec![(0, 1)]),
    ];
    assert_eq!(terms, want);
    let DocValues::Ordinals { ords, .. } = r.doc_values("s.keyword").unwrap() else {
        panic!("keyword doc values are sorted sets");
    };
    assert_eq!(ords, [vec![1], vec![0]]);
    assert!(r.terms("s").is_ok());
}

#[test]
fn disk_writer_completeness_coerces_scalars_and_skips_malformed_values() {
    use es_codec::doc_values::DocValues;
    let mut idx = Index::default();
    let props = &mut idx.mappings.properties;
    let keyword = FieldMapping::Keyword {
        ignore_above: None,
        split_queries_on_whitespace: false,
    };
    props.insert("k".into(), keyword);
    props.insert("t".into(), FieldMapping::Text { store: false });
    props.insert("n".into(), FieldMapping::Long);
    props.insert("b".into(), FieldMapping::Boolean);
    props.insert("i".into(), FieldMapping::Integer);
    // Elasticsearch indexes numbers and booleans as strings, and strings holding numbers or
    // booleans as those; it rejects documents with malformed values, which es-mini keeps but
    // never matches, so the segment leaves those values out.
    idx.put(
        "1",
        json!({"k": [42, true], "t": 7, "n": ["10", "abc", {"x": 1}], "b": "true", "i": 1.5}),
    );
    idx.put("2", json!({"k": "x", "n": 3, "b": false, "i": 2}));
    let (_root, r) = saved_segment("coerce", idx);
    let terms = |f: &str| -> Vec<_> {
        (r.terms(f).unwrap().into_iter())
            .map(|t| (String::from_utf8(t.term).unwrap(), t.docs))
            .collect()
    };
    let one = |d| vec![(d, 1)];
    let want = [("42", one(0)), ("true", one(0)), ("x", one(1))];
    assert_eq!(terms("k"), want.map(|(t, d)| (t.to_owned(), d)));
    assert_eq!(terms("t"), [("7".to_owned(), one(0))]);
    assert_eq!(
        terms("b"),
        [("F".to_owned(), one(1)), ("T".to_owned(), one(0))]
    );
    let Ok(DocValues::Numbers(n)) = r.doc_values("n") else {
        panic!("longs are sorted numerics");
    };
    assert_eq!(n, [vec![10], vec![3]]);
    // An integer with a fraction would need truncating, as Elasticsearch does, where es-mini
    // matches the fraction, so the field stays out of the segment.
    assert!(r.field("i").is_err());
}
