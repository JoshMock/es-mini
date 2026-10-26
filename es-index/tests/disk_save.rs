//! Crash safety of es_index::disk::save: a save either fully replaces the data directory or
//! leaves the previous snapshot intact.

use es_index::{Indices, disk};
use serde_json::json;
use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf};

struct Scratch(PathBuf, PathBuf);

impl std::ops::Deref for Scratch {
    type Target = PathBuf;
    fn deref(&self) -> &PathBuf {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.1);
    }
}

fn scratch(name: &str) -> Scratch {
    let root = std::env::temp_dir().join(format!("esm-save-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    Scratch(root.clone(), root)
}

fn one_doc(source: serde_json::Value) -> Indices {
    let mut indices = Indices::new();
    indices.entry("idx".into()).or_default().put("1", source);
    indices
}

fn source_of(dir: &std::path::Path) -> serde_json::Value {
    disk::open(dir).unwrap()["idx"]
        .get("1")
        .unwrap()
        .source
        .clone()
}

#[test]
fn failed_save_keeps_the_previous_snapshot() {
    // A document write commits in place: a read-only shard directory makes it fail.
    let root = scratch("fail");
    let dir = root.join("data");
    disk::save(&dir, &one_doc(json!({"v": 1}))).unwrap();
    let meta = es_codec::cluster_state::read_indices(&dir).unwrap();
    let shard = dir.join("indices").join(&meta[0].uuid).join("0/index");
    // A real update: the same index with a newer version (and seq_no) of the doc.
    let mut updated = one_doc(json!({"v": 1}));
    updated.get_mut("idx").unwrap().put("1", json!({"v": 2}));
    fs::set_permissions(&shard, fs::Permissions::from_mode(0o555)).unwrap();
    let result = disk::save(&dir, &updated);
    fs::set_permissions(&shard, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(result.is_err());
    assert_eq!(source_of(&dir), json!({"v": 1}));

    // Creating an index rebuilds the directory beside it: a read-only parent makes that fail.
    let mut more = one_doc(json!({"v": 1}));
    more.insert("other".into(), Default::default());
    fs::set_permissions(&*root, fs::Permissions::from_mode(0o555)).unwrap();
    let result = disk::save(&dir, &more);
    fs::set_permissions(&*root, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(result.is_err());
    assert_eq!(source_of(&dir), json!({"v": 1}));
}

#[test]
fn recover_restores_a_swap_interrupted_between_renames() {
    let root = scratch("recover");
    let dir = root.join("data");
    disk::save(&dir, &one_doc(json!({"v": 1}))).unwrap();
    // Simulate a crash after the old snapshot was moved aside, before the new one moved in.
    fs::rename(&dir, root.join("data.esmini-old")).unwrap();
    fs::create_dir_all(root.join("data.esmini-new/half-written")).unwrap();
    disk::recover(&dir).unwrap();
    assert_eq!(source_of(&dir), json!({"v": 1}));
    assert!(!root.join("data.esmini-new").exists());
}

#[test]
fn save_keeps_other_top_level_entries() {
    let root = scratch("extras");
    let dir = root.join("data");
    fs::create_dir_all(dir.join("nodes")).unwrap();
    fs::write(dir.join("node.lock"), b"").unwrap();
    disk::save(&dir, &one_doc(json!({"v": 1}))).unwrap();
    assert!(dir.join("nodes").is_dir() && dir.join("node.lock").is_file());
}

#[test]
fn resaving_an_elasticsearch_directory_keeps_its_index_identity() {
    let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../fixtures");
    let es_dir = fixtures.join("es-9.5.5/data");
    let es_meta = es_codec::cluster_state::read_indices(&es_dir).unwrap();
    let root = scratch("identity");
    let dir = root.join("data");
    disk::save(&dir, &disk::open(&es_dir).unwrap()).unwrap();
    let fixture = |m: Vec<es_codec::cluster_state::IndexMetadata>| {
        m.into_iter().find(|m| m.name == "fixture").unwrap()
    };
    let (meta, es_meta) = (
        fixture(es_codec::cluster_state::read_indices(&dir).unwrap()),
        fixture(es_meta),
    );
    assert_eq!(
        meta.uuid,
        fs::read_to_string(fixtures.join("expected/index_uuid"))
            .unwrap()
            .trim()
    );
    assert_eq!(meta.settings["index.uuid"], meta.uuid);
    assert_eq!(
        meta.settings["index.version.created"],
        es_meta.settings["index.version.created"]
    );
}

/// Elasticsearch treats a shard's history as continuous only if its commit keeps the same
/// history, translog and sequence-number markers.
#[test]
fn resaving_keeps_commit_history() {
    use es_codec::SegmentInfos;
    let es_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../fixtures/es-9.5.5/data");
    let root = scratch("history");
    let dir = root.join("data");
    disk::save(&dir, &disk::open(&es_dir).unwrap()).unwrap();
    let saved = es_codec::cluster_state::read_indices(&dir).unwrap();
    assert!(!saved.is_empty());
    for meta in saved {
        let shard = |root: &PathBuf| root.join("indices").join(&meta.uuid).join("0");
        let commit = |root: &PathBuf| {
            SegmentInfos::read_latest(&shard(root).join("index"))
                .unwrap()
                .user_data
        };
        let (es, ours) = (commit(&es_dir), commit(&dir));
        for key in [
            "history_uuid",
            "translog_uuid",
            "max_seq_no",
            "local_checkpoint",
            "max_unsafe_auto_id_timestamp",
        ] {
            assert_eq!(ours[key], es[key], "{} {key}", meta.name);
        }
        // The translog's header names the commit's translog_uuid.
        let translog = shard(&dir).join("translog");
        let tlog = fs::read_dir(&translog)
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.extension().is_some_and(|x| x == "tlog"))
            .unwrap();
        let tlog = fs::read(tlog).unwrap();
        let uuid = ours["translog_uuid"].as_bytes();
        assert!(tlog.windows(uuid.len()).any(|w| w == uuid), "{}", meta.name);
    }
}

/// Hundreds of distinct terms per field (each term in fewer than 256 docs).
#[test]
fn saves_and_reloads_many_terms_and_docs() {
    use es_index::{FieldMapping, Index};
    let mut idx = Index::default();
    idx.mappings
        .properties
        .insert("title".into(), FieldMapping::Text { store: false });
    idx.mappings
        .properties
        .insert("tag".into(), FieldMapping::Keyword { ignore_above: None });
    for i in 0..200 {
        idx.put(
            &i.to_string(),
            json!({"title": format!("common word{i}"), "tag": format!("t{}", i % 2)}),
        );
    }
    let mut indices = Indices::new();
    indices.insert("big".into(), idx);
    let root = scratch("big");
    let dir = root.join("data");
    disk::save(&dir, &indices).unwrap();
    let back = disk::open(&dir).unwrap();
    assert_eq!(back["big"].docs().count(), 200);
    assert_eq!(
        back["big"].get("199").unwrap().source["title"],
        "common word199"
    );
}

/// Terms in 256+ docs are written as full postings blocks.
#[test]
fn a_term_in_256_docs_saves_every_document() {
    use es_index::{FieldMapping, Index};
    let mut idx = Index::default();
    idx.mappings
        .properties
        .insert("tag".into(), FieldMapping::Keyword { ignore_above: None });
    idx.mappings
        .properties
        .insert("ok".into(), FieldMapping::Boolean);
    for i in 0..300 {
        idx.put(&i.to_string(), json!({"tag": "same", "ok": true}));
    }
    let root = scratch("full-block");
    let dir = root.join("data");
    disk::save(&dir, &Indices::from([("idx".to_owned(), idx)])).unwrap();
    let back = disk::open(&dir).unwrap();
    assert_eq!(back["idx"].docs().count(), 300);
    assert_eq!(
        back["idx"].get("299").unwrap().source,
        json!({"tag": "same", "ok": true})
    );
}

/// Text fields carry positions like Elasticsearch's: one per token, and a gap of 100
/// (`position_increment_gap`) after each value of an array, empty ones included.
#[test]
fn text_fields_save_positions() {
    use es_codec::{SegmentInfos, SegmentReader, field_infos::IndexOptions};
    use es_index::{FieldMapping, Index};
    let mut idx = Index::default();
    idx.mappings
        .properties
        .insert("t".into(), FieldMapping::Text { store: false });
    idx.put("1", json!({"t": ["Quick fox fox", "", "lazy fox"]}));
    let root = scratch("positions");
    let dir = root.join("data");
    disk::save(&dir, &Indices::from([("idx".to_owned(), idx)])).unwrap();
    let uuid = &es_codec::cluster_state::read_indices(&dir).unwrap()[0].uuid;
    let shard = dir.join("indices").join(uuid).join("0/index");
    let seg = SegmentInfos::read_latest(&shard)
        .unwrap()
        .segments
        .remove(0);
    let r = SegmentReader::open(&shard, seg).unwrap();
    assert_eq!(
        r.field("t").unwrap().index_options,
        IndexOptions::DocsAndFreqsAndPositions
    );
    let got: Vec<(String, Vec<u32>)> = r
        .terms("t")
        .unwrap()
        .into_iter()
        .map(|t| {
            let ps = t.positions.concat().iter().map(|p| p.position).collect();
            (String::from_utf8(t.term).unwrap(), ps)
        })
        .collect();
    let want = [
        ("fox", vec![1, 2, 204]),
        ("lazy", vec![203]),
        ("quick", vec![0]),
    ];
    let want: Vec<_> = want.map(|(t, p)| (t.to_owned(), p)).into();
    assert_eq!(got, want);
}

/// Object fields save under their dotted paths.
#[test]
fn object_fields_save_under_dotted_paths() {
    use es_codec::{SegmentInfos, SegmentReader};
    use es_index::Index;
    let mut idx = Index::default();
    let doc = json!({"o": {"n": [5, 6]}});
    idx.map_dynamic(&doc);
    idx.put("1", doc);
    let root = scratch("objects");
    let dir = root.join("data");
    disk::save(&dir, &Indices::from([("idx".to_owned(), idx)])).unwrap();
    let uuid = &es_codec::cluster_state::read_indices(&dir).unwrap()[0].uuid;
    let shard = dir.join("indices").join(uuid).join("0/index");
    let seg = SegmentInfos::read_latest(&shard)
        .unwrap()
        .segments
        .remove(0);
    let r = SegmentReader::open(&shard, seg).unwrap();
    assert!(r.field("o.n").is_ok());
    let reopened = disk::open(&dir).unwrap();
    let props = &reopened["idx"].mappings.properties;
    assert_eq!(props.keys().collect::<Vec<_>>(), ["o.n"]);
}

#[test]
fn a_corrupt_segment_fails_open() {
    let root = scratch("corrupt");
    let dir = root.join("data");
    disk::save(&dir, &one_doc(json!({"v": "corrupt-me"}))).unwrap();
    let meta = es_codec::cluster_state::read_indices(&dir).unwrap();
    let shard = dir.join("indices").join(&meta[0].uuid).join("0/index");
    let cfs = fs::read_dir(&shard)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|e| e == "cfs"))
        .unwrap();
    let mut bytes = fs::read(&cfs).unwrap();
    // Breaks the stored _source, which only reading the document would notice otherwise.
    let at = bytes.windows(10).position(|w| w == b"corrupt-me").unwrap();
    bytes[at - 1] = b'x';
    fs::write(&cfs, bytes).unwrap();
    let e = disk::open(&dir).unwrap_err().0;
    assert!(e.contains("checksum"), "{e}");
}

#[test]
fn saving_segments_of_a_sorted_index_fails() {
    // es-mini writes unsorted segments, and Lucene refuses to open those in a sorted index.
    let root = scratch("sorted");
    let dir = root.join("data");
    let mut indices = Indices::new();
    let ix = indices.entry("idx".into()).or_default();
    ix.settings
        .merge(&json!({"index": {"sort": {"field": "n"}}}));
    disk::save(&dir, &indices).unwrap(); // no documents, so no segments
    indices.get_mut("idx").unwrap().put("1", json!({"n": 1}));
    let e = disk::save(&dir, &indices).unwrap_err();
    assert!(e.to_string().contains("index.sort"), "{e}");
}

/// List settings are written as arrays, as Elasticsearch's cluster state stores them.
#[test]
fn list_settings_survive_a_save() {
    let root = scratch("lists");
    let dir = root.join("data");
    let mut indices = Indices::new();
    let list = json!({"index.routing.allocation.include._name": ["a", "b"]});
    indices
        .entry("idx".into())
        .or_default()
        .settings
        .merge(&list);
    disk::save(&dir, &indices).unwrap();
    let settings = disk::open(&dir).unwrap()["idx"].settings.to_json();
    assert_eq!(
        settings["index"]["routing"]["allocation"]["include"]["_name"],
        json!(["a", "b"])
    );
}
