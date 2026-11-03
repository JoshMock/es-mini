//! Elasticsearch 9.5.5 replays the translog es-mini appends after a flush: the operations are
//! encoded as Elasticsearch encodes them, and the segments they land on top of index every field
//! as Elasticsearch would, since Lucene refuses a document whose field schema differs from an
//! existing segment's (Elasticsearch then skips it as "a corrupt translog entry").

use es_codec::{
    SegmentInfos, SegmentReader, cluster_state::read_indices, points::sortable_bytes, translog,
};
use es_index::{Index, Indices, disk};
use serde_json::json;
use std::path::{Path, PathBuf};

mod common;
use common::Scratch;

fn fixture_index_dir() -> PathBuf {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures");
    let uuid = std::fs::read_to_string(fixtures.join("expected/index_uuid")).unwrap();
    fixtures
        .join("es-9.5.5/data/indices")
        .join(uuid.trim())
        .join("0/index")
}

fn segments(dir: &Path) -> Vec<SegmentReader> {
    (SegmentInfos::read_latest(dir).unwrap().segments.into_iter())
        .map(|seg| SegmentReader::open(dir, seg).unwrap())
        .collect()
}

/// A translog generation's bytes after its header.
fn operations(tlog: &[u8]) -> Vec<u8> {
    let header = translog::read(tlog).unwrap().0;
    tlog[translog::header(&header.uuid, header.primary_term).len()..].to_vec()
}

/// es-mini logs the writes of `data/translog-es-replay/generate.sh` in the bytes Elasticsearch
/// logged for them: an index, a routed index, an update, a delete and one with a generated id.
#[test]
fn operations_are_logged_as_elasticsearch_logs_them() {
    let recorded = std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/translog-es-replay/translog.tlog"),
    )
    .unwrap();
    let ops = translog::read(&recorded).unwrap().1;
    let Some(translog::Op::Index {
        id: auto_id,
        auto_id_timestamp,
        ..
    }) = ops.last()
    else {
        panic!("the last operation indexes a generated id: {ops:?}")
    };

    let dir = Scratch::new("es-replay-ops", Some("data"));
    let mut indices = Indices::from([("t".to_owned(), Index::default())]);
    disk::persist(&dir, &mut indices).unwrap();
    let ix = indices.get_mut("t").unwrap();
    ix.put("1", json!({"v": 1}));
    ix.put_auto("2", Some("r"), json!({"v": 2}), -1);
    ix.put("1", json!({"v": 3}));
    ix.delete("1");
    ix.put_auto(auto_id, None, json!({"v": 4}), *auto_id_timestamp);
    disk::persist(&dir, &mut indices).unwrap();
    let uuid = &read_indices(&dir).unwrap()[0].uuid;
    let tlog = dir.join("indices").join(uuid).join("0/translog");
    let ckp = translog::read_checkpoint(&std::fs::read(tlog.join("translog.ckp")).unwrap());
    let ours = std::fs::read(tlog.join(format!("translog-{}.tlog", ckp.unwrap().generation)));
    // The headers differ only in the translog's uuid.
    assert_eq!(operations(&ours.unwrap()), operations(&recorded));
}

/// `_seq_no` as `SeqNoFieldMapper` indexes it by default: a long point beside its doc values.
#[test]
fn segments_index_seq_no_points_as_elasticsearch_does() {
    let dir = Scratch::new("es-replay-seqno", Some("data"));
    let mut ix = Index::default();
    ix.put("a", json!({}));
    ix.put("b", json!({}));
    let indices = Indices::from([("idx".to_owned(), ix)]);
    disk::save(&dir, &indices).unwrap();
    let uuid = &read_indices(&dir).unwrap()[0].uuid;
    let ours = segments(&dir.join("indices").join(uuid).join("0/index")).remove(0);
    let theirs = segments(&fixture_index_dir()).remove(0);
    let seq_no = |r: &SegmentReader| {
        let f = r.field("_seq_no").unwrap();
        (
            f.point_dimensions,
            f.point_index_dimensions,
            f.point_num_bytes,
        )
    };
    assert_eq!(seq_no(&ours), seq_no(&theirs));
    let mut points = ours.points("_seq_no").unwrap();
    points.sort();
    assert_eq!(
        points,
        [(0, sortable_bytes(0, 8)), (1, sortable_bytes(1, 8))]
    );
}
