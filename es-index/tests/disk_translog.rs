//! es_index::disk::persist logs document writes to the shard's translog, in Elasticsearch's
//! format, and commits segments only on a flush; disk::open replays the logged operations.

use es_codec::{SegmentInfos, cluster_state::read_indices, translog};
use es_index::{FieldMapping, Index, Indices, disk};
use serde_json::json;
use std::path::{Path, PathBuf};

mod common;
use common::Scratch;

fn scratch(name: &str) -> Scratch {
    Scratch::new(&format!("tlog-{name}"), Some("data"))
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
#[test]
fn begun_syncs_log_only_the_writes_taken() {
    let dir = scratch("begun-sync");
    let mut indices = created();
    let ix = indices.get_mut("idx").unwrap();
    ix.settings
        .merge(&json!({"index.translog.durability": "async"}));
    disk::persist(&dir, &mut indices).unwrap();
    indices.get_mut("idx").unwrap().put("2", json!({}));
    let pending = disk::begin_sync(&dir, &mut indices).unwrap();
    assert_eq!(ckp(&dir).num_ops, 0);
    // The syncer can release its lock and take more writes while the fsync runs.
    indices.get_mut("idx").unwrap().put("3", json!({}));
    pending.finish().unwrap();
    let back = disk::open(&dir).unwrap();
    assert!(back["idx"].get("2").is_some());
    assert!(back["idx"].get("3").is_none());
}

/// A flush commits every write, so statistics then come from the committed segments (deleted
/// documents included until a merge drops them), as reopening the directory reads them.
#[test]
fn flushed_statistics_match_the_committed_segments() {
    let dir = scratch("stats");
    let mut ix = Index::default();
    ix.settings.record_creation("idx");
    ix.settings
        .merge(&json!({"index.translog.flush_threshold_size": "1b"}));
    ix.mappings
        .properties
        .insert("t".into(), FieldMapping::Text { store: false });
    let mut indices = Indices::from([("idx".to_owned(), ix)]);
    let stats = |ix: &Index| {
        let s = ix.shards()[0]
            .field_stats("t", "t", true, &["fox".into()])
            .unwrap();
        (s.docs, s.total_terms, s.doc_freq["fox"])
    };
    // Committing the empty index leaves in-memory postings, not empty segment statistics.
    disk::persist(&dir, &mut indices).unwrap();
    indices
        .get_mut("idx")
        .unwrap()
        .put("new", json!({"t": "fox"}));
    assert_eq!(stats(&indices["idx"]), (1.0, 1.0, 1.0));
    for round in 0..12 {
        let ix = indices.get_mut("idx").unwrap();
        ix.put(&round.to_string(), json!({"t": "fox fox"}));
        // Replaced both before and after a commit.
        ix.put("u", json!({"t": "fox"}));
        ix.put("u", json!({"t": "fox cat"}));
        disk::persist(&dir, &mut indices).unwrap();
        let reopened = disk::open(&dir).unwrap();
        assert_eq!(stats(&indices["idx"]), stats(&reopened["idx"]), "{round}");
    }
    // 24 docs were committed, 11 of them deleted later: merges dropped some.
    assert!(
        stats(&indices["idx"]).0 < 24.0,
        "{:?}",
        stats(&indices["idx"])
    );
}

/// A committed document replaced in memory stays counted once, as a deleted document of its
/// segment, not again as a superseded in-memory source.
#[test]
fn replacing_a_committed_document_counts_it_once() {
    let dir = scratch("recount");
    let mut ix = Index::default();
    ix.settings.record_creation("idx");
    ix.mappings
        .properties
        .insert("t".into(), FieldMapping::Text { store: false });
    ix.put("a", json!({"t": "fox"}));
    let mut indices = Indices::from([("idx".to_owned(), ix)]);
    disk::persist(&dir, &mut indices).unwrap();
    let ix = indices.get_mut("idx").unwrap();
    ix.put("a", json!({"t": "cat"}));
    let s = ix.shards()[0]
        .field_stats("t", "t", true, &["fox".into()])
        .unwrap();
    // The deleted "fox" in the segment, and "cat" in memory.
    assert_eq!((s.docs, s.total_terms, s.doc_freq["fox"]), (2.0, 2.0, 1.0));
}

/// A failed append keeps its operations, so the next persist logs them without waiting for a
/// flush.
#[test]
fn write_failure_handling_failed_appends_are_retried() {
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch("retry");
    let mut indices = created();
    disk::persist(&dir, &mut indices).unwrap();
    let first = ckp(&dir).generation;
    let tlog = shard(&dir).join(format!("translog/translog-{first}.tlog"));
    indices.get_mut("idx").unwrap().put("2", json!({}));
    std::fs::set_permissions(&tlog, std::fs::Permissions::from_mode(0o444)).unwrap();
    let failed = disk::persist(&dir, &mut indices);
    std::fs::set_permissions(&tlog, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(failed.is_err());
    indices.get_mut("idx").unwrap().put("3", json!({}));
    disk::persist(&dir, &mut indices).unwrap();
    // Logged in order, not flushed.
    let cp = ckp(&dir);
    assert_eq!((cp.generation, cp.num_ops), (first, 2));
    let back = disk::open(&dir).unwrap();
    assert!(back["idx"].get("2").is_some());
    assert!(back["idx"].get("3").is_some());
}

/// A flush that fails after taking the writes to log keeps them for the next persist too.
#[test]
fn write_failure_handling_failed_flushes_keep_their_operations() {
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch("retry-flush");
    let mut indices = created();
    let ix = indices.get_mut("idx").unwrap();
    ix.settings
        .merge(&json!({"index.translog.flush_threshold_size": "1kb"}));
    disk::persist(&dir, &mut indices).unwrap();
    let first = ckp(&dir).generation;
    let big = "x".repeat(1024);
    indices
        .get_mut("idx")
        .unwrap()
        .put("big", json!({ "s": big }));
    let segments = shard(&dir).join("index");
    std::fs::set_permissions(&segments, std::fs::Permissions::from_mode(0o555)).unwrap();
    let failed = disk::persist(&dir, &mut indices);
    std::fs::set_permissions(&segments, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(failed.is_err());
    // Retried, the operation still outgrows the threshold, so the next persist flushes it.
    disk::persist(&dir, &mut indices).unwrap();
    assert!(ckp(&dir).generation > first, "no flush");
    assert!(disk::open(&dir).unwrap()["idx"].get("big").is_some());
}
