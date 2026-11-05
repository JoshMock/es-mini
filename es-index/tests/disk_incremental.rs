//! es_index::disk::save writes only what changed since the previous save as a new segment.

use es_codec::{SegmentInfos, cluster_state::read_indices};
use es_index::{FieldMapping, Index, Indices, disk};
use serde_json::json;
use std::path::{Path, PathBuf};

mod common;
use common::Scratch;

fn no_check(_: &Index, _: &serde_json::Value) -> Result<(), es_index::mapping::MapError> {
    Ok(())
}

fn scratch(name: &str) -> Scratch {
    Scratch::new(&format!("incr-{name}"), Some("data"))
}

fn shard(dir: &Path) -> PathBuf {
    let meta = read_indices(dir).unwrap();
    dir.join("indices").join(&meta[0].uuid).join("0/index")
}

fn commit(dir: &Path) -> SegmentInfos {
    SegmentInfos::read_latest(&shard(dir)).unwrap()
}

#[test]
fn a_one_document_write_adds_one_small_segment() {
    let dir = scratch("one");
    let mut idx = Index::default();
    idx.mappings.properties.insert(
        "title".into(),
        FieldMapping::Text {
            store: false,
            fields: Default::default(),
        },
    );
    for i in 0..5000 {
        idx.put(&i.to_string(), json!({"title": format!("doc number {i}")}));
    }
    let mut indices = Indices::from([("idx".to_owned(), idx)]);
    disk::save(&dir, &indices).unwrap();
    assert_eq!(commit(&dir).segments.len(), 1);

    let ix = indices.get_mut("idx").unwrap();
    ix.put("7", json!({"title": "rewritten"}));
    disk::save(&dir, &indices).unwrap();
    let infos = commit(&dir);
    assert_eq!(infos.segments.len(), 2);
    let (old, new) = (&infos.segments[0], &infos.segments[1]);
    assert_eq!((old.info.max_doc, old.del_count), (5000, 1));
    assert_eq!(new.info.max_doc, 1);
    let size = |name: &str| std::fs::metadata(shard(&dir).join(format!("{name}.cfs"))).unwrap();
    assert!(size(&new.name).len() < 4096, "{}", size(&new.name).len());
    assert!(size(&old.name).len() > 10 * size(&new.name).len());

    let back = disk::open(&dir).unwrap();
    assert_eq!(back["idx"].docs().count(), 5000);
    let doc = back["idx"].get("7").unwrap();
    assert_eq!(
        (doc.version, &doc.source),
        (2, &json!({"title": "rewritten"}))
    );
    assert_eq!(
        back["idx"].get("4999").unwrap().source,
        json!({"title": "doc number 4999"})
    );
}

#[test]
fn deletes_stay_deleted_across_saves_and_reopens() {
    let dir = scratch("delete");
    let mut indices = Indices::new();
    let ix = indices.entry("idx".into()).or_default();
    for i in 0..10 {
        ix.put(&i.to_string(), json!({"v": i}));
    }
    disk::save(&dir, &indices).unwrap();
    let ix = indices.get_mut("idx").unwrap();
    ix.delete("3");
    ix.put("4", json!({"v": 40}));
    disk::save(&dir, &indices).unwrap();
    // An unrelated write and a reopen must not bring "3" back.
    let mut indices = disk::open(&dir).unwrap();
    indices.get_mut("idx").unwrap().put("10", json!({"v": 10}));
    disk::save(&dir, &indices).unwrap();
    let back = disk::open(&dir).unwrap();
    assert!(back["idx"].get("3").is_none());
    assert_eq!(back["idx"].docs().count(), 10);
    assert_eq!(back["idx"].get("4").unwrap().source, json!({"v": 40}));
    assert_eq!(back["idx"].get("10").unwrap().source, json!({"v": 10}));
    let infos = commit(&dir);
    assert_eq!(infos.segments.len(), 3);
    assert_eq!(infos.segments[0].del_count, 2);
}

#[test]
fn a_fully_deleted_segment_leaves_the_commit() {
    let dir = scratch("drop");
    let mut indices = Indices::new();
    indices.entry("idx".into()).or_default().put("1", json!({}));
    disk::save(&dir, &indices).unwrap();
    indices.get_mut("idx").unwrap().put("1", json!({"v": 2}));
    disk::save(&dir, &indices).unwrap();
    let infos = commit(&dir);
    assert_eq!(infos.segments.len(), 1);
    assert_eq!(infos.segments[0].info.max_doc, 1);
    assert_eq!(
        disk::open(&dir).unwrap()["idx"].get("1").unwrap().source,
        json!({"v": 2})
    );
}

