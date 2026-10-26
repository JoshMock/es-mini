//! Decodes the Elasticsearch 9.5.5 fixture (fixtures/generate.sh).

use es_codec::{
    CompoundFile, SegmentInfos, SegmentReader, doc_values::DocValues, input::DataInput,
    stored_fields::StoredValue,
};
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
    let fnm = cfs.file(".fnm").unwrap().unwrap();
    // Every Lucene file starts with the codec magic and ends with a valid checksum footer.
    assert_eq!(DataInput::new(&fnm).read_be_int().unwrap(), 0x3fd7_6c17);
    es_codec::codec_util::check_footer(&fnm).unwrap();
    assert!(cfs.file(".nope").unwrap().is_none());
}

#[test]
fn rejects_corrupt_checksums() {
    let dir = shard_index_dir();
    let mut bytes = std::fs::read(dir.join("_0.si")).unwrap();
    es_codec::codec_util::check_footer(&bytes).unwrap();
    bytes[20] ^= 1;
    assert!(es_codec::codec_util::check_footer(&bytes).is_err());
}

/// Every segment of the fixture's latest commit, opened.
fn segments() -> Vec<SegmentReader> {
    let dir = shard_index_dir();
    SegmentInfos::read_latest(&dir)
        .unwrap()
        .segments
        .into_iter()
        .map(|seg| SegmentReader::open(&dir, seg).unwrap())
        .collect()
}

