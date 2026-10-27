//! Binary doc values from an index written by real Lucene 10.5.1: see
//! tests/gen/GenBinaryDocValues.java (the values there must match `value` here).

use es_codec::doc_values::DocValues;

mod common;

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
    let seg = common::open("binary-doc-values");
    for field in ["fixed", "sparse_fixed", "varlen", "dense_varlen"] {
        let DocValues::Bytes(got) = seg.doc_values(field).unwrap() else {
            panic!("{field} is not binary");
        };
        let want: Vec<_> = (0..DOCS).map(|d| value(field, d)).collect();
        assert_eq!(got, want, "{field}");
    }
}

/// es-codec writes the same `.dvm`/`.dvd` bytes as Lucene for the same binary values.
#[test]
fn written_binary_doc_values_match_lucene() {
    let seg = common::open("binary-doc-values");
    let mut fields: Vec<_> = ["fixed", "sparse_fixed", "varlen", "dense_varlen"]
        .iter()
        .map(|f| {
            let number = seg.field(f).unwrap().number;
            (number, (0..DOCS).map(|d| value(f, d)).collect::<Vec<_>>())
        })
        .collect();
    fields.sort_by_key(|f| f.0); // a merge writes fields in number order
    let (dvm, dvd) = es_codec::doc_values::write_doc_values_with(
        &[],
        &[],
        &[],
        &fields,
        &seg.seg.info.id,
        "Lucene90_0",
    );
    for (ext, ours) in [("_Lucene90_0.dvm", dvm), ("_Lucene90_0.dvd", dvd)] {
        let real = seg.cfs.file(ext).unwrap().unwrap();
        let at = (real.iter().zip(&ours)).position(|(a, b)| a != b);
        assert!(
            at.is_none() && real.len() == ours.len(),
            "{ext} differs at {at:?}"
        );
    }
}
