//! Terms with 256+ docs (packed postings blocks), from an index written by real Lucene 10.5.1:
//! see tests/gen/GenBigPostings.java, regenerate with tests/gen/gen.sh.

use es_codec::{CompoundFile, SegmentInfos, field_infos::read_field_infos, terms::read_terms};
use std::path::PathBuf;

const DOCS: u32 = 8500;

/// `(term, [(doc, freq)])` the generator wrote, in term order. `freqs` is false for `key`.
fn expected(field: &str) -> Vec<(&'static str, Vec<(u32, u32)>)> {
    let text = field == "text";
    let freq = |f: u32| if text { f } else { 1 };
    let docs = |keep: &dyn Fn(u32) -> bool, f: &dyn Fn(u32) -> u32| -> Vec<(u32, u32)> {
        (0..DOCS)
            .filter(|d| keep(*d))
            .map(|d| (d, freq(f(d))))
            .collect()
    };
    let mut out = vec![
        ("all", docs(&|_| true, &|d| if d % 5 == 0 { 8 } else { 1 })),
        ("common", docs(&|d| d < 300, &|d| 1 + d % 4)),
        ("dense", docs(&|d| d % 3 != 0, &|_| 1)),
        (
            "seventh",
            docs(&|d| d % 7 == 0, &|d| if d % 13 == 0 { 40 } else { 1 }),
        ),
    ];
    if text {
        out.push(("single", vec![(4242, 1)]));
    }
    out
}

#[test]
fn reads_postings_with_packed_blocks() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data/big-postings");
    let infos = SegmentInfos::read_latest(&dir).unwrap();
    let seg = &infos.segments[0];
    assert_eq!(seg.info.max_doc, DOCS as i32);
    let cfs = CompoundFile::open(&dir, &seg.name, &seg.info.id).unwrap();
    let fields = read_field_infos(&dir, seg, &cfs).unwrap();
    for field in ["text", "key"] {
        let got: Vec<_> = read_terms(&cfs, seg, &fields, field)
            .unwrap()
            .into_iter()
            .map(|t| (String::from_utf8(t.term).unwrap(), t.docs))
            .collect();
        let want = expected(field);
        assert_eq!(got.len(), want.len(), "{field} terms");
        for ((term, docs), (want_term, want_docs)) in got.iter().zip(&want) {
            assert_eq!(term, want_term);
            assert_eq!(docs.len(), want_docs.len(), "{field}:{term} doc count");
            assert_eq!(docs, want_docs, "{field}:{term}");
        }
    }
}
