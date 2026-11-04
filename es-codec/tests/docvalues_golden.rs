//! es-codec writes the same doc values bytes as Elasticsearch 9.5.5 for multi-valued and sparse
//! fields with more than 1024 distinct terms: fixtures/generate_doc_values.sh had Elasticsearch
//! write the `dv` index, whose documents `kw`, `one` and `num` here mirror.
//!
//! No byte legitimately differs: both files' headers carry the segment id, which the test takes
//! from Elasticsearch's segment, and their footers' checksums then cover equal bytes.

use es_codec::{SegmentInfos, SegmentReader, doc_values::DocValuesWriter};
use std::path::PathBuf;

const DOCS: usize = 3000;

/// Every third doc has none; up to three of 2000 terms.
fn kw(d: usize) -> Vec<Vec<u8>> {
    if d.is_multiple_of(3) {
        return vec![];
    }
    (0..1 + d % 3)
        .map(|i| format!("term-{:04}", (d * 7 + i * 613) % 2000).into_bytes())
        .collect()
}

/// One term per odd doc, 1500 in all.
fn one(d: usize) -> Vec<Vec<u8>> {
    (d % 2 == 1)
        .then(|| format!("v-{d:05}").into_bytes())
        .into_iter()
        .collect()
}

/// Every fifth doc has none; up to two values, negative and large, in ascending order.
fn num(d: usize) -> Vec<i64> {
    let d64 = d as i64;
    match d {
        _ if d.is_multiple_of(5) => vec![],
        _ if d.is_multiple_of(2) => vec![d64 * 1_000_003 - 1_500_000_000],
        _ => vec![-d64, d64 << 33],
    }
}

fn segment() -> SegmentReader {
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../fixtures/es-9.5.5-doc-values");
    let uuid = std::fs::read_to_string(fixture.join("index_uuid")).unwrap();
    let dir = (fixture.join("data/indices"))
        .join(uuid.trim())
        .join("0/index");
    let seg = SegmentInfos::read_latest(&dir).unwrap().segments.remove(0);
    SegmentReader::open(&dir, seg).unwrap()
}

#[test]
fn written_doc_values_match_elasticsearch() {
    let seg = segment();
    assert_eq!(seg.max_doc(), DOCS);
    let n = |name| seg.field(name).unwrap().number;
    let per_doc = |f: fn(usize) -> Vec<Vec<u8>>| (0..DOCS).map(f).collect::<Vec<_>>();
    // The bulk indexed one doc at a time, so doc d has `_seq_no` d.
    let seq_no: Vec<_> = (0..DOCS as i64).map(|d| vec![d]).collect();
    let ones = vec![vec![1]; DOCS];
    let mut w = DocValuesWriter::new(&seg.seg.info.id, "Lucene90_0");
    // A flush writes fields in the order of Lucene's IndexingChain hash table, not by number.
    w.numeric(n("_seq_no"), &seq_no);
    w.keyword(n("one"), &per_doc(one));
    w.sorted_numeric(n("num"), &(0..DOCS).map(num).collect::<Vec<_>>());
    w.numeric(n("_primary_term"), &ones);
    w.keyword(n("kw"), &per_doc(kw));
    w.numeric(n("_version"), &ones);
    let (dvm, dvd) = w.finish();
    for (ext, ours) in [("_Lucene90_0.dvm", dvm), ("_Lucene90_0.dvd", dvd)] {
        let real = seg.cfs.file(ext).unwrap().unwrap();
        let at = (real.iter().zip(&ours)).position(|(a, b)| a != b);
        assert!(
            at.is_none() && real.len() == ours.len(),
            "{ext} differs at {at:?} (lengths {} and {})",
            real.len(),
            ours.len()
        );
    }
}
