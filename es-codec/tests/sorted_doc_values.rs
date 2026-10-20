//! Sorted and sorted set doc values from an index written by real Lucene 10.5.1: see
//! tests/gen/GenSortedDocValues.java (the terms there must match `terms` here).

use es_codec::{
    CompoundFile, SegmentInfos,
    doc_values::{DocValues, read_field_values},
    field_infos::{DocValuesType, read_field_infos},
};
use std::path::PathBuf;

const DOCS: usize = 300;

/// Terms of a doc in ordinal (byte) order, none when the doc has no value.
fn terms(field: &str, d: usize) -> Vec<String> {
    let mut out = match field {
        "dense" => vec![format!(
            "a-common-prefix-longer-than-fifteen-bytes-{:03}",
            d % 150
        )],
        "sparse" if d.is_multiple_of(4) => vec![format!("{}{}", "s".repeat(20 + d % 7), d % 3)],
        "multi" if !d.is_multiple_of(5) => {
            vec![format!("m{:03}", d % 100), format!("m{:03}", d * 7 % 100)]
        }
        _ => vec![],
    };
    out.sort();
    out.dedup();
    out
}

#[test]
fn reads_sorted_and_sorted_set_doc_values() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data/sorted-doc-values");
    let infos = SegmentInfos::read_latest(&dir).unwrap();
    let seg = &infos.segments[0];
    let cfs = CompoundFile::open(&dir, &seg.name, &seg.info.id).unwrap();
    let fields = read_field_infos(&dir, seg, &cfs).unwrap();
    for (field, kind) in [
        ("dense", DocValuesType::Sorted),
        ("sparse", DocValuesType::Sorted),
        ("multi", DocValuesType::SortedSet),
    ] {
        assert_eq!(
            fields.iter().find(|f| f.name == field).unwrap().doc_values,
            kind
        );
        let DocValues::Terms(got) = read_field_values(&dir, seg, &cfs, &fields, field).unwrap()
        else {
            panic!("{field} is not terms");
        };
        let got: Vec<Vec<String>> = got
            .into_iter()
            .map(|d| {
                d.into_iter()
                    .map(|t| String::from_utf8(t).unwrap())
                    .collect()
            })
            .collect();
        let want: Vec<_> = (0..DOCS).map(|d| terms(field, d)).collect();
        assert_eq!(got, want, "{field}");
    }
}
