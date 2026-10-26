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
fn flushes_by_translog_size() {
    let dir = scratch("flush");
    let mut indices = created();
    let ix = indices.get_mut("idx").unwrap();
    ix.settings
        .merge(&json!({"index.translog.flush_threshold_size": "1kb"}));
    disk::persist(&dir, &mut indices).unwrap();
    let first = ckp(&dir).generation;
    // Small writes stay in the translog...
    indices.get_mut("idx").unwrap().put("small", json!({}));
    disk::persist(&dir, &mut indices).unwrap();
    assert_eq!((ckp(&dir).generation, ckp(&dir).num_ops), (first, 1));
    // ...until it outgrows index.translog.flush_threshold_size.
    let big = "x".repeat(1024);
    indices
        .get_mut("idx")
        .unwrap()
        .put("big", json!({ "s": big }));
    disk::persist(&dir, &mut indices).unwrap();
    let cp = ckp(&dir);
    assert!(cp.generation > first, "no flush");
    assert_eq!(cp.num_ops, 0, "{cp:?}");
    assert!(
        !shard(&dir)
            .join(format!("translog/translog-{first}.tlog"))
            .exists()
    );
    assert_eq!(disk::open(&dir).unwrap()["idx"].docs().count(), 3);
}

#[test]
fn async_durability_defers_fsync() {
    let dir = scratch("async");
    let mut indices = created();
    let ix = indices.get_mut("idx").unwrap();
    ix.settings
        .merge(&json!({"index.translog": {"durability": "async", "sync_interval": "7s"}}));
    disk::persist(&dir, &mut indices).unwrap();
    assert_eq!(
        disk::sync_interval(&indices),
        std::time::Duration::from_secs(7)
    );
    indices.get_mut("idx").unwrap().put("2", json!({}));
    disk::persist(&dir, &mut indices).unwrap();
    // Not logged by the request: a crash now loses the write, as in Elasticsearch.
    assert_eq!(ckp(&dir).num_ops, 0);
    assert!(disk::open(&dir).unwrap()["idx"].get("2").is_none());
    // The periodic sync logs it.
    disk::sync(&dir, &mut indices).unwrap();
    assert_eq!(ckp(&dir).num_ops, 1);
    assert!(disk::open(&dir).unwrap()["idx"].get("2").is_some());
}

#[test]
fn flushes_idle_shards() {
    let dir = scratch("idle");
    let mut indices = created();
    disk::persist(&dir, &mut indices).unwrap();
    let first = ckp(&dir).generation;
    indices.get_mut("idx").unwrap().put("2", json!({}));
    disk::persist(&dir, &mut indices).unwrap();
    disk::sync(&dir, &mut indices).unwrap();
    assert_eq!((ckp(&dir).generation, ckp(&dir).num_ops), (first, 1));
    // No writes for 5 minutes (Elasticsearch's indices.memory.shard_inactive_time): a flush.
    let ago = std::time::SystemTime::now() - std::time::Duration::from_secs(301);
    let file = std::fs::File::options()
        .write(true)
        .open(shard(&dir).join("translog/translog.ckp"))
        .unwrap();
    file.set_modified(ago).unwrap();
    disk::sync(&dir, &mut indices).unwrap();
    let cp = ckp(&dir);
    assert!(cp.generation > first, "no flush");
    assert_eq!(cp.num_ops, 0);
    assert_eq!(disk::open(&dir).unwrap()["idx"].docs().count(), 2);
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

#[test]
fn begun_persists_log_only_the_writes_taken() {
    let dir = scratch("begun");
    let mut indices = created();
    disk::persist(&dir, &mut indices).unwrap();
    indices.get_mut("idx").unwrap().put("2", json!({}));
    let pending = disk::begin_persist(&dir, &mut indices).unwrap();
    assert_eq!(ckp(&dir).num_ops, 0);
    // The caller can release its lock and take more writes while the fsync runs.
    indices.get_mut("idx").unwrap().put("3", json!({}));
    pending.finish().unwrap();
    let back = disk::open(&dir).unwrap();
    assert!(back["idx"].get("2").is_some());
    assert!(back["idx"].get("3").is_none());
    disk::persist(&dir, &mut indices).unwrap();
    assert!(disk::open(&dir).unwrap()["idx"].get("3").is_some());
}
