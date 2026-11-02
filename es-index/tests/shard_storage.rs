//! Each index keeps `index.number_of_shards` independent shard stores (documents, segments and
//! translog), saved under `indices/<uuid>/<shard>/` as Elasticsearch lays them out.

use es_codec::{SegmentInfos, cluster_state::read_indices, translog};
use es_index::{Index, Indices, disk};
use serde_json::json;
use std::path::{Path, PathBuf};

mod common;
use common::Scratch;

fn shard_dir(dir: &Path, shard: usize) -> PathBuf {
    let meta = read_indices(dir).unwrap();
    (dir.join("indices").join(&meta[0].uuid)).join(shard.to_string())
}

fn sharded(n: u32) -> Indices {
    let mut ix = Index::default();
    ix.settings.merge(&json!({"number_of_shards": n}));
    Indices::from([("idx".to_owned(), ix)])
}

/// Per shard, the ids, versions and sequence numbers of its documents, sorted by id.
fn contents(ix: &Index) -> Vec<Vec<(String, u64, u64)>> {
    (ix.shards().iter())
        .map(|s| {
            let mut docs: Vec<_> = (s.docs())
                .map(|(id, d)| (id.to_owned(), d.version, d.seq_no))
                .collect();
            docs.sort();
            docs
        })
        .collect()
}

fn ids(v: &[(String, u64, u64)]) -> Vec<&str> {
    v.iter().map(|d| d.0.as_str()).collect()
}

#[test]
fn each_shard_keeps_its_own_documents_and_sequence_numbers() {
    let mut indices = sharded(3);
    let ix = indices.get_mut("idx").unwrap();
    assert_eq!(ix.put_in(0, "a", json!({"v": 1})).seq_no, 0);
    assert_eq!(ix.put_in(1, "b", json!({"v": 2})).seq_no, 0);
    let again = ix.put_in(1, "b", json!({"v": 3}));
    assert_eq!((again.version, again.seq_no), (2, 1));
    // the same id in another shard is another document
    assert_eq!(ix.put_in(2, "b", json!({"v": 4})).version, 1);
    assert_eq!(ix.shards()[1].get("b").unwrap().source, json!({"v": 3}));
    assert!(ix.shards()[0].get("b").is_none());
    assert_eq!(
        contents(ix),
        [
            vec![("a".into(), 1, 0)],
            vec![("b".into(), 2, 1)],
            vec![("b".into(), 1, 0)]
        ]
    );
}

#[test]
fn a_save_writes_one_directory_per_shard_and_open_reads_them_back() {
    let dir = Scratch::new("shards-save", Some("data"));
    let mut indices = sharded(3);
    let ix = indices.get_mut("idx").unwrap();
    for i in 0..9 {
        ix.put_in(i % 3, &i.to_string(), json!({"i": i}));
    }
    ix.delete_in(2, "5");
    let want = contents(ix);
    disk::save(&dir, &indices).unwrap();
    for shard in 0..3 {
        let s = shard_dir(&dir, shard);
        assert!(s.join("_state").is_dir() && s.join("translog/translog.ckp").is_file());
        let infos = SegmentInfos::read_latest(&s.join("index")).unwrap();
        assert_eq!(infos.segments.len(), 1, "shard {shard}");
    }
    assert!(!shard_dir(&dir, 3).exists());
    let back = disk::open(&dir).unwrap();
    assert_eq!(contents(&back["idx"]), want);
    assert_eq!(ids(&want[2]), ["2", "8"]);
}

#[test]
fn shards_never_written_are_saved_empty() {
    let dir = Scratch::new("shards-empty", Some("data"));
    let mut indices = sharded(2);
    indices.get_mut("idx").unwrap().put("a", json!({}));
    disk::save(&dir, &indices).unwrap();
    let empty = SegmentInfos::read_latest(&shard_dir(&dir, 1).join("index")).unwrap();
    assert!(empty.segments.is_empty());
    let back = disk::open(&dir).unwrap();
    assert_eq!(back["idx"].shards().len(), 2);
    assert_eq!(contents(&back["idx"])[1], []);
}

