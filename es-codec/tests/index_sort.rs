//! A segment written by real Lucene 10.5.1 with an index sort: see tests/gen/GenIndexSort.java
//! (its value formulas must match the ones here).

use es_codec::{SegmentInfos, segment_infos::SortField, stored_fields::StoredValue};
use std::{cmp::Ordering, path::PathBuf};

mod common;

const DOCS: i64 = 100;

fn tag(d: i64) -> Option<String> {
    (d % 10 != 0).then(|| format!("t{}", d % 3))
}

/// The larger of the doc's `ts` values (the MAX selector).
fn ts(d: i64) -> Option<i64> {
    (d % 7 != 0).then(|| d % 6 * 1000 + if d % 2 == 0 { 500 } else { 0 })
}

fn score(d: i64) -> Option<f32> {
    (d % 3 != 0).then_some((d % 9) as f32 * 0.5 - 1.0)
}

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data/index-sort")
}

#[test]
fn reads_the_index_sort_of_a_segment() {
    let infos = SegmentInfos::read_latest(&dir()).unwrap();
    assert_eq!(
        infos.segments[0].info.index_sort,
        [
            SortField::SortedSet {
                field: "tag".into(),
                reverse: false,
                selector: 0,
                missing: 1, // STRING_FIRST
            },
            SortField::SortedNumeric {
                field: "ts".into(),
                sort_type: "LONG".into(),
                reverse: true,
                selector: 1, // MAX
                missing: Some(i64::MIN),
            },
            SortField::Plain {
                field: "score".into(),
                sort_type: "FLOAT".into(),
                reverse: false,
                missing: Some(0.25f32.to_bits().into()), // floatToSortableInt of a positive float
            },
            SortField::Plain {
                field: "id".into(),
                sort_type: "LONG".into(),
                reverse: false,
                missing: None,
            },
        ]
    );
}

#[test]
fn reads_documents_of_a_sorted_segment_in_sort_order() {
    let mut expected: Vec<i64> = (0..DOCS).collect();
    expected.sort_by(|&a, &b| {
        tag(a)
            .cmp(&tag(b)) // None (missing first) sorts before any tag
            .then_with(|| ts(b).unwrap_or(i64::MIN).cmp(&ts(a).unwrap_or(i64::MIN)))
            .then_with(|| {
                let s = |d| score(d).unwrap_or(0.25);
                s(a).partial_cmp(&s(b)).unwrap_or(Ordering::Equal)
            })
            .then(a.cmp(&b))
    });
    let ids: Vec<i64> = common::open("index-sort")
        .stored_fields()
        .unwrap()
        .into_iter()
        .map(|doc| match doc[..] {
            [(_, StoredValue::Int(id))] => id.into(),
            _ => panic!("unexpected stored fields {doc:?}"),
        })
        .collect();
    assert_eq!(ids, expected);
}

#[test]
fn rewrites_the_index_sort_byte_for_byte() {
    let infos = SegmentInfos::read_latest(&dir()).unwrap();
    let out = std::env::temp_dir().join(format!("esm-index-sort-{}", std::process::id()));
    std::fs::create_dir_all(&out).unwrap();
    infos.write(&out).unwrap();
    let name = format!("{}.si", infos.segments[0].name);
    let (ours, real) = (
        std::fs::read(out.join(&name)).unwrap(),
        std::fs::read(dir().join(&name)).unwrap(),
    );
    std::fs::remove_dir_all(&out).unwrap();
    // Lucene writes the diagnostics and files in hash order, so compare from the sort field
    // count (the vint before the first provider name's length) to the footer.
    let sort_section = |si: &[u8]| {
        let at = si.windows(18).position(|w| w == b"SortedSetSortField");
        si[at.unwrap() - 2..si.len() - 16].to_vec()
    };
    assert_eq!(sort_section(&ours), sort_section(&real));
}
