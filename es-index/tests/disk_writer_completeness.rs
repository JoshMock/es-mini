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

#[test]
fn disk_writer_completeness_writes_date_nanos_as_epoch_nanos() {
    use es_codec::doc_values::DocValues;
    let mut idx = Index::default();
    let nanos = |format: Option<&str>| FieldMapping::DateNanos {
        format: format.map(Into::into),
        locale: None,
    };
    let props = &mut idx.mappings.properties;
    props.insert("dn".into(), nanos(None));
    props.insert("ms".into(), nanos(Some("epoch_millis")));
    idx.put(
        "1",
        json!({"dn": "2024-01-01T00:00:00.123456789Z", "ms": 1.5}),
    );
    idx.put("2", json!({"dn": "1970-01-01T01:00:01+01:00", "ms": "2"}));
    let (_root, r) = saved_segment("nanos", idx);
    let nanos = |f: &str| {
        let Ok(DocValues::Numbers(n)) = r.doc_values(f) else {
            panic!("date_nanos are sorted numerics");
        };
        n
    };
    let jan_2024 = 1_704_067_200_000_000_000;
    assert_eq!(
        nanos("dn"),
        [vec![jan_2024 + 123_456_789], vec![1_000_000_000]]
    );
    assert_eq!(nanos("ms"), [vec![1_500_000], vec![2_000_000]]);
    assert_eq!(r.field("dn").unwrap().point_num_bytes, 8);
}

#[test]
fn disk_writer_completeness_lists_ignore_above_fields_in_ignored() {
    use es_codec::doc_values::DocValues;
    let mut idx = Index::default();
    let keyword = || FieldMapping::Keyword {
        ignore_above: Some(3),
        split_queries_on_whitespace: false,
    };
    let props = &mut idx.mappings.properties;
    props.insert("a".into(), keyword());
    let s = FieldMapping::TextKeyword {
        fields: [("keyword".to_owned(), keyword())].into(),
    };
    props.insert("s".into(), s);
    idx.put("1", json!({"a": ["abc", "abcd"], "s": "long text"}));
    idx.put("2", json!({"a": "ab", "s": "x"}));
    idx.put("3", json!({"a": "abcde"}));
    let (_root, r) = saved_segment("ignored", idx);
    // Like IgnoredFieldMapper: a docs-only term and a sorted-set doc value per ignored field.
    let terms: Vec<_> = (r.terms("_ignored").unwrap().into_iter())
        .map(|t| (String::from_utf8(t.term).unwrap(), t.docs))
        .collect();
    let want = [
        ("a".to_owned(), vec![(0, 1), (2, 1)]),
        ("s.keyword".to_owned(), vec![(0, 1)]),
    ];
    assert_eq!(terms, want);
    let DocValues::Ordinals { ords, .. } = r.doc_values("_ignored").unwrap() else {
        panic!("_ignored doc values are sorted sets");
    };
    assert_eq!(ords, [vec![0, 1], vec![], vec![0]]);
}

/// Every field Elasticsearch indexes in a default index is in Lucene104 postings, which es-mini
/// reads for scoring statistics, except `_id` (ES812Postings), which no query scores.
#[test]
fn disk_writer_completeness_reads_stats_of_every_field_but_id_from_elasticsearch() {
    use es_codec::field_infos::IndexOptions;
    use std::collections::BTreeSet;
    let es_dir =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../fixtures/es-9.5.5/data");
    let indices = disk::open(&es_dir).unwrap();
    let mut checked = 0;
    for meta in es_codec::cluster_state::read_indices(&es_dir).unwrap() {
        let shard = es_dir.join("indices").join(&meta.uuid).join("0/index");
        let mut indexed = BTreeSet::new();
        for seg in SegmentInfos::read_latest(&shard).unwrap().segments {
            let r = SegmentReader::open(&shard, seg).unwrap();
            let fields = r
                .fields
                .iter()
                .filter(|f| f.index_options != IndexOptions::None);
            indexed.extend(fields.map(|f| f.name.clone()));
        }
        assert!(indexed.remove("_id"), "{}", meta.name);
        let stats = indices[&meta.name].shards()[0].stats.as_ref().unwrap();
        let with_stats: BTreeSet<_> = stats.fields.keys().cloned().collect();
        assert_eq!(with_stats, indexed, "{}", meta.name);
        checked += indexed.len();
    }
    assert!(checked > 0);
}

