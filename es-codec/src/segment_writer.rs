//! Writes one compound segment from in-memory data (inverse of [`crate::SegmentReader`]).

use crate::{
    Result, SegmentCommitInfo, SegmentInfo,
    codec_util::ID_LEN,
    compound::write_compound,
    doc_values::write_doc_values_with,
    field_infos::{DocValuesType, FieldInfo, IndexOptions, write_field_infos},
    norms::write_norms,
    output::Output,
    stored_fields::{Doc, StoredFieldsWriter},
    terms::{TermPostings, write_terms},
};
use std::{collections::BTreeMap, path::Path};

/// Elasticsearch opens `.dvs` for every doc values file set, even when no field has a skip index.
fn empty_skip_index(id: &[u8; ID_LEN]) -> Vec<u8> {
    let mut o = Output::default();
    o.index_header("Lucene90DocValuesSkipIndex", 2, id, "Lucene90_0");
    o.finish()
}

/// Everything in a new segment. `fields` must carry the `PerFieldPostingsFormat.format` =
/// `Lucene104` and `.suffix` = `0` attributes on indexed fields.
pub struct NewSegment<'a> {
    pub name: &'a str,
    pub id: [u8; ID_LEN],
    pub max_doc: i32,
    pub fields: &'a [FieldInfo],
    /// `(field number, values per doc)`, at most one per doc
    pub numeric: &'a [(i32, Vec<Vec<i64>>)],
    /// `(field number, one value per doc)` for single-valued SORTED_NUMERIC fields
    pub sorted_numeric: &'a [(i32, Vec<Vec<i64>>)],
    /// `(field number, terms per doc)` for keyword fields
    pub keyword: &'a [(i32, Vec<Vec<Vec<u8>>>)],
    /// `(field number, value per doc)` for BINARY fields
    pub binary: &'a [(i32, Vec<Option<Vec<u8>>>)],
    /// `(field number, one norm per doc)`
    pub norms: &'a [(i32, Vec<Option<i8>>)],
    /// `(field number, terms)`
    pub terms: &'a [(i32, Vec<TermPostings>)],
    /// `(field number, sortable value bytes per doc)`
    pub points: &'a [(i32, crate::points::PointValues)],
}

/// Writes the segment's compound file into `dir` and returns its commit info; the caller adds it
/// to a [`crate::SegmentInfos`] and writes that (which writes the `.si`). `stored` yields each
/// doc's stored fields in order, so they need not all be in memory at once.
pub fn write_segment(
    dir: &Path,
    s: &NewSegment,
    stored: impl IntoIterator<Item = Doc>,
) -> Result<SegmentCommitInfo> {
    // Each sub-file goes to disk as soon as it is encoded (stored fields a chunk at a time), and
    // the compound file copies them from there, so at most one format's files are in memory.
    let mut fdt = StoredFieldsWriter::create(&dir.join(format!("{}.fdt", s.name)), &s.id)?;
    let mut docs = 0;
    for doc in stored {
        fdt.add(&doc)?;
        docs += 1;
    }
    let (fdm, fdx) = fdt.finish()?;
    if docs != s.max_doc {
        return crate::err(format!("{docs} stored docs in a segment of {}", s.max_doc));
    }
    let mut names = vec![".fdt".to_string()];
    let mut put = |name: &str, bytes: Vec<u8>| -> Result<()> {
        let path = dir.join(format!("{}{name}", s.name));
        std::fs::write(&path, bytes)
            .map_err(|e| crate::Error(format!("{}: {e}", path.display())))?;
        names.push(name.to_string());
        Ok(())
    };
    put(".fnm", write_field_infos(s.fields, &s.id, ""))?;
    put(".fdm", fdm)?;
    put(".fdx", fdx)?;
    let (dvm, dvd) = write_doc_values_with(
        s.numeric,
        s.sorted_numeric,
        s.keyword,
        s.binary,
        &s.id,
        "Lucene90_0",
    );
    put("_Lucene90_0.dvm", dvm)?;
    put("_Lucene90_0.dvd", dvd)?;
    put("_Lucene90_0.dvs", empty_skip_index(&s.id))?;
    let term_fields = s
        .terms
        .iter()
        .map(|(n, t)| match s.fields.iter().find(|f| f.number == *n) {
            Some(f) => Ok((f, &t[..])),
            None => crate::err(format!("terms for unknown field number {n}")),
        })
        .collect::<Result<Vec<_>>>()?;
    let dense: Vec<(i32, Vec<i8>)> = s
        .norms
        .iter()
        .map(|(n, v)| (*n, v.iter().map(|x| x.unwrap_or(0)).collect()))
        .collect();
    let dense: Vec<_> = dense.iter().map(|(n, v)| (*n, &v[..])).collect();
    let t = write_terms(&term_fields, &s.id, "Lucene104_0", &dense)?;
    for (ext, bytes) in [
        ("tim", t.tim),
        ("tip", t.tip),
        ("tmd", t.tmd),
        ("doc", t.doc),
        ("psm", t.psm),
    ] {
        put(&format!("_Lucene104_0.{ext}"), bytes)?;
    }
    if !t.pos.is_empty() {
        put("_Lucene104_0.pos", t.pos)?;
    }
    let sparse: Vec<_> = s.norms.iter().map(|(n, v)| (*n, &v[..])).collect();
    let (nvm, nvd) = write_norms(&s.id, &sparse);
    put(".nvm", nvm)?;
    put(".nvd", nvd)?;
    if !s.points.is_empty() {
        let points = (s.points.iter())
            .map(|(n, p)| match s.fields.iter().find(|f| f.number == *n) {
                Some(f) => Ok((*n, f.point_dimensions as usize, p.clone())),
                None => crate::err(format!("points for unknown field number {n}")),
            })
            .collect::<Result<Vec<_>>>()?;
        let (m, i, d) = crate::points::write_points(&points, &s.id)?;
        put(".kdm", m)?;
        put(".kdi", i)?;
        put(".kdd", d)?;
    }

    write_compound(dir, s.name, &s.id, &names)?;
    Ok(SegmentCommitInfo {
        name: s.name.into(),
        codec: "Lucene104".into(),
        info: SegmentInfo {
            id: s.id,
            version: crate::segment_infos::LUCENE_VERSION,
            min_version: Some(crate::segment_infos::LUCENE_VERSION),
            max_doc: s.max_doc,
            is_compound: true,
            has_blocks: false,
            diagnostics: BTreeMap::from([
                ("source".into(), "flush".into()),
                ("lucene.version".into(), "10.5.1".into()),
            ]),
            files: ["cfs", "cfe", "si"]
                .map(|e| format!("{}.{e}", s.name))
                .into(),
            attributes: BTreeMap::from([(
                "Lucene90StoredFieldsFormat.mode".into(),
                "BEST_SPEED".into(),
            )]),
            index_sort: vec![],
        },
        del_gen: -1,
        del_count: 0,
        field_infos_gen: -1,
        dv_gen: -1,
        soft_del_count: 0,
        field_infos_files: Default::default(),
        dv_update_files: Default::default(),
    })
}

