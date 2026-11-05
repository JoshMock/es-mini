//! Writes one segment from in-memory data (inverse of [`crate::SegmentReader`]).

use crate::{
    Result, SegmentCommitInfo, SegmentInfo,
    codec_util::ID_LEN,
    compound::write_compound,
    doc_values::DocValuesWriter,
    field_infos::{DocValuesType, FieldInfo, IndexOptions, write_field_infos},
    norms::NormsWriter,
    output::Output,
    points::PointsWriter,
    stored_fields::{Doc, StoredFieldsWriter},
    terms::{TermPostings, TermsWriter},
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
/// doc's stored fields in order (or the error reading them), so they need not all be in memory
/// at once.
pub fn write_segment(
    dir: &Path,
    s: &NewSegment,
    stored: impl IntoIterator<Item = Result<Doc>>,
) -> Result<SegmentCommitInfo> {
    let numbers =
        (s.terms.iter().map(|t| ("terms", t.0))).chain(s.points.iter().map(|p| ("points", p.0)));
    for (what, n) in numbers {
        if !s.fields.iter().any(|f| f.number == n) {
            return crate::err(format!("{what} for unknown field number {n}"));
        }
    }
    fn of<T>(list: &[(i32, T)], number: i32) -> Option<&T> {
        list.iter().find(|e| e.0 == number).map(|e| &e.1)
    }
    let mut w = SegmentWriter::new(s.name, s.id, s.max_doc);
    for f in s.fields {
        let n = f.number;
        let doc_values = (of(s.numeric, n).map(|v| DocValuesData::Numeric(v)))
            .or_else(|| of(s.sorted_numeric, n).map(|v| DocValuesData::SortedNumeric(v)))
            .or_else(|| of(s.keyword, n).map(|v| DocValuesData::Keyword(v)))
            .or_else(|| of(s.binary, n).map(|v| DocValuesData::Binary(v)));
        let data = FieldData {
            doc_values,
            norms: of(s.norms, n).map(|v| &v[..]),
            terms: of(s.terms, n).map_or(&[], |v| &v[..]),
            points: of(s.points, n).map(|v| &v[..]),
        };
        w.add(f.clone(), data)?;
    }
    w.finish(dir, stored, |_| true)
}

/// One field's doc values, by type, per doc of the segment.
pub enum DocValuesData<'a> {
    /// At most one value per doc
    Numeric(&'a [Vec<i64>]),
    SortedNumeric(&'a [Vec<i64>]),
    /// A keyword field's terms, as SORTED_SET
    Keyword(&'a [Vec<Vec<u8>>]),
    Binary(&'a [Option<Vec<u8>>]),
}

/// One field's data for [`SegmentWriter::add`].
#[derive(Default)]
pub struct FieldData<'a> {
    pub doc_values: Option<DocValuesData<'a>>,
    /// One norm per doc
    pub norms: Option<&'a [Option<i8>]>,
    pub terms: &'a [TermPostings],
    /// Sortable value bytes per doc; a field listed with none is an error.
    pub points: Option<&'a [(u32, Vec<u8>)]>,
}

/// Writes a segment a field at a time, so a caller need only hold one field's data. Each
/// format's encoded bytes stay in memory until [`Self::finish`] writes them.
pub struct SegmentWriter<'a> {
    name: &'a str,
    id: [u8; ID_LEN],
    fields: Vec<FieldInfo>,
    doc_values: DocValuesWriter,
    terms: TermsWriter,
    norms: NormsWriter,
    points: Option<PointsWriter>,
    max_doc: i32,
}

impl<'a> SegmentWriter<'a> {
    /// A segment of `max_doc` docs.
    pub fn new(name: &'a str, id: [u8; ID_LEN], max_doc: i32) -> Self {
        Self {
            name,
            id,
            fields: vec![],
            doc_values: DocValuesWriter::new(&id, "Lucene90_0"),
            terms: TermsWriter::new(&id, "Lucene104_0"),
            norms: NormsWriter::new(&id),
            points: None,
            max_doc,
        }
    }

    /// Adds `field`, which must carry the `PerFieldPostingsFormat.format` = `Lucene104` and
    /// `.suffix` = `0` attributes if indexed, with its data.
    pub fn add(&mut self, field: FieldInfo, data: FieldData) -> Result<()> {
        let terms = data.terms;
        self.add_streamed(field, data, terms.iter().map(Ok))
    }

    /// As [`Self::add`], with the field's terms (in place of `data.terms`) written one at a
    /// time as `terms` yields them, so they need not all be in memory at once.
    pub fn add_streamed<T: std::borrow::Borrow<TermPostings>>(
        &mut self,
        field: FieldInfo,
        data: FieldData,
        terms: impl IntoIterator<Item = Result<T>>,
    ) -> Result<()> {
        let n = field.number;
        match data.doc_values {
            Some(DocValuesData::Numeric(v)) => self.doc_values.numeric(n, v),
            Some(DocValuesData::SortedNumeric(v)) => self.doc_values.sorted_numeric(n, v),
            Some(DocValuesData::Keyword(v)) => self.doc_values.keyword(n, v),
            Some(DocValuesData::Binary(v)) => self.doc_values.binary(n, v),
            None => {}
        }
        if let Some(norms) = data.norms {
            self.norms.add(n, norms);
        }
        let mut terms = terms.into_iter().peekable();
        if terms.peek().is_some() {
            let dense = (data.norms).map(|v| v.iter().map(|x| x.unwrap_or(0)).collect::<Vec<_>>());
            self.terms.add(&field, terms, dense.as_deref())?;
        }
        if let Some(points) = data.points {
            let w = self
                .points
                .get_or_insert_with(|| PointsWriter::new(&self.id, self.max_doc));
            w.add(n, field.point_dimensions as usize, points)?;
        }
        self.fields.push(field);
        Ok(())
    }

    /// Writes the segment, of the `max_doc` docs [`Self::new`] took, into `dir`, as
    /// [`write_segment`]. Like Lucene's `IndexWriter`, it writes each format's files, then puts
    /// them in a compound file if `compound` says so given their total bytes
    /// (`MergePolicy.useCompoundFile`); otherwise they stay, fsynced, as the segment's files.
    pub fn finish(
        self,
        dir: &Path,
        stored: impl IntoIterator<Item = Result<Doc>>,
        compound: impl FnOnce(u64) -> bool,
    ) -> Result<SegmentCommitInfo> {
        let Self {
            name,
            id,
            fields,
            doc_values,
            terms,
            norms,
            points,
            max_doc,
        } = self;
        // Stored fields go to disk a chunk at a time, the other formats' files here, and the
        // compound file copies them from there.
        let mut fdt = StoredFieldsWriter::create(&dir.join(format!("{name}.fdt")), &id)?;
        let mut docs = 0;
        for doc in stored {
            fdt.add(&doc?)?;
            docs += 1;
        }
        let (fdm, fdx) = fdt.finish()?;
        if docs != max_doc {
            return crate::err(format!("{docs} stored docs in a segment of {max_doc}"));
        }
        let mut names = vec![".fdt".to_string()];
        let mut put = |ext: &str, bytes: Vec<u8>| -> Result<()> {
            let path = dir.join(format!("{name}{ext}"));
            std::fs::write(&path, bytes)
                .map_err(|e| crate::Error(format!("{}: {e}", path.display())))?;
            names.push(ext.to_string());
            Ok(())
        };
        put(".fnm", write_field_infos(&fields, &id, ""))?;
        put(".fdm", fdm)?;
        put(".fdx", fdx)?;
        let (dvm, dvd) = doc_values.finish();
        put("_Lucene90_0.dvm", dvm)?;
        put("_Lucene90_0.dvd", dvd)?;
        put("_Lucene90_0.dvs", empty_skip_index(&id))?;
        let t = terms.finish();
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
        let (nvm, nvd) = norms.finish();
        put(".nvm", nvm)?;
        put(".nvd", nvd)?;
        if let Some(points) = points {
            let (m, i, d) = points.finish();
            put(".kdm", m)?;
            put(".kdi", i)?;
            put(".kdd", d)?;
        }
        let path = |ext: &str| dir.join(format!("{name}{ext}"));
        let io = |ext: &str, e: std::io::Error| crate::Error(format!("{name}{ext}: {e}"));
        let mut bytes = 0;
        for ext in &names {
            bytes += std::fs::metadata(path(ext)).map_err(|e| io(ext, e))?.len();
        }
        let is_compound = compound(bytes);
        let files = if is_compound {
            write_compound(dir, name, &id, &names)?;
            vec![".cfs".to_string(), ".cfe".to_string()]
        } else {
            for ext in &names {
                (std::fs::File::open(path(ext)).and_then(|f| f.sync_all()))
                    .map_err(|e| io(ext, e))?;
            }
            names
        };
        Ok(SegmentCommitInfo {
            name: name.into(),
            codec: "Lucene104".into(),
            info: SegmentInfo {
                id,
                version: crate::segment_infos::LUCENE_VERSION,
                min_version: Some(crate::segment_infos::LUCENE_VERSION),
                max_doc,
                is_compound,
                has_blocks: false,
                diagnostics: BTreeMap::from([
                    ("source".into(), "flush".into()),
                    ("lucene.version".into(), "10.5.1".into()),
                ]),
                files: (files.iter().chain([&".si".to_string()]))
                    .map(|ext| format!("{name}{ext}"))
                    .collect(),
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
            stored.iter().cloned().map(Ok),
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
            [Ok(vec![]), Ok(vec![])],
        )
        .unwrap();
        let r = SegmentReader::open(&dir, seg).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(r.terms("t").unwrap(), terms[0].1);
    }

    #[test]
    fn memory_bounds_segment_writer_takes_one_field_at_a_time() {
        let dir = std::env::temp_dir().join(format!("esm-seg-each-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut w = SegmentWriter::new("_0", [9; 16], 2);
        // Formats and doc values types interleave by field, and positions start after a field
        // without them. Each field's data is dropped once added.
        {
            let f = FieldInfo {
                index_options: IndexOptions::Docs,
                doc_values: DocValuesType::SortedSet,
                ..field("k", 0)
            };
            let keyword = [vec![b"b".to_vec()], vec![b"a".to_vec()]];
            let terms = [
                TermPostings::new(b"a".to_vec(), vec![(1, 1)]),
                TermPostings::new(b"b".to_vec(), vec![(0, 1)]),
            ];
            let doc_values = Some(DocValuesData::Keyword(&keyword));
            let terms = &terms[..];
            let data = FieldData {
                doc_values,
                terms,
                ..Default::default()
            };
            w.add(f, data).unwrap();
        }
        {
            let f = FieldInfo {
                omit_norms: false,
                index_options: IndexOptions::DocsAndFreqsAndPositions,
                doc_values: DocValuesType::None,
                ..field("t", 1)
            };
            let at = |position| crate::terms::Position {
                position,
                offsets: None,
                payload: vec![],
            };
            let mut fox = TermPostings::new(b"fox".to_vec(), vec![(0, 1), (1, 2)]);
            fox.positions = vec![vec![at(1)], vec![at(0), at(4)]];
            let norms = [Some(3), Some(5)];
            let terms = &[fox][..];
            let data = FieldData {
                norms: Some(&norms),
                terms,
                ..Default::default()
            };
            w.add(f, data).unwrap();
        }
        {
            let f = FieldInfo {
                index_options: IndexOptions::None,
                doc_values: DocValuesType::SortedNumeric,
                point_dimensions: 1,
                point_index_dimensions: 1,
                point_num_bytes: 8,
                ..field("n", 2)
            };
            let numbers = [vec![7], vec![-2, 9]];
            let b = crate::points::sortable_bytes;
            let points = [(0, b(7, 8)), (1, b(-2, 8)), (1, b(9, 8))];
            let doc_values = Some(DocValuesData::SortedNumeric(&numbers));
            let points = Some(&points[..]);
            let data = FieldData {
                doc_values,
                points,
                ..Default::default()
            };
            w.add(f, data).unwrap();
        }
        let seg = w.finish(&dir, [Ok(vec![]), Ok(vec![])], |_| true).unwrap();
        let r = SegmentReader::open(&dir, seg).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        let DocValues::Ordinals { ords, terms } = r.doc_values("k").unwrap() else {
            panic!("k should be ordinals")
        };
        assert_eq!(
            (terms, ords),
            (vec![b"a".to_vec(), b"b".to_vec()], vec![vec![1], vec![0]])
        );
        assert_eq!(r.terms("k").unwrap().len(), 2);
        let t = r.terms("t").unwrap();
        assert_eq!(t[0].positions.len(), 2);
        assert_eq!(r.norms("t").unwrap(), Some(vec![3, 5]));
        let want = DocValues::Numbers(vec![vec![7], vec![-2, 9]]);
        assert_eq!(r.doc_values("n").unwrap(), want);
        assert_eq!(r.points("n").unwrap().len(), 3);
    }

    #[test]
    fn points_read_back_leaf_by_leaf() {
        let dir = std::env::temp_dir().join(format!("esm-seg-points-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let point = |dims: i32, w: i32, number| FieldInfo {
            point_dimensions: dims,
            point_index_dimensions: dims,
            point_num_bytes: w,
            ..FieldInfo::new(&format!("p{number}"), number)
        };
        let fields = [
            point(1, 8, 0),
            point(2, 8, 1),
            point(1, 4, 2),
            point(1, 2, 3),
        ];
        let b = crate::points::sortable_bytes;
        let n = 1500u32;
        // spread and repeated values (high and low cardinality leaves), two per doc in places,
        // equal values (leaves of one value), and ranges in two dimensions
        let spread: crate::points::PointValues = (0..n)
            .flat_map(|d| {
                let v = i64::from(d) * 7919 % 100_003 - 50_000;
                let extra = (d % 3 == 0).then(|| (d, b(i64::from(d % 5), 8)));
                std::iter::once((d, b(v, 8))).chain(extra)
            })
            .collect();
        let ranges = (0..n)
            .map(|d| {
                (
                    d,
                    [b(i64::from(d), 8), b(i64::from(d % 40) * 1000, 8)].concat(),
                )
            })
            .collect();
        let equal = (0..n).map(|d| (d, b(42, 4))).collect();
        let half = (0..n)
            .step_by(2)
            .map(|d| (d, b(i64::from(d % 9), 2)))
            .collect();
        let points = [(0, spread), (1, ranges), (2, equal), (3, half)];
        let seg = write_segment(
            &dir,
            &NewSegment {
                name: "_0",
                id: [9; 16],
                max_doc: n as i32,
                fields: &fields,
                numeric: &[],
                sorted_numeric: &[],
                keyword: &[],
                binary: &[],
                norms: &[],
                terms: &[],
                points: &points,
            },
            (0..n).map(|_| Ok(vec![])),
        )
        .unwrap();
        let r = SegmentReader::open(&dir, seg).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        for (number, mut want) in points {
            want.sort();
            assert_eq!(r.points(&format!("p{number}")).unwrap(), want, "p{number}");
        }
    }

    #[test]
    fn points_past_the_segment_are_an_error() {
        let dir = std::env::temp_dir().join(format!("esm-seg-points-past-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fields = [FieldInfo {
            point_dimensions: 1,
            point_index_dimensions: 1,
            point_num_bytes: 8,
            ..FieldInfo::new("p", 0)
        }];
        let points = [(
            0,
            vec![
                (0, crate::points::sortable_bytes(1, 8)),
                (5, crate::points::sortable_bytes(2, 8)),
            ],
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
                terms: &[],
                points: &points,
            },
            [Ok(vec![]), Ok(vec![])],
        )
        .unwrap();
        let r = SegmentReader::open(&dir, seg).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(r.point_docs("p", |_| std::cmp::Ordering::Equal).is_err());
        assert!(r.points("p").is_err());
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