#[test]
fn many_segments_are_rewritten_as_one() {
    let dir = scratch("merge");
    let mut indices = Indices::new();
    for i in 0..12 {
        indices
            .entry("idx".into())
            .or_default()
            .put(&i.to_string(), json!({"v": i}));
        disk::save(&dir, &indices).unwrap();
        assert!(commit(&dir).segments.len() <= 10);
    }
    assert_eq!(disk::open(&dir).unwrap()["idx"].docs().count(), 12);
}

/// Like Lucene's TieredMergePolicy, merges pick a few similar-size segments: small flushes merge
/// together, dropping their deleted docs, while a much larger segment is left as it is.
#[test]
fn merges_rewrite_only_small_segments() {
    use std::os::unix::fs::MetadataExt;
    let dir = scratch("tiered");
    let mut indices = Indices::new();
    let ix = indices.entry("idx".into()).or_default();
    for i in 0..5000 {
        ix.put(
            &format!("big{i}"),
            json!({"title": format!("doc number {i}")}),
        );
    }
    disk::save(&dir, &indices).unwrap();
    let big = commit(&dir).segments[0].name.clone();
    let cfs = || std::fs::metadata(shard(&dir).join(format!("{big}.cfs"))).unwrap();
    let ino = cfs().ino();
    for i in 0..30 {
        let ix = indices.get_mut("idx").unwrap();
        ix.put(&format!("n{i}"), json!({"n": i}));
        ix.put("upd", json!({"n": i}));
        disk::save(&dir, &indices).unwrap();
        let infos = commit(&dir);
        assert!(
            infos.segments.len() <= 8,
            "{} segments",
            infos.segments.len()
        );
        assert_eq!(infos.segments[0].name, big);
        assert_eq!(cfs().ino(), ino, "the large segment was rewritten");
    }
    let infos = commit(&dir);
    // 60 small writes, 29 of them overwritten: merges dropped most deleted docs.
    let small: i32 = infos.segments[1..].iter().map(|s| s.info.max_doc).sum();
    assert!(small < 40, "{small} docs in small segments");
    let back = disk::open(&dir).unwrap();
    assert_eq!(back["idx"].docs().count(), 5031);
    assert_eq!(back["idx"].get("upd").unwrap().source, json!({"n": 29}));
    assert_eq!(back["idx"].get("n0").unwrap().source, json!({"n": 0}));
}

#[test]
fn an_elasticsearch_written_index_is_rewritten_whole() {
    let es_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../fixtures/es-9.5.5/data");
    let dir = scratch("es");
    std::process::Command::new("cp")
        .args(["-r".as_ref(), es_dir.as_os_str(), dir.as_os_str()])
        .status()
        .unwrap();
    let mut indices = disk::open(&dir).unwrap();
    let name = indices.keys().next().unwrap().clone();
    let count = indices[&name].docs().count();
    indices.get_mut(&name).unwrap().put("new", json!({}));
    disk::save(&dir, &indices).unwrap();
    let back = disk::open(&dir).unwrap();
    assert_eq!(back[&name].docs().count(), count + 1);
    for meta in read_indices(&dir).unwrap() {
        let shard = dir.join("indices").join(&meta.uuid).join("0/index");
        assert!(SegmentInfos::read_latest(&shard).unwrap().segments.len() <= 1);
    }
}

#[test]
fn writes_replace_and_delete_documents_served_from_disk() {
    let dir = scratch("overwrite");
    let mut indices = Indices::new();
    let ix = indices.entry("idx".into()).or_default();
    for i in 0..5 {
        ix.put(&i.to_string(), json!({"v": i}));
    }
    disk::save(&dir, &indices).unwrap();

    let mut indices = disk::open(&dir).unwrap();
    let ix = indices.get_mut("idx").unwrap();
    assert_eq!(ix.create("0", json!({})), Err(1));
    let r = ix
        .update("1", None, &json!({"w": 1}), None, true, no_check)
        .unwrap()
        .unwrap();
    assert_eq!((r.version, r.seq_no), (2, 5));
    assert_eq!(ix.delete("2").version, 2);
    assert!(ix.get("2").is_none());
    assert_eq!(ix.get("1").unwrap().source, json!({"v": 1, "w": 1}));
    assert_eq!(ix.docs().count(), 4);
    disk::save(&dir, &indices).unwrap();

    let back = disk::open(&dir).unwrap();
    let mut ids: Vec<_> = back["idx"].docs().map(|(id, _)| id.to_owned()).collect();
    ids.sort();
    assert_eq!(ids, ["0", "1", "3", "4"]);
    let one = back["idx"].get("1").unwrap();
    assert_eq!((one.version, &one.source), (2, &json!({"v": 1, "w": 1})));
}

