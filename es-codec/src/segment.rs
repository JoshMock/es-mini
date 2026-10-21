//! One segment opened for reading: its compound file and field infos, and everything they unlock.

use crate::{
    CompoundFile, Result, SegmentCommitInfo, doc_values, err,
    field_infos::{self, FieldInfo},
    live_docs, stored_fields, terms,
};
use std::path::{Path, PathBuf};

pub struct SegmentReader {
    pub dir: PathBuf,
    pub seg: SegmentCommitInfo,
    pub cfs: CompoundFile,
    pub fields: Vec<FieldInfo>,
}

impl SegmentReader {
    pub fn open(dir: &Path, seg: SegmentCommitInfo) -> Result<Self> {
        let cfs = CompoundFile::open(dir, &seg.name, &seg.info.id)?;
        let fields = field_infos::read_field_infos(dir, &seg, &cfs)?;
        Ok(Self {
            dir: dir.to_path_buf(),
            seg,
            cfs,
            fields,
        })
    }

    pub fn max_doc(&self) -> usize {
        self.seg.info.max_doc as usize
    }

    pub fn field(&self, name: &str) -> Result<&FieldInfo> {
        self.fields
            .iter()
            .find(|f| f.name == name)
            .map_or_else(|| err(format!("{} has no field {name}", self.seg.name)), Ok)
    }

    // ponytail: linear scan per lookup; index by number in a map if field counts grow.
    pub fn field_by_number(&self, number: i32) -> Result<&FieldInfo> {
        self.fields.iter().find(|f| f.number == number).map_or_else(
            || err(format!("{} has no field {number}", self.seg.name)),
            Ok,
        )
    }

    /// A sub-file of the compound file by name without the segment prefix, e.g. `.fdt`.
    pub(crate) fn cfs_file(&self, name: &str) -> Result<&[u8]> {
        self.cfs
            .file(name)
            .map_or_else(|| err(format!("{} has no {name}", self.seg.name)), Ok)
    }

    /// A per-generation file beside the compound file, e.g. `_0_1.liv` for `suffix` `_1.liv`.
    pub(crate) fn gen_file(&self, suffix: &str) -> Result<Vec<u8>> {
        crate::read_file(&self.dir.join(format!("{}{suffix}", self.seg.name)))
    }

    /// All stored fields of every doc, indexed by doc id: `(field number, value)`.
    pub fn stored_fields(&self) -> Result<Vec<Vec<(i32, stored_fields::StoredValue)>>> {
        stored_fields::read_stored_fields(self)
    }

    /// Whether each doc id is live: neither hard-deleted nor soft-deleted.
    pub fn live_docs(&self) -> Result<Vec<bool>> {
        live_docs::read_live_docs(self)
    }

    /// Every term of `field` in term order with its postings.
    pub fn terms(&self, field: &str) -> Result<Vec<terms::TermPostings>> {
        terms::read_terms(self, field)
    }

    /// A field's doc values, from the field's latest doc values generation.
    pub fn doc_values(&self, field: &str) -> Result<doc_values::DocValues> {
        doc_values::read_field_values(self, field)
    }
}
