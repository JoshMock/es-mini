//! Sorted and sorted set doc values from an index written by real Lucene 10.5.1: see
//! tests/gen/GenSortedDocValues.java (the terms there must match `terms` here).

use es_codec::{doc_values::DocValues, field_infos::DocValuesType};

mod common;

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
    let seg = common::open("sorted-doc-values");
    for (field, kind) in [
        ("dense", DocValuesType::Sorted),
        ("sparse", DocValuesType::Sorted),
        ("multi", DocValuesType::SortedSet),
    ] {
        assert_eq!(seg.field(field).unwrap().doc_values, kind);
        let DocValues::Ordinals { ords, terms: dict } = seg.doc_values(field).unwrap() else {
            panic!("{field} is not sorted");
        };
        let got: Vec<Vec<String>> = ords
            .into_iter()
            .map(|d| {
                d.into_iter()
                    .map(|o| String::from_utf8(dict[o].clone()).unwrap())
                    .collect()
            })
            .collect();
        let want: Vec<_> = (0..DOCS).map(|d| terms(field, d)).collect();
        assert_eq!(got, want, "{field}");
    }
}
