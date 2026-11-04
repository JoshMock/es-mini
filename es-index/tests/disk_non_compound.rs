//! Elasticsearch 9.5.5 data with a non-compound segment (fixtures/generate_non_compound.sh).

use es_index::{Indices, disk};
use std::path::PathBuf;

mod common;
use common::Scratch;

fn ids(indices: &Indices) -> Vec<String> {
    let mut ids: Vec<_> = indices["nocfs"]
        .docs()
        .map(|(id, _)| id.to_string())
        .collect();
    ids.sort();
    ids
}

#[test]
fn opens_and_resaves_an_index_with_a_non_compound_segment() {
    let es =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../fixtures/es-9.5.5-non-compound/data");
    let indices = disk::open(&es).unwrap();
    assert_eq!(ids(&indices), ["1", "2", "4", "5", "6"]);
    assert_eq!(indices["nocfs"].get("5").unwrap().source["views"], 42);
    let dir = Scratch::new("non-compound", Some("data"));
    disk::save(&dir, &indices).unwrap();
    assert_eq!(ids(&disk::open(&dir).unwrap()), ["1", "2", "4", "5", "6"]);
}

#[test]
fn merges_write_non_compound_segments_where_index_compound_format_says() {
    use es_codec::SegmentInfos;
    let dir = Scratch::new("non-compound-merge", Some("data"));
    let mut indices = Indices::new();
    let mut ix = es_index::Index::default();
    ix.settings
        .merge(&serde_json::json!({"index": {"compound_format": "false"}}));
    indices.insert("idx".into(), ix);
    for i in 0..12 {
        let ix = indices.get_mut("idx").unwrap();
        ix.put(&i.to_string(), serde_json::json!({"v": i}));
        disk::save(&dir, &indices).unwrap();
    }
    let uuid = &es_codec::cluster_state::read_indices(&dir).unwrap()[0].uuid;
    let shard = dir.join("indices").join(uuid).join("0/index");
    let infos = SegmentInfos::read_latest(&shard).unwrap();
    // Each save flushes a one-doc segment; merges hold more.
    assert!(infos.segments.iter().any(|s| s.info.max_doc > 1));
    for s in &infos.segments {
        // Flushes stay compound, as Lucene's IndexWriterConfig.useCompoundFile has them.
        assert_eq!(s.info.is_compound, s.info.max_doc == 1);
        // A segment lists every one of its files, as Lucene's SegmentInfo.files does.
        let mut on_disk: Vec<String> = (std::fs::read_dir(&shard).unwrap())
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|f| {
                f.strip_prefix(&s.name)
                    .is_some_and(|r| r.starts_with('.') || r.starts_with("_L"))
            })
            .collect();
        on_disk.sort();
        assert_eq!(on_disk, s.info.files.iter().cloned().collect::<Vec<_>>());
    }
    let back = disk::open(&dir).unwrap();
    assert_eq!(back["idx"].docs().count(), 12);
    assert_eq!(back["idx"].get("3").unwrap().source["v"], 3);
}
