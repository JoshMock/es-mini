//! Binary doc values from an index written by real Lucene 10.5.1: see
//! tests/gen/GenBinaryDocValues.java (the values there must match `value` here).

use es_codec::{
    CompoundFile, SegmentInfos,
    doc_values::{DocValues, read_field_values},
    field_infos::read_field_infos,
};
use std::path::PathBuf;

const DOCS: usize = 300;

fn value(field: &str, d: usize) -> Option<Vec<u8>> {
    match field {
        "fixed" => Some((d as u64 * 0x0001_0101_0101).to_be_bytes().to_vec()),
        "sparse_fixed" => d
            .is_multiple_of(10)
            .then(|| (d as u32).to_be_bytes().to_vec()),
        "varlen" => {
            (!d.is_multiple_of(3)).then(|| (0..d % 17).map(|i| (d * 31 + i) as u8).collect())
        }
        "dense_varlen" => Some((0..1 + d % 5).map(|i| (d + i) as u8).collect()),
        _ => unreachable!(),
    }
}

#[test]
fn reads_binary_doc_values() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data/binary-doc-values");
    let infos = SegmentInfos::read_latest(&dir).unwrap();
    let seg = &infos.segments[0];
    let cfs = CompoundFile::open(&dir, &seg.name, &seg.info.id).unwrap();
    let fields = read_field_infos(&dir, seg, &cfs).unwrap();
    for field in ["fixed", "sparse_fixed", "varlen", "dense_varlen"] {
        let DocValues::Bytes(got) = read_field_values(&dir, seg, &cfs, &fields, field).unwrap()
        else {
            panic!("{field} is not binary");
        };
        let want: Vec<_> = (0..DOCS).map(|d| value(field, d)).collect();
        assert_eq!(got, want, "{field}");
    }
}