#[test]
fn disk_writer_completeness_writes_ranges_as_points_and_binary_doc_values() {
    use es_codec::{doc_values::DocValues, field_infos::DocValuesType};
    let mut idx = Index::default();
    let props = &mut idx.mappings.properties;
    props.insert("l".into(), FieldMapping::LongRange);
    props.insert("d".into(), FieldMapping::DateRange { format: None });
    // gt/lt exclude their bound, an open end is the type's limit, and the doc values hold each
    // doc's distinct ranges sorted, as RangeFieldMapper indexes them
    idx.put(
        "1",
        json!({"l": [{"gte": 1, "lt": 5}, {"gt": -2, "lte": -1}, {"gte": 1, "lt": 5}]}),
    );
    idx.put(
        "2",
        json!({"l": {"gte": null, "lte": 7}, "d": {"gt": "2024-01-01", "lte": "2024-01-02"}}),
    );
    idx.put("3", json!({"x": 1}));
    let (_root, r) = saved_segment("ranges", idx);
    for f in ["l", "d"] {
        let info = r.field(f).unwrap();
        assert_eq!((info.point_dimensions, info.point_index_dimensions), (2, 2));
        assert_eq!(info.point_num_bytes, 8);
        assert_eq!(info.doc_values, DocValuesType::Binary);
    }
    let DocValues::Bytes(l) = r.doc_values("l").unwrap() else {
        panic!("range doc values are binary");
    };
    // vint count, then BinaryRangeUtil.encodeLong of each from and to: -1 is 0x7f, 1 is 0x81
    let min = [0x3f, 0x80, 0, 0, 0, 0, 0, 0, 0];
    assert_eq!(l[0].as_deref(), Some(&[2, 0x7f, 0x7f, 0x81, 0x84][..]));
    assert_eq!(
        l[1].as_deref(),
        Some(&[[1].as_slice(), &min, &[0x87]].concat()[..])
    );
    assert_eq!(l[2], None);
    let DocValues::Bytes(d) = r.doc_values("d").unwrap() else {
        panic!("range doc values are binary");
    };
    // gt and lte round their dates up to the day's last millisecond (gt then adds one)
    let encode = |n: i64| {
        let mut b = n.to_be_bytes()[2..].to_vec();
        b[0] |= 0x80 | 5 << 3;
        b
    };
    let want = [
        vec![1],
        encode(1_704_153_600_000),
        encode(1_704_239_999_999),
    ]
    .concat();
    assert_eq!(d[1].as_deref(), Some(&want[..]));
    assert_eq!((d[0].as_ref(), d[2].as_ref()), (None, None));
}

fn nested_index() -> Index {
    let mut idx = Index::default();
    let props = &mut idx.mappings.properties;
    let keyword = || FieldMapping::Keyword {
        ignore_above: None,
        split_queries_on_whitespace: false,
    };
    props.insert("t".into(), keyword());
    props.insert("c".into(), FieldMapping::Nested { dynamic: None });
    props.insert("c.a".into(), keyword());
    props.insert("c.r".into(), FieldMapping::Nested { dynamic: None });
    props.insert("c.r.n".into(), FieldMapping::Long);
    idx.put(
        "1",
        json!({"t": "x", "c": [{"a": "p", "r": [{"n": 1}]}, {"a": "q"}]}),
    );
    idx.put("2", json!({"t": "y"}));
    idx
}

