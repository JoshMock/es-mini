//! Stored fields of every type, wide numeric doc values, and hard deletes, from an index written
//! by real Lucene 10.5.1: see tests/gen/GenStoredAndNumeric.java (its value formulas must match
//! the ones here).

use es_codec::{
    CompoundFile, SegmentCommitInfo, SegmentInfos,
    field_infos::{FieldInfo, read_field_infos},
    live_docs::read_live_docs,
    stored_fields::{StoredValue, read_stored_fields},
};
use std::path::PathBuf;

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

fn open() -> (PathBuf, SegmentCommitInfo, CompoundFile, Vec<FieldInfo>) {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data/stored-and-numeric");
    let seg = SegmentInfos::read_latest(&dir).unwrap().segments.remove(0);
    let cfs = CompoundFile::open(&dir, &seg.name, &seg.info.id).unwrap();
    let fields = read_field_infos(&dir, &seg, &cfs).unwrap();
    (dir, seg, cfs, fields)
}

#[test]
fn reads_stored_fields_of_every_type() {
    let (_, seg, cfs, fields) = open();
    let name = |n: i32| fields.iter().find(|f| f.number == n).unwrap().name.as_str();
    let docs = read_stored_fields(&cfs, &seg).unwrap();
    assert_eq!(docs.len(), DOCS);
    for (d, doc) in docs.into_iter().enumerate() {
        let got: Vec<(&str, StoredValue)> = doc.into_iter().map(|(n, v)| (name(n), v)).collect();
        assert_eq!(got, stored(d), "doc {d}");
    }
}

#[test]
fn reads_hard_deletes_from_liv() {
    let (dir, seg, cfs, fields) = open();
    assert_eq!((seg.del_gen, seg.del_count), (1, 70));
    let live = read_live_docs(&dir, &seg, &cfs, &fields).unwrap();
    let dead: Vec<usize> = (0..DOCS).filter(|d| !live[*d]).collect();
    let want: Vec<usize> = (0..DOCS).filter(|d| d % 1000 == 7).collect();
    assert_eq!(dead, want);
}
