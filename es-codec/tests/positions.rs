//! Positions, offsets, and payloads from an index written by real Lucene 10.5.1: see
//! tests/gen/GenPositions.java (the doc text there must match `text` here).

use es_codec::{
    CompoundFile, SegmentInfos,
    field_infos::read_field_infos,
    terms::{Position, read_terms},
};
use std::{collections::BTreeMap, path::PathBuf};

const DOCS: usize = 600;

fn text(d: usize, payloads: bool) -> String {
    let w = 1 + d % 5;
    let tail = d.is_multiple_of(3) as usize;
    let n = w + if d.is_multiple_of(7) { 300 } else { 0 } + tail;
    (0..n)
        .map(|j| {
            let tok = if j < w {
                "w"
            } else if j < n - tail {
                "x"
            } else {
                "y"
            };
            match payloads && !(d + j).is_multiple_of(4) {
                true => format!("{tok}|{:02}", (d + j) % 100),
                false => tok.to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// term -> per doc `(doc, positions)`, computed by tokenizing `text` on whitespace.
fn expected(offsets: bool, payloads: bool) -> BTreeMap<String, Vec<(u32, Vec<Position>)>> {
    let mut out: BTreeMap<String, Vec<(u32, Vec<Position>)>> = BTreeMap::new();
    for d in 0..DOCS {
        let text = text(d, payloads);
        let mut per_term: BTreeMap<&str, Vec<Position>> = BTreeMap::new();
        let mut start = 0;
        for (j, token) in text.split(' ').enumerate() {
            let (term, payload) = token.split_once('|').unwrap_or((token, ""));
            per_term.entry(term).or_default().push(Position {
                position: j as u32,
                offsets: offsets.then_some((start as u32, (start + token.len()) as u32)),
                payload: payload.as_bytes().to_vec(),
            });
            start += token.len() + 1;
        }
        for (term, positions) in per_term {
            out.entry(term.to_string())
                .or_default()
                .push((d as u32, positions));
        }
    }
    out
}

#[test]
fn reads_positions_offsets_and_payloads() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data/positions");
    let infos = SegmentInfos::read_latest(&dir).unwrap();
    let seg = &infos.segments[0];
    let cfs = CompoundFile::open(&dir, &seg.name, &seg.info.id).unwrap();
    let fields = read_field_infos(&dir, seg, &cfs).unwrap();
    for (field, offsets, payloads) in [
        ("plain", false, false),
        ("offsets", true, false),
        ("payloads", false, true),
        ("both", true, true),
    ] {
        let got = read_terms(&cfs, seg, &fields, field).unwrap();
        let want = expected(offsets, payloads);
        assert_eq!(got.len(), want.len(), "{field} terms");
        for (t, (term, docs)) in got.iter().zip(&want) {
            assert_eq!(t.term, term.as_bytes());
            let doc_ids: Vec<_> = t.docs.iter().map(|(d, _)| *d).collect();
            assert_eq!(doc_ids, docs.iter().map(|(d, _)| *d).collect::<Vec<_>>());
            assert_eq!(
                t.positions.len(),
                t.docs.len(),
                "{field}:{term} positions per doc"
            );
            for ((doc, freq), ((_, want), got)) in t.docs.iter().zip(docs.iter().zip(&t.positions))
            {
                assert_eq!(*freq as usize, want.len(), "{field}:{term} doc {doc} freq");
                assert_eq!(got, want, "{field}:{term} doc {doc}");
            }
        }
    }
}