/// The metadata fields every ES 9.5.5 shard segment has, numbered 0..=5 in this order: `_id`,
/// `_source`, `_version`, `_seq_no`, `_primary_term`, `__soft_deletes`. `_id` uses the
/// `Lucene104` postings format (the reader keys off the attribute), not ES's bloom filter.
pub fn es_meta_fields() -> Vec<FieldInfo> {
    let base = FieldInfo::new;
    let numeric = |name: &str, number: i32| FieldInfo {
        doc_values: DocValuesType::Numeric,
        ..base(name, number).with_formats(None, Some("Lucene90"))
    };
    vec![
        FieldInfo {
            omit_norms: true,
            index_options: IndexOptions::Docs,
            ..base("_id", 0).with_formats(Some("Lucene104"), None)
        },
        base("_source", 1),
        numeric("_version", 2),
        FieldInfo {
            point_dimensions: 1,
            point_index_dimensions: 1,
            point_num_bytes: 8,
            ..numeric("_seq_no", 3)
        },
        numeric("_primary_term", 4),
        FieldInfo {
            soft_deletes: true,
            ..numeric("__soft_deletes", 5)
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        SegmentInfos, SegmentReader,
        doc_values::DocValues,
        field_infos::{DocValuesType, IndexOptions},
        stored_fields::StoredValue,
    };

    fn field(name: &str, number: i32) -> FieldInfo {
        FieldInfo {
            omit_norms: true,
            index_options: IndexOptions::DocsAndFreqs,
            doc_values: DocValuesType::Numeric,
            ..FieldInfo::new(name, number).with_formats(Some("Lucene104"), None)
        }
    }

    #[test]
    fn written_segment_reads_back() {
        let dir = std::env::temp_dir().join(format!("esm-seg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fields = [field("title", 0), field("n", 1), field("tag", 2)];
        let stored: Vec<_> = (0..5)
            .map(|i| vec![(0, StoredValue::String(format!("doc {i}")))])
            .collect();
        let numeric = [(1, vec![vec![10], vec![20], vec![30], vec![40], vec![50]])];
        let keyword = [(
            2,
            (0..5)
                .map(|i| vec![format!("k{}", i % 2).into_bytes()])
                .collect(),
        )];
        let terms = [(
            0,
            vec![TermPostings {
                term: b"doc".to_vec(),
                total_term_freq: 5,
                docs: (0..5).map(|d| (d, 1)).collect(),
                positions: vec![],
            }],
        )];
        let seg = write_segment(
            &dir,
            &NewSegment {
                name: "_0",
                id: [9; 16],
                max_doc: 5,
                fields: &fields,
                numeric: &numeric,
                sorted_numeric: &[],
                keyword: &keyword,
                binary: &[],
                norms: &[],
                terms: &terms,
                points: &[],
            },
            stored.clone(),
        )
        .unwrap();
        let infos = SegmentInfos {
            id: [8; 16],
            generation: 1,
            lucene_version: crate::segment_infos::LUCENE_VERSION,
            index_created_major: 10,
            version: 1,
            counter: 1,
            segments: vec![seg],
            user_data: BTreeMap::new(),
        };
        infos.write(&dir).unwrap();
        let read = SegmentInfos::read_latest(&dir).unwrap();
        let r = SegmentReader::open(&dir, read.segments[0].clone()).unwrap();
        assert_eq!(r.max_doc(), 5);
        let info = &read.segments[0].info;
        assert_eq!(info.version, (10, 5, 1));
        assert_eq!(info.diagnostics["source"], "flush");
        assert_eq!(info.diagnostics["lucene.version"], "10.5.1");
        assert_eq!(r.stored_fields().unwrap(), stored);
        assert_eq!(r.terms("title").unwrap(), terms[0].1);
        let want = DocValues::Numbers(numeric[0].1.clone());
        assert_eq!(r.doc_values("n").unwrap(), want);
        let DocValues::Ordinals { ords, terms } = r.doc_values("tag").unwrap() else {
            panic!("tag should be ordinals")
        };
        assert_eq!(terms, [b"k0".to_vec(), b"k1".to_vec()]);
        assert_eq!(ords, [[0], [1], [0], [1], [0]]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn positions_read_back() {
        let dir = std::env::temp_dir().join(format!("esm-seg-pos-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fields = [FieldInfo {
            index_options: IndexOptions::DocsAndFreqsAndPositions,
            ..field("t", 0)
        }];
        let at = |position| crate::terms::Position {
            position,
            offsets: None,
            payload: vec![],
        };
        let terms = [(
            0,
            vec![TermPostings {
                term: b"fox".to_vec(),
                total_term_freq: 3,
                docs: vec![(0, 1), (1, 2)],
                positions: vec![vec![at(1)], vec![at(0), at(4)]],
            }],
        )];
        let seg = write_segment(
            &dir,
            &NewSegment {
                name: "_0",
                id: [9; 16],
                max_doc: 2,
                fields: &fields,
                numeric: &[],
                sorted_numeric: &[],
                keyword: &[],
                binary: &[],
                norms: &[],
                terms: &terms,
                points: &[],
            },
            [vec![], vec![]],
        )
        .unwrap();
        let r = SegmentReader::open(&dir, seg).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(r.terms("t").unwrap(), terms[0].1);
    }

    #[test]
    fn unwritable_input_is_an_error_not_a_panic() {
        let fields = [field("n", 0)];
        let write = |terms: &[(i32, Vec<TermPostings>)], points: &[(i32, _)]| {
            write_segment(
                Path::new("/nonexistent"),
                &NewSegment {
                    name: "_0",
                    id: [9; 16],
                    max_doc: 0,
                    fields: &fields,
                    numeric: &[],
                    sorted_numeric: &[],
                    keyword: &[],
                    binary: &[],
                    norms: &[],
                    terms,
                    points,
                },
                [],
            )
        };
        assert!(write(&[], &[(0, vec![])]).is_err(), "field without points");
        assert!(
            write(&[(7, vec![])], &[]).is_err(),
            "terms of an unknown field"
        );
    }

    #[test]
    fn meta_fields_match_the_es_fixture() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../fixtures/es-9.5.5/data/indices");
        let mut checked = 0;
        for e in std::fs::read_dir(dir).unwrap() {
            let idx = e.unwrap().path().join("0/index");
            let si = SegmentInfos::read_latest(&idx).unwrap();
            let seg = SegmentReader::open(&idx, si.segments[0].clone()).unwrap();
            for ours in es_meta_fields() {
                let Ok(real) = seg.field(&ours.name) else {
                    continue; // only one fixture index has soft deletes
                };
                let key = |f: &FieldInfo| {
                    (
                        f.omit_norms,
                        f.soft_deletes,
                        f.index_options,
                        f.doc_values,
                        f.point_dimensions,
                        f.point_num_bytes,
                    )
                };
                assert_eq!(key(real), key(&ours), "{}", ours.name);
                checked += 1;
            }
        }
        assert!(checked >= 11);
    }
}