mod in_place {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    fn ino(p: &Path) -> u64 {
        std::fs::metadata(p).unwrap().ino()
    }

    fn two_indices() -> Indices {
        let mut indices = Indices::new();
        for name in ["a", "b"] {
            let ix = indices.entry(name.into()).or_default();
            ix.settings.record_creation(name);
            for i in 0..100 {
                ix.put(&i.to_string(), json!({"n": i}));
            }
        }
        indices
    }

    fn shard_of(dir: &Path, name: &str) -> PathBuf {
        let meta = read_indices(dir).unwrap();
        let m = meta.iter().find(|m| m.name == name).unwrap();
        dir.join("indices").join(&m.uuid).join("0/index")
    }

    /// A document write commits into the live directory, Lucene-style, instead of building and
    /// swapping in a copy of it: the directory and existing segment files stay the same.
    #[test]
    fn a_document_write_commits_in_place() {
        let dir = scratch("inplace");
        let mut indices = two_indices();
        disk::save(&dir, &indices).unwrap();
        let a = shard_of(&dir, "a");
        let first = commit_at(&a).segments[0].name.clone();
        let (dir_ino, cfs_ino) = (ino(&dir), ino(&a.join(format!("{first}.cfs"))));
        indices.get_mut("a").unwrap().put("1", json!({"n": -1}));
        disk::save(&dir, &indices).unwrap();
        assert_eq!(ino(&dir), dir_ino, "data directory was replaced");
        assert_eq!(
            ino(&a.join(format!("{first}.cfs"))),
            cfs_ino,
            "segment was rewritten"
        );
        let back = disk::open(&dir).unwrap();
        assert_eq!(back["a"].get("1").unwrap().source, json!({"n": -1}));
        // Files of the superseded commit are gone.
        let commits = std::fs::read_dir(&a).unwrap().filter(|e| {
            e.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("segments_")
        });
        assert_eq!(commits.count(), 1);
    }

    #[test]
    fn an_unchanged_index_is_not_rewritten() {
        let dir = scratch("untouched");
        let mut indices = two_indices();
        disk::save(&dir, &indices).unwrap();
        let before = commit_at(&shard_of(&dir, "b")).generation;
        indices.get_mut("a").unwrap().put("1", json!({"n": -1}));
        disk::save(&dir, &indices).unwrap();
        assert_eq!(commit_at(&shard_of(&dir, "b")).generation, before);
    }

    /// A crash mid-commit leaves partial files under names the next commit would use, and maybe
    /// a temporary segments file; reopening sees the last commit and the next save succeeds.
    #[test]
    fn leftovers_of_an_interrupted_commit_are_harmless() {
        let dir = scratch("leftovers");
        let mut indices = two_indices();
        disk::save(&dir, &indices).unwrap();
        let a = shard_of(&dir, "a");
        let infos = commit_at(&a);
        let next = format!(
            "_{}",
            es_codec::segment_infos::radix36(infos.counter as u64)
        );
        for f in [
            format!("{next}.cfs"),
            format!("{next}.si"),
            format!("segments_{}.tmp", infos.generation + 1),
        ] {
            std::fs::write(a.join(f), b"half-written").unwrap();
        }
        assert_eq!(disk::open(&dir).unwrap()["a"].docs().count(), 100);
        indices.get_mut("a").unwrap().put("new", json!({"n": 1}));
        disk::save(&dir, &indices).unwrap();
        let back = disk::open(&dir).unwrap();
        assert_eq!(back["a"].docs().count(), 101);
        assert_eq!(back["a"].get("new").unwrap().source, json!({"n": 1}));
    }

    fn commit_at(shard: &Path) -> SegmentInfos {
        SegmentInfos::read_latest(shard).unwrap()
    }
}

/// Index creation commits through the swap path; like an in-place commit, it leaves only the
/// latest `segments_N` and the files it references.
#[test]
fn a_created_index_has_one_commit_file() {
    let dir = scratch("created");
    let mut idx = Index::default();
    idx.put("1", json!({"n": 1}));
    disk::save(&dir, &Indices::from([("idx".to_owned(), idx)])).unwrap();
    let infos = commit(&dir);
    let mut referenced: Vec<String> = infos
        .segments
        .iter()
        .flat_map(|s| s.info.files.iter().cloned())
        .collect();
    referenced.push(format!(
        "segments_{}",
        es_codec::segment_infos::radix36(infos.generation)
    ));
    referenced.sort();
    let mut files: Vec<String> = std::fs::read_dir(shard(&dir))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with('_') || n.starts_with("segments"))
        .collect();
    files.sort();
    assert_eq!(files, referenced);
}