#[test]
fn reads_field_infos_including_updated_generation() {
    use es_codec::field_infos::{DocValuesType, IndexOptions};
    for seg in &segments() {
        let fields = &seg.fields;
        let get = |n: &str| seg.field(n).unwrap();
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
fn stored_docs(seg: &SegmentReader) -> Vec<(Vec<u8>, Option<String>)> {
    let name = |n: i32| &seg.field_by_number(n).unwrap().name;
    seg.stored_fields()
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
    let docs: Vec<_> = segments().iter().map(stored_docs).collect();
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

#[test]
fn reads_single_stored_documents() {
    for seg in segments() {
        let all = seg.stored_fields().unwrap();
        for (doc, fields) in all.iter().enumerate() {
            assert_eq!(&seg.document(doc).unwrap(), fields);
        }
        assert!(seg.document(all.len()).is_err());
    }
}

#[test]
fn reads_live_docs_from_soft_deletes() {
    let segments = segments();
    let live_ids = |seg: &SegmentReader| -> Vec<String> {
        let live = seg.live_docs().unwrap();
        let ids: Vec<_> = stored_docs(seg)
            .iter()
            .map(|(id, _)| decode_numeric_id(id))
            .collect();
        assert_eq!(live.len(), ids.len());
        ids.into_iter()
            .zip(live)
            .filter(|(_, l)| *l)
            .map(|(id, _)| id)
            .collect()
    };
    // Doc 2 is superseded and doc 5 deleted; _1 holds the new doc 2, doc 6 and doc 5's tombstone.
    assert_eq!(live_ids(&segments[0]), ["1", "3", "4"]);
    assert_eq!(live_ids(&segments[1]), ["6", "2"]);
}

/// `(term, [(doc, freq)])` for a field of a segment.
fn terms_of(seg: &SegmentReader, field: &str) -> Vec<(String, Vec<(u32, u32)>)> {
    seg.terms(field)
        .unwrap()
        .into_iter()
        .map(|t| (String::from_utf8(t.term).unwrap(), t.docs))
        .collect()
}

fn postings(terms: &[(String, Vec<(u32, u32)>)], term: &str) -> Vec<(u32, u32)> {
    terms.iter().find(|(t, _)| t == term).unwrap().1.clone()
}

#[test]
fn reads_terms_and_postings() {
    let segments = segments();
    let (s0, s1) = (&segments[0], &segments[1]);

    let tag = terms_of(s0, "tag");
    let names: Vec<_> = tag.iter().map(|(t, _)| t.as_str()).collect();
    assert_eq!(names, ["animal", "misc", "pet"]);
    assert_eq!(postings(&tag, "animal"), [(0, 1), (1, 1)]);
    assert_eq!(postings(&tag, "misc"), [(4, 1)]);
    assert_eq!(postings(&tag, "pet"), [(2, 1), (3, 1)]);

    let title = terms_of(s0, "title");
    let names: Vec<_> = title.iter().map(|(t, _)| t.as_str()).collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(names, sorted, "terms come out in byte order");
    assert_eq!(
        names,
        [
            "a",
            "and",
            "brown",
            "café",
            "dog",
            "fox",
            "lazy",
            "naïve",
            "quick",
            "rabbits",
            "sleeps",
            "the",
            "ünïcödé",
            "京",
            "東"
        ]
    );
    assert_eq!(postings(&title, "brown"), [(0, 1), (3, 1)]);
    assert_eq!(postings(&title, "a"), [(3, 2)]);
    assert_eq!(postings(&title, "quick"), [(0, 1), (1, 2)]);

    let tag = terms_of(s1, "tag");
    assert_eq!(postings(&tag, "animal"), [(1, 1)]);
    assert_eq!(postings(&tag, "news"), [(0, 1)]);
    let title = terms_of(s1, "title");
    assert_eq!(postings(&title, "fox"), [(0, 1)]);
    assert_eq!(postings(&title, "quick"), [(1, 2)]);
}

fn numbers(seg: &SegmentReader, field: &str) -> Vec<Vec<i64>> {
    match seg.doc_values(field).unwrap() {
        DocValues::Numbers(n) => n,
        other => panic!("{other:?}"),
    }
}

/// `NumericUtils.sortableLongToDouble`.
fn sortable_double(bits: i64) -> f64 {
    f64::from_bits((bits ^ ((bits >> 63) & i64::MAX)) as u64)
}

fn millis(date: &str) -> i64 {
    let ymd: Vec<i64> = date.split('-').map(|p| p.parse().unwrap()).collect();
    // days from civil (Howard Hinnant)
    let (y, m, d) = (ymd[0] - (ymd[1] <= 2) as i64, ymd[1], ymd[2]);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    (era * 146_097 + doe - 719_468) * 86_400_000
}

#[test]
fn reads_numeric_doc_values() {
    let segments = segments();
    let (s0, s1) = (&segments[0], &segments[1]);
    let flat = |v: Vec<Vec<i64>>| -> Vec<i64> { v.into_iter().flatten().collect() };

    assert_eq!(flat(numbers(s0, "views")), [10, 25, 3, 7, 0]);
    // The tombstone (doc 2 of _1) has no field values.
    assert_eq!(numbers(s1, "views"), [vec![42], vec![26], vec![]]);
    let ratings: Vec<f64> = flat(numbers(s0, "rating"))
        .into_iter()
        .map(sortable_double)
        .collect();
    assert_eq!(ratings, [4.5, 3.0, 2.5, 5.0, 1.0]);
    assert_eq!(flat(numbers(s0, "published")), [1, 0, 1, 1, 0]);
    let dates = [
        "2024-01-01",
        "2024-02-15",
        "2023-12-31",
        "2024-03-10",
        "2024-04-01",
    ];
    assert_eq!(flat(numbers(s0, "date")), dates.map(millis));
}

#[test]
fn reads_sorted_set_doc_values() {
    let segments = segments();
    let tags = |s: usize| -> Vec<Vec<String>> {
        let DocValues::Ordinals { ords, terms } = segments[s].doc_values("tag").unwrap() else {
            panic!("not sorted")
        };
        ords.into_iter()
            .map(|d| {
                d.into_iter()
                    .map(|o| String::from_utf8(terms[o].clone()).unwrap())
                    .collect()
            })
            .collect()
    };
    let one = |t: &str| vec![t.to_string()];
    assert_eq!(
        tags(0),
        [
            one("animal"),
            one("animal"),
            one("pet"),
            one("pet"),
            one("misc")
        ]
    );
    assert_eq!(tags(1), [one("news"), one("animal"), vec![]]);
}

#[test]
fn reads_norms() {
    let segs = segments();
    // Field lengths of the titles of docs 1-5 (Lucene's SmallFloat bytes equal small lengths).
    assert_eq!(segs[0].norms("title").unwrap(), Some(vec![4, 3, 3, 6, 5]));
    // sparse: the delete tombstone has no title
    assert_eq!(segs[1].norms("title").unwrap(), Some(vec![3, 3, 0]));
    // keyword fields omit norms
    assert_eq!(segs[0].norms("tag").unwrap(), None);
}
