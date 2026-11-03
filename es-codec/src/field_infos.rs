//! Field metadata (`.fnm`, Lucene94FieldInfosFormat).

use crate::{
    CompoundFile, Result, SegmentCommitInfo, codec_util, err, input::DataInput, output::Output,
    segment_infos::radix36,
};
use std::{collections::BTreeMap, path::Path};

#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub enum IndexOptions {
    None = 0,
    Docs = 1,
    DocsAndFreqs = 2,
    DocsAndFreqsAndPositions = 3,
    DocsAndFreqsAndPositionsAndOffsets = 4,
    DocsAndCustomFreqs = 5,
}

impl TryFrom<u8> for IndexOptions {
    type Error = u8;
    fn try_from(b: u8) -> std::result::Result<Self, u8> {
        use IndexOptions::*;
        [
            None,
            Docs,
            DocsAndFreqs,
            DocsAndFreqsAndPositions,
            DocsAndFreqsAndPositionsAndOffsets,
            DocsAndCustomFreqs,
        ]
        .get(b as usize)
        .copied()
        .ok_or(b)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DocValuesType {
    None = 0,
    Numeric = 1,
    Binary = 2,
    Sorted = 3,
    SortedSet = 4,
    SortedNumeric = 5,
}

impl TryFrom<u8> for DocValuesType {
    type Error = u8;
    fn try_from(b: u8) -> std::result::Result<Self, u8> {
        use DocValuesType::*;
        [None, Numeric, Binary, Sorted, SortedSet, SortedNumeric]
            .get(b as usize)
            .copied()
            .ok_or(b)
    }
}

#[derive(Debug, Clone)]
pub struct FieldInfo {
    pub name: String,
    pub number: i32,
    pub store_term_vector: bool,
    pub omit_norms: bool,
    pub store_payloads: bool,
    pub soft_deletes: bool,
    pub parent: bool,
    pub index_options: IndexOptions,
    pub doc_values: DocValuesType,
    pub doc_values_skip_index: bool,
    pub dv_gen: i64,
    pub attributes: BTreeMap<String, String>,
    pub point_dimensions: i32,
    pub point_index_dimensions: i32,
    pub point_num_bytes: i32,
    pub vector_dimension: i32,
}

impl FieldInfo {
    /// A field with no indexing, doc values, points or vectors.
    pub fn new(name: &str, number: i32) -> Self {
        FieldInfo {
            name: name.into(),
            number,
            store_term_vector: false,
            omit_norms: false,
            store_payloads: false,
            soft_deletes: false,
            parent: false,
            index_options: IndexOptions::None,
            doc_values: DocValuesType::None,
            doc_values_skip_index: false,
            dv_gen: -1,
            attributes: BTreeMap::new(),
            point_dimensions: 0,
            point_index_dimensions: 0,
            point_num_bytes: 0,
            vector_dimension: 0,
        }
    }

    /// Records the per-field postings and/or doc values format (suffix `0`) as Lucene's
    /// `PerField*Format` attributes.
    pub fn with_formats(mut self, postings: Option<&str>, doc_values: Option<&str>) -> Self {
        for (kind, format) in [("Postings", postings), ("DocValues", doc_values)] {
            if let Some(f) = format {
                self.attributes
                    .insert(format!("PerField{kind}Format.format"), f.into());
                self.attributes
                    .insert(format!("PerField{kind}Format.suffix"), "0".into());
            }
        }
        self
    }
}

const FORMAT_PARENT_FIELD: i32 = 1;
const FORMAT_DOCVALUE_SKIPPER: i32 = 2;

/// Reads a segment's field infos, from the latest field-infos generation if it was updated.
pub fn read_field_infos(
    dir: &Path,
    seg: &SegmentCommitInfo,
    cfs: &CompoundFile,
) -> Result<Vec<FieldInfo>> {
    let (file, suffix) = if seg.field_infos_gen == -1 {
        if !seg.info.is_compound {
            return err("non-compound segments are not supported yet");
        }
        let file = cfs
            .file(".fnm")?
            .ok_or_else(|| crate::Error(format!("{} has no .fnm", seg.name)))?;
        (file, String::new())
    } else {
        let suffix = radix36(seg.field_infos_gen as u64);
        let file = crate::read_file(&dir.join(format!("{}_{suffix}.fnm", seg.name)))?;
        (file, suffix)
    };
    parse_field_infos(&file, &seg.info.id, &suffix)
}

fn parse_field_infos(file: &[u8], id: &[u8; 16], suffix: &str) -> Result<Vec<FieldInfo>> {
    codec_util::check_footer(file)?;
    let input = &mut DataInput::new(file);
    let format = codec_util::check_index_header(
        input,
        "Lucene94FieldInfos",
        0,
        FORMAT_DOCVALUE_SKIPPER,
        id,
        suffix,
    )?;
    let mut fields = Vec::new();
    for _ in 0..input.read_len()? {
        let name = input.read_string()?;
        let number = input.read_vint()?;
        if number < 0 {
            return err(format!("invalid field number {number} for {name}"));
        }
        let bits = input.read_byte()?;
        if bits & 0xc0 != 0 || (format < FORMAT_PARENT_FIELD && bits & 0xf0 != 0) {
            return err(format!("invalid field bits {bits:#b} for {name}"));
        }
        let index_options = IndexOptions::try_from(input.read_byte()?)
            .or_else(|b| err(format!("invalid index options {b} for {name}")))?;
        let doc_values = DocValuesType::try_from(input.read_byte()?)
            .or_else(|b| err(format!("invalid doc values type {b} for {name}")))?;
        let doc_values_skip_index = format >= FORMAT_DOCVALUE_SKIPPER && input.read_byte()? == 1;
        let dv_gen = input.read_long()?;
        let attributes = input.read_map_of_strings()?;
        let point_dimensions = input.read_vint()?;
        let (point_index_dimensions, point_num_bytes) = match point_dimensions {
            0 => (0, 0),
            _ => (input.read_vint()?, input.read_vint()?),
        };
        let vector_dimension = input.read_vint()?;
        input.read_byte()?; // vector encoding
        input.read_byte()?; // vector similarity
        fields.push(FieldInfo {
            name,
            number,
            store_term_vector: bits & 0x1 != 0,
            omit_norms: bits & 0x2 != 0,
            store_payloads: bits & 0x4 != 0,
            soft_deletes: bits & 0x8 != 0,
            parent: bits & 0x10 != 0,
            index_options,
            doc_values,
            doc_values_skip_index,
            dv_gen,
            attributes,
            point_dimensions,
            point_index_dimensions,
            point_num_bytes,
            vector_dimension,
        });
    }
    Ok(fields)
}

/// Serializes field infos as a `.fnm` file (inverse of [`read_field_infos`]).
pub fn write_field_infos(fields: &[FieldInfo], id: &[u8; 16], suffix: &str) -> Vec<u8> {
    let mut o = Output::default();
    o.index_header("Lucene94FieldInfos", FORMAT_DOCVALUE_SKIPPER, id, suffix);
    o.vint(fields.len() as i32);
    for f in fields {
        o.string(&f.name);
        o.vint(f.number);
        o.byte(
            f.store_term_vector as u8
                | (f.omit_norms as u8) << 1
                | (f.store_payloads as u8) << 2
                | (f.soft_deletes as u8) << 3
                | (f.parent as u8) << 4,
        );
        o.byte(f.index_options as u8);
        o.byte(f.doc_values as u8);
        o.byte(f.doc_values_skip_index as u8);
        o.long(f.dv_gen);
        o.map_of_strings(&f.attributes);
        o.vint(f.point_dimensions);
        if f.point_dimensions != 0 {
            o.vint(f.point_index_dimensions);
            o.vint(f.point_num_bytes);
        }
        o.vint(f.vector_dimension);
        o.byte(1); // vector encoding: FLOAT32, FieldType's default
        o.byte(0); // vector similarity: EUCLIDEAN, FieldType's default
    }
    o.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enum_discriminants_match_lucene_bytes() {
        for b in 0..=5u8 {
            assert_eq!(IndexOptions::try_from(b).unwrap() as u8, b);
            assert_eq!(DocValuesType::try_from(b).unwrap() as u8, b);
        }
        assert_eq!(IndexOptions::try_from(6), Err(6));
        assert_eq!(DocValuesType::try_from(6), Err(6));
    }

    #[test]
    fn write_then_read_round_trips() {
        let field = |name: &str, number, index_options, doc_values| FieldInfo {
            name: name.into(),
            number,
            store_term_vector: false,
            omit_norms: true,
            store_payloads: false,
            soft_deletes: false,
            parent: false,
            index_options,
            doc_values,
            doc_values_skip_index: false,
            dv_gen: -1,
            attributes: BTreeMap::from([("k".into(), "v".into())]),
            point_dimensions: 0,
            point_index_dimensions: 0,
            point_num_bytes: 0,
            vector_dimension: 0,
        };
        let mut with_points = field("p", 2, IndexOptions::None, DocValuesType::SortedNumeric);
        (
            with_points.point_dimensions,
            with_points.point_index_dimensions,
        ) = (1, 1);
        with_points.point_num_bytes = 8;
        let fields = vec![
            field(
                "t",
                0,
                IndexOptions::DocsAndFreqsAndPositions,
                DocValuesType::None,
            ),
            field("s", 1, IndexOptions::Docs, DocValuesType::SortedSet),
            with_points,
        ];
        let id = [7u8; 16];
        let bytes = write_field_infos(&fields, &id, "");
        let back = parse_field_infos(&bytes, &id, "").unwrap();
        assert_eq!(format!("{fields:?}"), format!("{back:?}"));
        assert!(parse_field_infos(&bytes, &[8u8; 16], "").is_err());
    }
}
