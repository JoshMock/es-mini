//! Writes one compound segment from in-memory data (inverse of [`crate::SegmentReader`]).

use crate::{
    Result, SegmentCommitInfo, SegmentInfo,
    codec_util::ID_LEN,
    compound::write_compound,
    doc_values::write_numeric_doc_values,
    field_infos::{FieldInfo, write_field_infos},
    stored_fields::{StoredValue, write_stored_fields},
    terms::{TermPostings, write_terms},
};
use std::{collections::BTreeMap, path::Path};

/// Everything in a new segment. `fields` must carry the `PerFieldPostingsFormat.format` =
/// `Lucene104` and `.suffix` = `0` attributes on indexed fields.
pub struct NewSegment<'a> {
    pub name: &'a str,
    pub id: [u8; ID_LEN],
    pub max_doc: i32,
    pub fields: &'a [FieldInfo],
    pub stored: &'a [Vec<(i32, StoredValue)>],
    /// `(field number, value per doc)`
    pub numeric: &'a [(i32, Vec<i64>)],
    /// `(field number, terms)`
    pub terms: &'a [(i32, Vec<TermPostings>)],
}

/// Writes the segment's compound file into `dir` and returns its commit info; the caller adds it
/// to a [`crate::SegmentInfos`] and writes that (which writes the `.si`).
pub fn write_segment(dir: &Path, s: &NewSegment) -> Result<SegmentCommitInfo> {
    let (fdm, fdt) = write_stored_fields(s.stored, &s.id)?;
    let (dvm, dvd) = write_numeric_doc_values(s.numeric, &s.id, "Lucene90_0");
    let by_number = |n: &i32| {
        s.fields
            .iter()
            .find(|f| f.number == *n)
            .expect("known field")
    };
    let term_fields: Vec<_> = s
        .terms
        .iter()
        .map(|(n, t)| (by_number(n), &t[..]))
        .collect();
    let t = write_terms(&term_fields, &s.id, "Lucene104_0");
    let mut files = vec![
        (".fnm".to_string(), write_field_infos(s.fields, &s.id, "")),
        (".fdm".into(), fdm),
        (".fdt".into(), fdt),
        ("_Lucene90_0.dvm".into(), dvm),
        ("_Lucene90_0.dvd".into(), dvd),
    ];
    for (ext, bytes) in [
        ("tim", t.tim),
        ("tip", t.tip),
        ("tmd", t.tmd),
        ("doc", t.doc),
    ] {
        files.push((format!("_Lucene104_0.{ext}"), bytes));
    }
    write_compound(dir, s.name, &s.id, &files)?;
    Ok(SegmentCommitInfo {
        name: s.name.into(),
        codec: "Lucene104".into(),
        info: SegmentInfo {
            id: s.id,
            version: (10, 0, 0),
            min_version: Some((10, 0, 0)),
            max_doc: s.max_doc,
            is_compound: true,
            has_blocks: false,
            diagnostics: BTreeMap::new(),
            files: ["cfs", "cfe"].map(|e| format!("{}.{e}", s.name)).into(),
            attributes: BTreeMap::new(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        SegmentInfos, SegmentReader,
        doc_values::DocValues,
        field_infos::{DocValuesType, IndexOptions},
    };

    fn field(name: &str, number: i32) -> FieldInfo {
        let attributes = [("format", "Lucene104"), ("suffix", "0")]
            .map(|(k, v)| (format!("PerFieldPostingsFormat.{k}"), v.to_string()));
        FieldInfo {
            name: name.into(),
            number,
            store_term_vector: false,
            omit_norms: true,
            store_payloads: false,
            soft_deletes: false,
            parent: false,
            index_options: IndexOptions::DocsAndFreqs,
            doc_values: DocValuesType::Numeric,
            doc_values_skip_index: false,
            dv_gen: -1,
            attributes: attributes.into(),
            point_dimensions: 0,
            point_index_dimensions: 0,
            point_num_bytes: 0,
            vector_dimension: 0,
        }
    }

    #[test]
    fn written_segment_reads_back() {
        let dir = std::env::temp_dir().join(format!("esm-seg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fields = [field("title", 0), field("n", 1)];
        let stored: Vec<_> = (0..5)
            .map(|i| vec![(0, StoredValue::String(format!("doc {i}")))])
            .collect();
        let numeric = [(1, vec![10, 20, 30, 40, 50])];
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
                stored: &stored,
                numeric: &numeric,
                terms: &terms,
            },
        )
        .unwrap();
        let infos = SegmentInfos {
            id: [8; 16],
            generation: 1,
            lucene_version: (10, 0, 0),
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
        assert_eq!(r.stored_fields().unwrap(), stored);
        assert_eq!(r.terms("title").unwrap(), terms[0].1);
        let want = DocValues::Numbers(numeric[0].1.iter().map(|v| vec![*v]).collect());
        assert_eq!(r.doc_values("n").unwrap(), want);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
