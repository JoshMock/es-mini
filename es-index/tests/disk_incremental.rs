//! es_index::disk::save writes only what changed since the previous save as a new segment.

use es_codec::{SegmentInfos, cluster_state::read_indices};
use es_index::{FieldMapping, Index, Indices, disk};
use serde_json::json;
use std::path::{Path, PathBuf};

fn scratch(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("esm-incr-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root.join("data")
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
    idx.mappings
        .properties
        .insert("title".into(), FieldMapping::Text);
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