/// Like DocumentParser: each nested object is a Lucene doc before the one holding it, with the
/// root's `_id`, `_version` and `_seq_no` and its `_nested_path`; only roots have stored fields
/// and `_primary_term` (which tells Elasticsearch's nested queries the roots apart).
#[test]
fn disk_writer_completeness_writes_nested_objects_as_child_documents() {
    use es_codec::doc_values::DocValues;
    let (_root, r) = saved_segment("nested", nested_index());
    assert_eq!(r.max_doc(), 5);
    let terms = |f: &str| -> Vec<_> {
        (r.terms(f).unwrap().into_iter())
            .map(|t| (t.term, t.docs.iter().map(|d| d.0).collect::<Vec<_>>()))
            .collect()
    };
    let s = |s: &str| s.as_bytes().to_vec();
    assert_eq!(
        terms("_nested_path"),
        [(s("c"), vec![1, 2]), (s("c.r"), vec![0])]
    );
    assert_eq!(terms("c.a"), [(s("p"), vec![1]), (s("q"), vec![2])]);
    assert_eq!(terms("t"), [(s("x"), vec![3]), (s("y"), vec![4])]);
    let id = |i: &str| es_codec::uid::encode_id(i);
    assert_eq!(
        terms("_id"),
        [(id("1"), vec![0, 1, 2, 3]), (id("2"), vec![4])]
    );
    let numbers = |f: &str| match r.doc_values(f).unwrap() {
        DocValues::Numbers(n) => n,
        _ => panic!("{f} has numeric doc values"),
    };
    assert_eq!(numbers("c.r.n"), [vec![1], vec![], vec![], vec![], vec![]]);
    assert_eq!(
        numbers("_primary_term"),
        [vec![], vec![], vec![], vec![1], vec![1]]
    );
    assert_eq!(
        numbers("_seq_no"),
        [vec![0], vec![0], vec![0], vec![0], vec![1]]
    );
    assert!(r.document(0).unwrap().is_empty());
    assert_eq!(r.document(3).unwrap().len(), 2);
}

/// es-mini reads the roots back as its documents; nested fields, whose postings and doc values
/// are on the children, fall back to `_source`; a replaced document's children die with it.
#[test]
fn disk_writer_completeness_reads_back_segments_with_nested_documents() {
    let root = Scratch::new("completeness-nested-reopen", None);
    let dir = root.join("data");
    disk::save(&dir, &Indices::from([("idx".to_owned(), nested_index())])).unwrap();
    let mut indices = disk::open(&dir).unwrap();
    let (ix, shard) = (&indices["idx"], &indices["idx"].shards()[0]);
    let mut live: Vec<_> = shard
        .disk_live()
        .map(|(p, (id, _))| (id.to_owned(), p))
        .collect();
    live.sort();
    assert_eq!(live, [("1".to_owned(), (0, 3)), ("2".to_owned(), (0, 4))]);
    assert_eq!(shard.disk_term("c.a", "p", true), None);
    assert_eq!(
        shard.disk_numbers(&ix.mappings, "c.r.n", |_| std::cmp::Ordering::Equal, true),
        None
    );
    assert_eq!(shard.disk_values(&ix.mappings, "c.r.n")("1"), None);
    assert_eq!(shard.disk_term("t", "x", true), Some([(0, 3)].into()));
    assert_eq!(
        shard.disk_values(&ix.mappings, "t")("2"),
        Some(vec![json!("y")])
    );
    assert_eq!(ix.get("1").unwrap().source["c"][1]["a"], "q");
    // Enough other docs that the deletes stay under the 20% that makes a merge drop the segment
    let ix = indices.get_mut("idx").unwrap();
    for i in 3..20 {
        ix.put(&i.to_string(), json!({}));
    }
    disk::save(&dir, &indices).unwrap();
    indices.get_mut("idx").unwrap().put("1", json!({"t": "z"}));
    disk::save(&dir, &indices).unwrap();
    let ix = &disk::open(&dir).unwrap()["idx"];
    let uuid = &es_codec::cluster_state::read_indices(&dir).unwrap()[0].uuid;
    let shard = dir.join("indices").join(uuid).join("0/index");
    let first = SegmentInfos::read_latest(&shard)
        .unwrap()
        .segments
        .remove(0);
    let live = SegmentReader::open(&shard, first)
        .unwrap()
        .live_docs()
        .unwrap();
    assert_eq!(live, [false, false, false, false, true]);
    assert_eq!(ix.get("1").unwrap().source, json!({"t": "z"}));
}

