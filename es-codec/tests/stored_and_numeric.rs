//! Stored fields of every type, wide numeric doc values, and hard deletes, from an index written
//! by real Lucene 10.5.1: see tests/gen/GenStoredAndNumeric.java (its value formulas must match
//! the ones here).

use es_codec::{
    doc_values::{DocValues, write_doc_values_with},
    stored_fields::StoredValue,
};

mod common;

const DOCS: usize = 70000;
const STORED_DOCS: usize = 3000;
const HUGE_DOC: usize = 1500;
const DAY: i64 = 86_400_000;

fn float(d: usize) -> f32 {
    match d % 4 {
        0 => -1.5 * d as f32,
        1 => (d % 100) as f32,
        2 => d as f32 + 0.25,
        _ => 0.0,
    }
}

fn double(d: usize) -> f64 {
    match d % 4 {
        0 => -(d as f64) * 1.125,
        1 => (d % 100) as f64,
        2 => d as f64 + 0.5,
        _ => d as f64 + 0.1,
    }
}

fn long(d: usize) -> i64 {
    let d = d as i64;
    match d % 5 {
        0 => d * DAY,
        1 => d * 3_600_000,
        2 => d * 1000,
        3 => -d * 7,
        _ => i64::MAX - d,
    }
}

fn stored(d: usize) -> Vec<(&'static str, StoredValue)> {
    if d >= STORED_DOCS {
        return vec![];
    }
    let string = if d == HUGE_DOC {
        "x".repeat(200_000)
    } else {
        format!("s{d}")
    };
    let int = if d.is_multiple_of(2) {
        d as i32
    } else {
        -(d as i32)
    };
    vec![
        ("int", StoredValue::Int(int)),
        ("float", StoredValue::Float(float(d))),
        ("long", StoredValue::Long(long(d))),
        ("double", StoredValue::Double(double(d))),
        ("string", StoredValue::String(string)),
        ("bytes", StoredValue::Bytes(vec![d as u8, (d >> 8) as u8])),
    ]
}

#[test]
fn reads_stored_fields_of_every_type() {
    let seg = common::open("stored-and-numeric");
    let name = |n: i32| seg.field_by_number(n).unwrap().name.as_str();
    let docs = seg.stored_fields().unwrap();
    assert_eq!(docs.len(), DOCS);
    for (d, doc) in docs.into_iter().enumerate() {
        let got: Vec<(&str, StoredValue)> = doc.into_iter().map(|(n, v)| (name(n), v)).collect();
        assert_eq!(got, stored(d), "doc {d}");
    }
}

#[test]
fn reads_hard_deletes_from_liv() {
    let seg = common::open("stored-and-numeric");
    assert_eq!((seg.seg.del_gen, seg.seg.del_count), (1, 70));
    let live = seg.live_docs().unwrap();
    let dead: Vec<usize> = (0..DOCS).filter(|d| !live[*d]).collect();
    let want: Vec<usize> = (0..DOCS).filter(|d| d % 1000 == 7).collect();
    assert_eq!(dead, want);
}

#[test]
fn reads_numeric_doc_values_across_disi_and_value_blocks() {
    let seg = common::open("stored-and-numeric");
    let numbers = |field: &str| match seg.doc_values(field).unwrap() {
        DocValues::Numbers(n) => n,
        other => panic!("{field}: {other:?}"),
    };
    let expect = |field: &str, value: &dyn Fn(i64) -> Vec<i64>| {
        let want: Vec<Vec<i64>> = (0..DOCS as i64).map(value).collect();
        assert_eq!(numbers(field), want, "{field}");
    };
    // Dense bit-set block (every other doc of 65536), then a sparse block.
    expect("half", &|d| if d % 2 == 0 { vec![d % 8] } else { vec![] });
    // A block holding every doc, then a sparse block.
    expect("head", &|d| {
        if d < 65536 || d % 9 == 0 {
            vec![1]
        } else {
            vec![]
        }
    });
    // 2 bits per value in the first 16384-value block, 12 in the second.
    expect("varying", &|d| match d {
        0..16384 => vec![d % 4],
        16384..32768 => vec![d % 4096],
        _ => vec![],
    });
    expect("multi", &|d| {
        let mut v = vec![d % 4];
        if d % 3 == 0 {
            v.push(-(d % 4));
        }
        v.sort();
        v
    });
}

/// Rewriting the segment's doc values gives Lucene's own `.dvm` and `.dvd`, byte for byte: the
/// same table, GCD and block encodings, and the same IndexedDISI rank tables.
#[test]
fn writes_numeric_doc_values_like_lucene() {
    let seg = common::open("stored-and-numeric");
    let field = |name: &str| match seg.doc_values(name).unwrap() {
        DocValues::Numbers(n) => (seg.field(name).unwrap().number, n),
        other => panic!("{name}: {other:?}"),
    };
    // Lucene's merge wrote the NUMERIC fields in this order.
    let numeric = ["head", "varying", "half"].map(field);
    let id = seg.seg.info.id;
    let (dvm, dvd) = write_doc_values_with(&numeric, &[field("multi")], &[], &id, "Lucene90_0");
    let lucene = |ext| seg.cfs.file(ext).unwrap().unwrap();
    assert_eq!(dvm, lucene("_Lucene90_0.dvm"));
    let want = lucene("_Lucene90_0.dvd");
    assert_eq!(dvd.len(), want.len());
    assert!(dvd == want, "same length, different .dvd bytes");
}
