//! Decodes the Elasticsearch 9.5.5 fixture (fixtures/generate.sh).

use es_codec::{CompoundFile, SegmentInfos, input::DataInput};
use std::path::PathBuf;

fn shard_index_dir() -> PathBuf {
    let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../fixtures");
    let uuid = std::fs::read_to_string(fixtures.join("expected/index_uuid")).unwrap();
    fixtures
        .join("es-9.5.5/data/indices")
        .join(uuid.trim())
        .join("0/index")
}

#[test]
fn reads_latest_commit() {
    let infos = SegmentInfos::read_latest(&shard_index_dir()).unwrap();
    assert_eq!(infos.lucene_version.0, 10);
    assert_eq!(infos.index_created_major, 10);
    let names: Vec<&str> = infos.segments.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["_0", "_1"]);
    assert!(infos.segments.iter().all(|s| s.codec == "Lucene104"));
    // ES commit metadata
    assert!(infos.user_data.contains_key("history_uuid"));
    assert!(infos.user_data.contains_key("translog_uuid"));
    assert_eq!(infos.user_data["max_seq_no"], "7");
}

#[test]
fn reads_segment_infos() {
    let infos = SegmentInfos::read_latest(&shard_index_dir()).unwrap();
    let s0 = &infos.segments[0];
    // Docs 1-5 from the first refresh; doc 2 (updated) and doc 5 (deleted) are soft-deleted later.
    assert_eq!(s0.info.max_doc, 5);
    assert_eq!(s0.soft_del_count, 2);
    assert_eq!(s0.del_count, 0);
    assert!(s0.info.is_compound);
    assert_eq!(s0.info.version.0, 10);
    assert!(s0.info.files.iter().any(|f| f == "_0.cfs"));
    // Soft deletes are written as a doc-values update generation for the segment.
    assert_eq!(s0.field_infos_gen, 1);
    assert!(s0.field_infos_files.contains("_0_1.fnm"));
    // Doc 6, the new version of doc 2, and the delete tombstone for doc 5 (itself soft-deleted).
    let s1 = &infos.segments[1];
    assert_eq!((s1.info.max_doc, s1.soft_del_count), (3, 1));
}

#[test]
fn opens_compound_files() {
    let dir = shard_index_dir();
    let infos = SegmentInfos::read_latest(&dir).unwrap();
    let s0 = &infos.segments[0];
    let cfs = CompoundFile::open(&dir, &s0.name, &s0.info.id).unwrap();
    let fnm = cfs.file(".fnm").unwrap();
    // Every Lucene file starts with the codec magic and ends with a valid checksum footer.
    assert_eq!(DataInput::new(fnm).read_be_int().unwrap(), 0x3fd7_6c17);
    es_codec::codec_util::check_footer(fnm).unwrap();
    assert!(cfs.file(".nope").is_none());
}

#[test]
fn rejects_corrupt_checksums() {
    let dir = shard_index_dir();
    let mut bytes = std::fs::read(dir.join("_0.si")).unwrap();
    es_codec::codec_util::check_footer(&bytes).unwrap();
    bytes[20] ^= 1;
    assert!(es_codec::codec_util::check_footer(&bytes).is_err());
}

#[test]
fn reads_field_infos_including_updated_generation() {
    use es_codec::field_infos::{DocValuesType, IndexOptions, read_field_infos};
    let dir = shard_index_dir();
    let infos = SegmentInfos::read_latest(&dir).unwrap();
    for seg in &infos.segments {
        let cfs = CompoundFile::open(&dir, &seg.name, &seg.info.id).unwrap();
        let fields = read_field_infos(&dir, seg, &cfs).unwrap();
        let get = |n: &str| {
            fields
                .iter()
                .find(|f| f.name == n)
                .unwrap_or_else(|| panic!("{} lacks {n}", seg.name))
        };
        let title = get("title");
        assert_eq!(title.index_options, IndexOptions::DocsAndFreqsAndPositions);
        assert!(!title.omit_norms);
        let tag = get("tag");
        assert_eq!(
            (tag.index_options, tag.doc_values),
            (IndexOptions::Docs, DocValuesType::SortedSet)
        );
        let views = get("views");
        assert_eq!(views.doc_values, DocValuesType::SortedNumeric);
        assert_eq!((views.point_dimensions, views.point_num_bytes), (1, 8));
        assert_eq!(get("_source").index_options, IndexOptions::None);
        assert!(get("__soft_deletes").soft_deletes);
        let mut numbers: Vec<i32> = fields.iter().map(|f| f.number).collect();
        numbers.dedup();
        assert_eq!(numbers.len(), fields.len(), "field numbers are unique");
    }
}

/// `(_id, _source)` per doc id; the delete tombstone has no `_source`.
fn stored_docs(seg: &es_codec::SegmentCommitInfo) -> Vec<(Vec<u8>, Option<String>)> {
    use es_codec::{field_infos::read_field_infos, stored_fields::*};
    let dir = shard_index_dir();
    let cfs = CompoundFile::open(&dir, &seg.name, &seg.info.id).unwrap();
    let names = read_field_infos(&dir, seg, &cfs).unwrap();
    let name = |n: i32| names.iter().find(|f| f.number == n).unwrap().name.clone();
    read_stored_fields(&cfs, seg)
        .unwrap()
        .into_iter()
        .map(|doc| {
            let bytes = |f: &str| match doc.iter().find(|(n, _)| name(*n) == f) {
                Some((_, StoredValue::Bytes(b))) => Some(b.clone()),
                Some(other) => panic!("{f}: {other:?}"),
                None => None,
            };
            let src = bytes("_source").map(|b| String::from_utf8(b).unwrap());
            (bytes("_id").unwrap(), src)
        })
        .collect()
}

/// ES `Uid.encodeId`: digit-only ids are `0xfe` then packed decimal nibbles, `0xf` padding.
fn decode_numeric_id(b: &[u8]) -> String {
    assert_eq!(b[0], 0xfe);
    b[1..]
        .iter()
        .flat_map(|x| [x >> 4, x & 0xf])
        .take_while(|n| *n != 0xf)
        .map(|n| char::from(b'0' + n))
        .collect()
}

fn title(source: &str) -> &str {
    source
        .split("\"title\":\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap()
}

#[test]
fn reads_stored_fields() {
    let infos = SegmentInfos::read_latest(&shard_index_dir()).unwrap();
    let docs: Vec<_> = infos.segments.iter().map(stored_docs).collect();
    let ids = |s: usize| -> Vec<String> {
        docs[s]
            .iter()
            .map(|(id, _)| decode_numeric_id(id))
            .collect()
    };
    assert_eq!(ids(0), ["1", "2", "3", "4", "5"]);
    // Doc 2 again (the update), then the tombstone for the deleted doc 5.
    assert_eq!(ids(1), ["6", "2", "5"]);
    let titles = |s: usize| -> Vec<Option<&str>> {
        docs[s]
            .iter()
            .map(|(_, src)| src.as_deref().map(title))
            .collect()
    };
    assert_eq!(titles(0)[0], Some("The quick brown fox"));
    assert_eq!(titles(0)[4], Some("Ünïcödé café naïve 東京"));
    assert_eq!(
        titles(1),
        [Some("Fresh fox news"), Some("Quick quick rabbits"), None]
    );
    assert!(docs[1][1].1.as_ref().unwrap().contains("\"views\":26"));
}