#[test]
fn ip_and_range_fields_writes_every_range_type_as_elasticsearch_does() {
    use es_codec::{doc_values::DocValues, field_infos::DocValuesType};
    let mut idx = Index::default();
    idx.mappings = serde_json::from_value(json!({"properties": {
        "i": {"type": "integer_range"}, "f": {"type": "float_range"},
        "x": {"type": "double_range"}, "ip": {"type": "ip_range"},
    }}))
    .unwrap();
    idx.put(
        "1",
        json!({"i": {"gte": -1, "lt": 5}, "f": {"gt": 1.0}, "x": {"lte": -2.0}, "ip": "10.0.0.0/30"}),
    );
    let (_root, r) = saved_segment("more-ranges", idx);
    for (f, width) in [("i", 4), ("f", 4), ("x", 8), ("ip", 16)] {
        let info = r.field(f).unwrap();
        assert_eq!(
            (info.point_dimensions, info.point_num_bytes),
            (2, width),
            "{f}"
        );
        assert_eq!(info.doc_values, DocValuesType::Binary);
    }
    let points = |f: &str| r.points(f).unwrap()[0].1.clone();
    // IntRange: the sortable ints of 4 (lt 5) and -1
    assert_eq!(points("i"), [0x7f, 0xff, 0xff, 0xff, 0x80, 0, 0, 4]);
    // FloatRange: Math.nextUp(1.0f) up to +Infinity
    assert_eq!(points("f"), [0xbf, 0x80, 0, 1, 0xff, 0x80, 0, 0]);
    // InetAddressRange: the block's first and last address, IPv4-mapped
    let v4 = |last| [[0; 10].as_slice(), &[0xff, 0xff, 10, 0, 0, last]].concat();
    assert_eq!(points("ip"), [v4(0), v4(3)].concat());
    let bytes = |f: &str| match r.doc_values(f).unwrap() {
        DocValues::Bytes(b) => b[0].clone().unwrap(),
        _ => panic!("range doc values are binary"),
    };
    // integers encode like longs (encodeLong of -1 and 4)
    assert_eq!(bytes("i"), [1, 0x7f, 0x84]);
    assert_eq!(bytes("f"), [[1].as_slice(), &points("f")].concat());
    // -Infinity up to -2.0, as sortable longs
    let want = [
        [1].as_slice(),
        &[0, 0x0f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
        &[0x3f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
    ]
    .concat();
    assert_eq!(bytes("x"), want);
    assert_eq!(bytes("ip"), [[1].as_slice(), &v4(0), &v4(3)].concat());
}

/// Like DotExpandingXContentParser: a dotted key crossing into a nested path (`c.a` with `c`
/// nested) is expanded to objects, so it becomes a nested document of its own, in source order.
#[test]
fn disk_fidelity_dotted_keys_into_nested_paths_become_nested_documents() {
    let mut idx = Index::default();
    idx.mappings = nested_index().mappings;
    idx.put(
        "1",
        json!({"c.a": "p", "c": {"a": "q"}, "c.r.n": 5, "t": "x"}),
    );
    let (_root, r) = saved_segment("dotted-nested", idx);
    let terms = |f: &str| -> Vec<_> {
        (r.terms(f).unwrap().into_iter())
            .map(|t| (t.term, t.docs.iter().map(|d| d.0).collect::<Vec<_>>()))
            .collect()
    };
    let s = |s: &str| s.as_bytes().to_vec();
    // Docs: c {a: p}, c {a: q}, c.r {n: 5}, c {r.n: 5}, the root.
    assert_eq!(r.max_doc(), 5);
    assert_eq!(
        terms("_nested_path"),
        [(s("c"), vec![0, 1, 3]), (s("c.r"), vec![2])]
    );
    assert_eq!(terms("c.a"), [(s("p"), vec![0]), (s("q"), vec![1])]);
    assert_eq!(terms("t"), [(s("x"), vec![4])]);
}