#[test]
fn writes_are_logged_to_their_own_shards_translog_and_replayed() {
    let dir = Scratch::new("shards-tlog", Some("data"));
    let mut indices = sharded(3);
    disk::persist(&dir, &mut indices).unwrap();
    let ix = indices.get_mut("idx").unwrap();
    ix.put_in(2, "x", json!({"v": 1}));
    ix.put_in(2, "y", json!({"v": 2}));
    ix.put_in(0, "z", json!({"v": 3}));
    disk::persist(&dir, &mut indices).unwrap();
    let ops = |shard| {
        let ckp = std::fs::read(shard_dir(&dir, shard).join("translog/translog.ckp")).unwrap();
        translog::read_checkpoint(&ckp).unwrap().num_ops
    };
    assert_eq!([ops(0), ops(1), ops(2)], [1, 0, 2]);
    let back = disk::open(&dir).unwrap();
    let got = contents(&back["idx"]);
    assert_eq!(
        got.iter().map(|s| ids(s)).collect::<Vec<_>>(),
        [vec!["z"], vec![], vec!["x", "y"]]
    );
}

#[test]
fn a_flush_commits_every_shard() {
    let dir = Scratch::new("shards-flush", Some("data"));
    let mut indices = sharded(2);
    disk::persist(&dir, &mut indices).unwrap();
    let ix = indices.get_mut("idx").unwrap();
    ix.put_in(1, "a", json!({}));
    // a metadata change commits segments instead of logging
    ix.settings.merge(&json!({"index.refresh_interval": "1s"}));
    disk::persist(&dir, &mut indices).unwrap();
    let infos = SegmentInfos::read_latest(&shard_dir(&dir, 1).join("index")).unwrap();
    assert_eq!(infos.segments.len(), 1);
    let ckp = std::fs::read(shard_dir(&dir, 1).join("translog/translog.ckp")).unwrap();
    assert_eq!(translog::read_checkpoint(&ckp).unwrap().num_ops, 0);
    assert_eq!(ids(&contents(&disk::open(&dir).unwrap()["idx"])[1]), ["a"]);
}

#[test]
fn a_single_shard_index_is_saved_as_shard_zero_only() {
    let dir = Scratch::new("shards-one", Some("data"));
    let mut indices = sharded(1);
    indices.get_mut("idx").unwrap().put("a", json!({}));
    disk::save(&dir, &indices).unwrap();
    let index_dir = shard_dir(&dir, 0).parent().unwrap().to_owned();
    let entries: Vec<_> = std::fs::read_dir(index_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(entries, ["0"]);
    assert_eq!(ids(&contents(&disk::open(&dir).unwrap()["idx"])[0]), ["a"]);
}

#[test]
fn snapshots_copy_every_shard() {
    let mut indices = sharded(2);
    let ix = indices.get_mut("idx").unwrap();
    ix.put_in(1, "a", json!({}));
    let snap = ix.snapshot();
    ix.delete_in(1, "a");
    assert_eq!(ids(&contents(&snap)[1]), ["a"]);
}

/// A shard directory missing beside its index's others is damage, not an empty shard: opening it
/// as empty would let the next save erase that shard's documents.
#[test]
fn a_missing_shard_directory_fails_open() {
    for missing in [0, 1] {
        let dir = Scratch::new(&format!("shards-missing-{missing}"), Some("data"));
        let mut indices = sharded(2);
        let ix = indices.get_mut("idx").unwrap();
        ix.put_in(0, "a", json!({"v": 1}));
        ix.put_in(1, "b", json!({"v": 2}));
        disk::save(&dir, &indices).unwrap();
        std::fs::remove_dir_all(shard_dir(&dir, missing)).unwrap();
        let err = disk::open(&dir).err().map(|e| e.0);
        let expected = format!("index [idx]: shard [{missing}]: missing");
        assert!(
            err.as_ref().is_some_and(|e| e.contains(&expected)),
            "{err:?}"
        );
    }
}

/// A get without routing reads the shard its id routes to, not shard 0.
#[test]
fn shard_reads_get_reads_the_shard_an_id_routes_to() {
    let mut indices = sharded(3);
    let ix = indices.get_mut("idx").unwrap();
    let ids: Vec<String> = (0..12).map(|n| n.to_string()).collect();
    for id in &ids {
        ix.put(id, json!({"n": id}));
    }
    assert!(ids.iter().any(|id| ix.shard_for(id, None) != 0));
    for id in &ids {
        assert_eq!(ix.get(id).unwrap().source["n"], *id, "{id}");
    }
}
