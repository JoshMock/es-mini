//! Multi-valued and sparse doc values with more than 1024 terms, from an index written by real
//! Lucene 10.5.1 (review finding D12): see tests/gen/GenWideDocValues.java, whose `kw` and `num`
//! these mirror.

use es_codec::{doc_values::DocValues, field_infos::DocValuesType};

mod common;

const DOCS: usize = 3000;

/// A doc's distinct terms in byte order, none for every third doc.
fn kw(d: usize) -> Vec<String> {
    if d.is_multiple_of(3) {
        return vec![];
    }
    let mut out: Vec<String> = (0..1 + d % 3)
        .map(|i| format!("term-{:04}", (d * 7 + i * 613) % 2000))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// A doc's values in ascending order, none for every fifth doc.
fn num(d: usize) -> Vec<i64> {
    let d64 = d as i64;
    let mut out = match d {
        _ if d.is_multiple_of(5) => vec![],
        _ if d.is_multiple_of(2) => vec![d64 * 1_000_003 - 1_500_000_000],
        _ => vec![-d64, d64 << 33],
    };
    out.sort();
    out
}

#[test]
fn reads_multi_valued_sparse_sorted_set_with_many_terms() {
    let seg = common::open("wide-doc-values");
    assert_eq!(
        seg.field("kw").unwrap().doc_values,
        DocValuesType::SortedSet
    );
    let DocValues::Ordinals { ords, terms } = seg.doc_values("kw").unwrap() else {
        panic!("kw is not sorted set")
    };
    assert!(terms.len() > 1024, "only {} terms", terms.len());
    let got: Vec<Vec<String>> = ords
        .iter()
        .map(|d| {
            d.iter()
                .map(|&o| String::from_utf8(terms[o].clone()).unwrap())
                .collect()
        })
        .collect();
    let want: Vec<_> = (0..DOCS).map(kw).collect();
    assert_eq!(got, want);
}

#[test]
fn reads_multi_valued_sparse_sorted_numeric() {
    let seg = common::open("wide-doc-values");
    assert_eq!(
        seg.field("num").unwrap().doc_values,
        DocValuesType::SortedNumeric
    );
    let DocValues::Numbers(got) = seg.doc_values("num").unwrap() else {
        panic!("num is not numeric")
    };
    let want: Vec<_> = (0..DOCS).map(num).collect();
    assert_eq!(got, want);
}
