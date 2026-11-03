//! Decodes the Elasticsearch 9.5.5 fixture with a non-compound segment
//! (fixtures/generate_non_compound.sh).

use es_codec::{SegmentInfos, SegmentReader, doc_values::DocValues, stored_fields::StoredValue};
use std::path::PathBuf;

fn shard_index_dir() -> PathBuf {
    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../fixtures/es-9.5.5-non-compound");
    let uuid = std::fs::read_to_string(fixture.join("index_uuid")).unwrap();
    fixture
        .join("data/indices")
        .join(uuid.trim())
        .join("0/index")
}

/// The force-merged non-compound `_2` and the compound `_3` flushed after it.
fn segments() -> Vec<SegmentReader> {
    let dir = shard_index_dir();
    let infos = SegmentInfos::read_latest(&dir).unwrap();
    (infos.segments.into_iter())
        .map(|s| SegmentReader::open(&dir, s).unwrap())
        .collect()
}

/// Each doc's `_id` (ES `Uid.encodeId` of a digit-only id: `0xfe`, then decimal nibbles).
fn ids(seg: &SegmentReader) -> Vec<String> {
    let id = seg.field("_id").unwrap().number;
    (seg.stored_fields().unwrap().iter())
        .map(|doc| match doc.iter().find(|(n, _)| *n == id) {
            Some((_, StoredValue::Bytes(b))) => (b[1..].iter())
                .flat_map(|x| [x >> 4, x & 0xf])
                .take_while(|n| *n != 0xf)
                .map(|n| char::from(b'0' + n))
                .collect(),
            other => panic!("{other:?}"),
        })
        .collect()
}

#[test]
fn reads_segment_infos_of_a_non_compound_segment() {
    let infos = SegmentInfos::read_latest(&shard_index_dir()).unwrap();
    let names: Vec<_> = infos.segments.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["_2", "_3"]);
    let (s2, s3) = (&infos.segments[0].info, &infos.segments[1].info);
    assert!(!s2.is_compound && s3.is_compound);
    assert_eq!(s2.diagnostics["source"], "merge");
    assert!(s2.files.contains("_2.fdt") && s2.files.contains("_2.si"));
    assert!(!s2.files.iter().any(|f| f.ends_with(".cfs")));
}

#[test]
fn reads_a_non_compound_segment() {
    let segs = segments();
    let s2 = &segs[0];
    s2.cfs.verify().unwrap();
    // The merge put the larger `_1` (docs 4 and 5) first.
    assert_eq!(ids(s2), ["4", "5", "1", "2", "3"]);
    assert_eq!(ids(&segs[1]), ["3", "6"]);
    // Doc 3 was deleted after the merge, a soft delete in a doc values update of `_2`.
    assert_eq!(s2.live_docs().unwrap(), [true, true, true, true, false]);
    let fox = s2.term("title", b"fox").unwrap().unwrap();
    assert_eq!(fox.docs, [(0, 1), (1, 1), (2, 1)]);
    let quick = (s2.terms("title").unwrap().into_iter())
        .find(|t| t.term == b"quick")
        .unwrap();
    let positions: Vec<Vec<u32>> = (quick.positions.iter())
        .map(|p| p.iter().map(|p| p.position).collect())
        .collect();
    assert_eq!(
        (quick.docs, positions),
        (vec![(2, 1), (3, 2)], vec![vec![1], vec![0, 1]])
    );
    let DocValues::Numbers(views) = s2.doc_values("views").unwrap() else {
        panic!("views")
    };
    assert_eq!(views, [[7], [42], [10], [25], [3]]);
    let DocValues::Ordinals { terms, .. } = s2.doc_values("tag").unwrap() else {
        panic!("tag")
    };
    assert_eq!(terms, [&b"animal"[..], b"news", b"pet"]);
    assert_eq!(s2.norms("title").unwrap().unwrap().len(), 5);
    let mut points: Vec<_> = s2
        .points("views")
        .unwrap()
        .into_iter()
        .map(|(d, _)| d)
        .collect();
    points.sort();
    assert_eq!(points, [0, 1, 2, 3, 4]);
}

#[test]
fn verify_checks_each_file_of_a_non_compound_segment() {
    let dir = std::env::temp_dir().join(format!("esm-nocfs-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for f in std::fs::read_dir(shard_index_dir()).unwrap() {
        let f = f.unwrap().path();
        std::fs::copy(&f, dir.join(f.file_name().unwrap())).unwrap();
    }
    let mut fdt = std::fs::read(dir.join("_2.fdt")).unwrap();
    fdt[40] ^= 1;
    std::fs::write(dir.join("_2.fdt"), fdt).unwrap();
    let seg = SegmentInfos::read_latest(&dir).unwrap().segments.remove(0);
    let verified = SegmentReader::open(&dir, seg).unwrap().cfs.verify();
    std::fs::remove_dir_all(&dir).unwrap();
    assert!(verified.unwrap_err().0.contains("checksum mismatch"));
}
