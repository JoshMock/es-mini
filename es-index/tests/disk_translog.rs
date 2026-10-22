//! es_index::disk::persist logs document writes to the shard's translog, in Elasticsearch's
//! format, and commits segments only on a flush; disk::open replays the logged operations.

use es_codec::{SegmentInfos, cluster_state::read_indices, translog};
use es_index::{Index, Indices, disk};
use serde_json::json;
use std::path::{Path, PathBuf};

fn scratch(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("esm-tlog-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root.join("data")
}

fn shard(dir: &Path) -> PathBuf {
    let meta = read_indices(dir).unwrap();
    dir.join("indices").join(&meta[0].uuid).join("0")
}

fn ckp(dir: &Path) -> translog::TranslogCheckpoint {
    translog::read_checkpoint(&std::fs::read(shard(dir).join("translog/translog.ckp")).unwrap())
        .unwrap()
}

fn created() -> Indices {
    let mut ix = Index::default();
    ix.settings.record_creation("idx");
    ix.put("1", json!({"v": 1}));
    Indices::from([("idx".to_owned(), ix)])
}

#[test]
fn writes_are_logged_and_replayed_without_a_commit() {
    let dir = scratch("logged");
    let mut indices = created();
    disk::persist(&dir, &mut indices).unwrap();
    let commit = SegmentInfos::read_latest(&shard(&dir).join("index"))
        .unwrap()
        .generation;
    let ix = indices.get_mut("idx").unwrap();
    ix.put("1", json!({"v": 2}));
    ix.put("2", json!({"v": 3}));
    ix.delete("2");
    disk::persist(&dir, &mut indices).unwrap();
    // Logged, not committed: the commit is the same, and the checkpoint covers three ops.
    assert_eq!(
        SegmentInfos::read_latest(&shard(&dir).join("index"))
            .unwrap()
            .generation,
        commit
    );
    let cp = ckp(&dir);
    assert_eq!((cp.num_ops, cp.min_seq_no, cp.max_seq_no), (3, 1, 3));

    let back = disk::open(&dir).unwrap();
    let d = back["idx"].get("1").unwrap();
    assert_eq!(
        (d.source.clone(), d.version, d.seq_no),
        (json!({"v": 2}), 2, 1)
    );
    assert!(back["idx"].get("2").is_none());
    // Sequence numbers and versions continue after the replayed operations.
    let mut back = back;
    let w = back.get_mut("idx").unwrap().put("2", json!({}));
    assert_eq!((w.seq_no, w.version), (4, 3));
}

#[test]
fn a_torn_last_operation_is_ignored() {
    let dir = scratch("torn");
    let mut indices = created();
    disk::persist(&dir, &mut indices).unwrap();
    indices.get_mut("idx").unwrap().put("2", json!({"v": 2}));
    disk::persist(&dir, &mut indices).unwrap();
    // A crash mid-append leaves bytes beyond the checkpoint.
    let cp = ckp(&dir);
    let tlog = shard(&dir).join(format!("translog/translog-{}.tlog", cp.generation));
    let mut bytes = std::fs::read(&tlog).unwrap();
    bytes.extend_from_slice(&[0, 0, 0, 40, 2, 12]);
    std::fs::write(&tlog, bytes).unwrap();
    let back = disk::open(&dir).unwrap();
    assert_eq!(back["idx"].docs().count(), 2);
    // ...and the next append overwrites them.
    let mut back = back;
    back.get_mut("idx").unwrap().put("3", json!({}));
    disk::persist(&dir, &mut back).unwrap();
    assert_eq!(disk::open(&dir).unwrap()["idx"].docs().count(), 3);
}

#[test]
fn many_operations_flush_into_segments_and_a_new_generation() {
    let dir = scratch("flush");
    let mut indices = created();
    disk::persist(&dir, &mut indices).unwrap();
    let first = ckp(&dir).generation;
    for i in 0..disk::FLUSH_OPS {
        indices
            .get_mut("idx")
            .unwrap()
            .put(&format!("d{i}"), json!({"i": i}));
        disk::persist(&dir, &mut indices).unwrap();
    }
    let cp = ckp(&dir);
    assert!(cp.generation > first, "no flush");
    assert!(cp.num_ops < 2, "{cp:?}");
    assert!(
        !shard(&dir)
            .join(format!("translog/translog-{first}.tlog"))
            .exists()
    );
    assert_eq!(
        disk::open(&dir).unwrap()["idx"].docs().count(),
        disk::FLUSH_OPS + 1
    );
}

#[test]
fn creating_an_index_commits() {
    let dir = scratch("create");
    let mut indices = created();
    disk::persist(&dir, &mut indices).unwrap();
    let mut other = Index::default();
    other.settings.record_creation("other");
    other.put("x", json!({}));
    indices.insert("other".into(), other);
    disk::persist(&dir, &mut indices).unwrap();
    let back = disk::open(&dir).unwrap();
    assert_eq!(back["other"].get("x").unwrap().source, json!({}));
}
