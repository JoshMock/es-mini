//! One segment opened for reading: its (compound or separate) files and field infos, and everything they unlock.

use crate::{
    CompoundFile, Result, SegmentCommitInfo, doc_values, err,
    field_infos::{self, FieldInfo},
    live_docs, norms, points, stored_fields, terms,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub struct SegmentReader {
    pub dir: PathBuf,
    pub seg: SegmentCommitInfo,
    pub cfs: CompoundFile,
    pub fields: Vec<FieldInfo>,
    by_number: HashMap<i32, usize>,
}

impl SegmentReader {
    pub fn open(dir: &Path, seg: SegmentCommitInfo) -> Result<Self> {
        let cfs = if seg.info.is_compound {
            CompoundFile::open(dir, &seg.name, &seg.info.id)?
        } else {
            CompoundFile::open_files(dir, &seg.name, &seg.info.files)?
        };
        let fields = field_infos::read_field_infos(dir, &seg, &cfs)?;
        let by_number = fields
            .iter()
            .enumerate()
            .map(|(i, f)| (f.number, i))
            .collect();
        Ok(Self {
            dir: dir.to_path_buf(),
            seg,
            cfs,
            fields,
            by_number,
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

    pub fn field_by_number(&self, number: i32) -> Result<&FieldInfo> {
        self.by_number.get(&number).map_or_else(
            || err(format!("{} has no field {number}", self.seg.name)),
            |&i| Ok(&self.fields[i]),
        )
    }

    /// A sub-file of the compound file by name without the segment prefix, e.g. `.fdt`.
    pub(crate) fn cfs_file(&self, name: &str) -> Result<Vec<u8>> {
        self.cfs
            .file(name)?
            .map_or_else(|| err(format!("{} has no {name}", self.seg.name)), Ok)
    }

    /// `len` bytes at `offset` of a sub-file of the compound file.
    pub(crate) fn cfs_slice(&self, name: &str, offset: u64, len: u64) -> Result<Vec<u8>> {
        self.cfs
            .slice(name, offset, len)?
            .map_or_else(|| err(format!("{} has no {name}", self.seg.name)), Ok)
    }

    /// A per-generation file beside the compound file, e.g. `_0_1.liv` for `suffix` `_1.liv`.
    pub(crate) fn gen_file(&self, suffix: &str) -> Result<Vec<u8>> {
        crate::read_file(&self.dir.join(format!("{}{suffix}", self.seg.name)))
    }

    /// All stored fields of every doc, indexed by doc id: `(field number, value)`.
    pub fn stored_fields(&self) -> Result<Vec<stored_fields::Doc>> {
        self.documents(0..self.max_doc())?.collect()
    }

    /// The stored fields of the docs in `docs`, in doc order, read from disk a chunk at a time.
    pub fn documents(
        &self,
        docs: std::ops::Range<usize>,
    ) -> Result<impl Iterator<Item = Result<stored_fields::Doc>> + '_> {
        let docs = self.documents_where(move |d| docs.contains(&d))?;
        Ok(docs.map(|d| d.map(|d| d.1)))
    }

    /// The stored fields of the docs `keep` accepts, with their doc ids, in doc order. Only the
    /// chunks holding them are read.
    pub fn documents_where<'a>(
        &'a self,
        keep: impl Fn(usize) -> bool + 'a,
    ) -> Result<impl Iterator<Item = Result<(usize, stored_fields::Doc)>> + 'a> {
        stored_fields::read_documents(self, keep)
    }

    /// The stored fields of one doc.
    pub fn document(&self, doc: usize) -> Result<stored_fields::Doc> {
        self.documents(doc..doc + 1)?
            .next()
            .unwrap_or_else(|| err(format!("{} has no doc {doc}", self.seg.name)))
    }

    /// Whether each doc id is live: neither hard-deleted nor soft-deleted.
    pub fn live_docs(&self) -> Result<Vec<bool>> {
        live_docs::read_live_docs(self)
    }

    /// Every term of `field` in term order with its postings.
    pub fn terms(&self, field: &str) -> Result<Vec<terms::TermPostings>> {
        terms::read_terms(self, field, None)
    }

    /// Every term of `field` in term order with its postings, decoded a term at a time.
    pub fn term_stream(&self, field: &str) -> Result<terms::TermsIter<'static>> {
        terms::term_stream(self, field)
    }

    /// One term of `field` with its docs and freqs (no positions), if the field has it.
    pub fn term(&self, field: &str, term: &[u8]) -> Result<Option<terms::TermPostings>> {
        Ok(terms::read_terms(self, field, Some(term))?.pop())
    }

    /// Each doc's norm of `field` (0 for docs without one); `None` if the field omits norms.
    pub fn norms(&self, field: &str) -> Result<Option<Vec<i64>>> {
        norms::read_norms(self, field)
    }

    /// The docs with a point value of one-dimensional `field` that `place` puts in its range
    /// (`Equal`), once per such value, in no particular order, read by walking the field's BKD
    /// tree. `place` gets values as [`points::decode`] gives them and must be monotone (`Less`
    /// below the range, `Greater` above); subtrees entirely below or above are not read. Errs
    /// if the field has no such points.
    pub fn point_docs(
        &self,
        field: &str,
        place: impl Fn(i64) -> std::cmp::Ordering,
    ) -> Result<Vec<u32>> {
        points::read_point_docs(self, field, place)
    }

    /// Every point of `field`, `(doc, value)` sorted, read leaf by leaf.
    pub fn points(&self, field: &str) -> Result<points::PointValues> {
        points::read_points(self, field)
    }

    /// A field's doc values, from the field's latest doc values generation.
    pub fn doc_values(&self, field: &str) -> Result<doc_values::DocValues> {
        doc_values::read_field_values(self, field)
    }
}
