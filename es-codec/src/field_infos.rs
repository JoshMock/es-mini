//! Field metadata (`.fnm`, Lucene94FieldInfosFormat).

use crate::{
    CompoundFile, Result, SegmentCommitInfo, codec_util, err, input::DataInput,
    segment_infos::radix36,
};
use std::{collections::BTreeMap, path::Path};

#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub enum IndexOptions {
    None,
    Docs,
    DocsAndFreqs,
    DocsAndFreqsAndPositions,
    DocsAndFreqsAndPositionsAndOffsets,
    DocsAndCustomFreqs,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DocValuesType {
    None,
    Numeric,
    Binary,
    Sorted,
    SortedSet,
    SortedNumeric,
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

const FORMAT_PARENT_FIELD: i32 = 1;
const FORMAT_DOCVALUE_SKIPPER: i32 = 2;

/// Reads a segment's field infos, from the latest field-infos generation if it was updated.
pub fn read_field_infos(
    dir: &Path,
    seg: &SegmentCommitInfo,
    cfs: &CompoundFile,
) -> Result<Vec<FieldInfo>> {
    let owned;
    let (file, suffix) = match seg.field_infos_gen {
        -1 => match seg.info.is_compound {
            true => (
                cfs.file(".fnm")
                    .ok_or_else(|| crate::Error(format!("{} has no .fnm", seg.name)))?,
                String::new(),
            ),
            false => return err("non-compound segments are not supported yet"),
        },
        g => {
            let suffix = radix36(g as u64);
            owned = crate::read_file(&dir.join(format!("{}_{suffix}.fnm", seg.name)))?;
            (owned.as_slice(), suffix)
        }
    };
    codec_util::check_footer(file)?;
    let input = &mut DataInput::new(file);
    let format = codec_util::check_index_header(
        input,
        "Lucene94FieldInfos",
        0,
        FORMAT_DOCVALUE_SKIPPER,
        &seg.info.id,
        &suffix,
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
        let index_options = match input.read_byte()? {
            0 => IndexOptions::None,
            1 => IndexOptions::Docs,
            2 => IndexOptions::DocsAndFreqs,
            3 => IndexOptions::DocsAndFreqsAndPositions,
            4 => IndexOptions::DocsAndFreqsAndPositionsAndOffsets,
            5 => IndexOptions::DocsAndCustomFreqs,
            b => return err(format!("invalid index options {b} for {name}")),
        };
        let doc_values = match input.read_byte()? {
            0 => DocValuesType::None,
            1 => DocValuesType::Numeric,
            2 => DocValuesType::Binary,
            3 => DocValuesType::Sorted,
            4 => DocValuesType::SortedSet,
            5 => DocValuesType::SortedNumeric,
            b => return err(format!("invalid doc values type {b} for {name}")),
        };
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
