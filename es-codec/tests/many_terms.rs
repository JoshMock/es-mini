//! A terms dictionary big enough for sub-blocks, floor blocks, and both suffix compressions,
//! from an index written by real Lucene 10.5.1: see tests/gen/GenManyTerms.java (`terms()` there
//! must match `terms` here). Doc d holds term d.

use es_codec::{CompoundFile, SegmentInfos, field_infos::read_field_infos, terms::read_terms};
use std::path::PathBuf;

fn base36(mut n: u32) -> String {
    let mut out = vec![];
    loop {
        out.push(char::from_digit(n % 36, 36).unwrap());
        n /= 36;
        if n == 0 {
            return out.iter().rev().collect();
        }
    }
}

fn terms() -> Vec<String> {
    let mut out: Vec<String> = (0..60)
        .map(|i| format!("lz4{i:03}-abcdefabcdefabcdef"))
        .collect();
    let mut x: u64 = 42;
    for i in 0..60 {
        let mut t = "low".to_string();
        for _ in 0..8 {
            x = (x * 1_103_515_245 + 12345) & 0x7fff_ffff;
            t.push(char::from(b'a' + ((x >> 16) % 26) as u8));
        }
        out.push(format!("{t}{i}"));
    }
    out.extend((0..400).map(|i| format!("flo{i:03}")));
    out.extend((0..100).map(|i| base36(i * 37)));
    out
}

#[test]
fn reads_a_multi_block_terms_dictionary() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data/many-terms");
    let infos = SegmentInfos::read_latest(&dir).unwrap();
    let seg = &infos.segments[0];
    let cfs = CompoundFile::open(&dir, &seg.name, &seg.info.id).unwrap();
    let fields = read_field_infos(&dir, seg, &cfs).unwrap();
    let mut want: Vec<(String, u32)> = terms().into_iter().zip(0..).collect();
    want.sort();
    let got: Vec<(String, Vec<(u32, u32)>)> = read_terms(&cfs, seg, &fields, "t")
        .unwrap()
        .into_iter()
        .map(|t| (String::from_utf8(t.term).unwrap(), t.docs))
        .collect();
    assert_eq!(got.len(), want.len());
    for ((term, docs), (want_term, doc)) in got.iter().zip(&want) {
        assert_eq!(term, want_term);
        assert_eq!(docs, &[(*doc, 1)], "{term}");
    }
}
