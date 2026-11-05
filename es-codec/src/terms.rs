//! Terms dictionary and postings (Lucene103 block tree + Lucene104 postings).
//!
//! Reading every term walks the blocks from the root; looking up one term seeks through the
//! `.tip` trie to the only block that can hold it. Impacts and skip data are skipped.

pub use crate::postings::Position;
use std::borrow::Cow;

use crate::{
    Result, SegmentReader, codec_util, err,
    field_infos::{FieldInfo, IndexOptions},
    input::{DataInput, zigzag},
    lz4, postings,
    trie::{self, Node, NodeOutput, TrieOutput, TrieReader},
};

/// Deepest nesting of sub-blocks accepted before a `.tim` is called corrupt.
// ponytail: a crafted index could otherwise stack a block per few bytes of `.tim`; real block
// trees are a handful of levels deep.
const MAX_BLOCK_DEPTH: usize = 128;

/// One term of a field and its postings. `freq` is 1 for every doc when the field omits freqs.
#[derive(Debug, Clone, PartialEq)]
pub struct TermPostings {
    pub term: Vec<u8>,
    pub total_term_freq: u64,
    /// `(doc id, freq)`, ascending by doc id.
    pub docs: Vec<(u32, u32)>,
    /// Occurrences per doc, parallel to `docs`; empty for fields indexed without positions.
    pub positions: Vec<Vec<Position>>,
}

impl TermPostings {
    /// A term with docs-and-freqs postings and no positions.
    pub fn new(term: Vec<u8>, docs: Vec<(u32, u32)>) -> Self {
        let total_term_freq = docs.iter().map(|d| u64::from(d.1)).sum();
        Self {
            term,
            total_term_freq,
            docs,
            positions: vec![],
        }
    }

    /// Groups `(term, doc)` entries, sorted by term then doc, into one freq-1 posting per entry.
    pub fn from_sorted(entries: impl IntoIterator<Item = (Vec<u8>, u32)>) -> Vec<Self> {
        let mut terms: Vec<Self> = vec![];
        for (term, doc) in entries {
            match terms.last_mut() {
                Some(t) if t.term == term => {
                    t.docs.push((doc, 1));
                    t.total_term_freq += 1;
                }
                _ => terms.push(Self::new(term, vec![(doc, 1)])),
            }
        }
        terms
    }
}

/// Every term of `field`, or only `want` (if present) and without positions.
pub(crate) fn read_terms(
    r: &SegmentReader,
    field: &str,
    want: Option<&[u8]>,
) -> Result<Vec<TermPostings>> {
    let (files, suffix, info) = field_files(r, field, want.is_none())?;
    decode_terms(&files, &r.seg.info.id, &suffix, &r.fields, info, want)
}

/// Every term of `field` in term order with its postings, decoded a block of terms at a time.
pub(crate) fn term_stream(r: &SegmentReader, field: &str) -> Result<TermsIter<'static>> {
    let (files, suffix, info) = field_files(r, field, true)?;
    open_terms(Cow::Owned(files), &r.seg.info.id, &suffix, &r.fields, info)?.iter()
}

/// The postings files holding `field`, without `.pos` and `.pay` unless `positions`, with their
/// suffix and the field.
fn field_files<'r>(
    r: &'r SegmentReader,
    field: &str,
    positions: bool,
) -> Result<(TermFiles, String, &'r FieldInfo)> {
    let info = r.field(field)?;
    let attr = |k: &str| info.attributes.get(&format!("PerFieldPostingsFormat.{k}"));
    let (Some(format), Some(suffix)) = (attr("format"), attr("suffix")) else {
        return err(format!("field {field} has no postings"));
    };
    if format != "Lucene104" {
        return err(format!("postings format {format} is not supported"));
    }
    let suffix = format!("{format}_{suffix}");
    let file = |ext: &str| r.cfs_file(&format!("_{suffix}.{ext}"));
    // `.pos` and `.pay` exist only when some field has positions, or offsets or payloads.
    let positions = |ext| match positions {
        true => file(ext).unwrap_or_default(),
        false => Vec::new(),
    };
    let files = TermFiles {
        tim: file("tim")?,
        tip: file("tip")?,
        tmd: file("tmd")?,
        doc: file("doc")?,
        pos: positions("pos"),
        pay: positions("pay"),
        psm: Vec::new(),
    };
    Ok((files, suffix, info))
}

/// The postings files of one segment's postings format instance.
#[derive(Default, Clone)]
pub struct TermFiles {
    pub tim: Vec<u8>,
    pub tip: Vec<u8>,
    pub tmd: Vec<u8>,
    pub doc: Vec<u8>,
    pub pos: Vec<u8>,
    pub pay: Vec<u8>,
    /// Postings metadata (`.psm`), written but not read.
    pub psm: Vec<u8>,
}

fn decode_terms(
    files: &TermFiles,
    id: &[u8; codec_util::ID_LEN],
    suffix: &str,
    fields: &[FieldInfo],
    info: &FieldInfo,
    want: Option<&[u8]>,
) -> Result<Vec<TermPostings>> {
    let terms = open_terms(Cow::Borrowed(files), id, suffix, fields, info)?;
    match want {
        Some(target) => Ok(terms.seek(target)?.into_iter().collect()),
        None => terms.iter()?.collect(),
    }
}

/// Checks `files` and finds `info`'s entry in the terms metadata.
fn open_terms<'a>(
    files: Cow<'a, TermFiles>,
    id: &[u8; codec_util::ID_LEN],
    suffix: &str,
    fields: &[FieldInfo],
    info: &FieldInfo,
) -> Result<FieldTerms<'a>> {
    let field = info.name.as_str();
    for (name, f, codec) in [
        ("tim", &files.tim, "BlockTreeTermsDict"),
        ("tip", &files.tip, "BlockTreeTermsIndex"),
        ("doc", &files.doc, "Lucene104PostingsWriterDoc"),
        ("pos", &files.pos, "Lucene104PostingsWriterPos"),
        ("pay", &files.pay, "Lucene104PostingsWriterPay"),
    ] {
        if f.is_empty() {
            continue;
        }
        codec_util::check_footer(f)?;
        codec_util::check_index_header(&mut DataInput::new(f), codec, 0, 0, id, suffix)
            .map_err(|e| crate::Error(format!(".{name}: {e}")))?;
    }

    let tmd = &files.tmd[..];
    codec_util::check_footer(tmd)?;
    let meta = &mut DataInput::new(tmd);
    codec_util::check_index_header(meta, "BlockTreeTermsMeta", 0, 0, id, suffix)?;
    codec_util::check_index_header(meta, "Lucene104PostingsWriterTerms", 0, 0, id, suffix)?;
    if meta.read_vint()? as u64 != postings::BLOCK_SIZE as u64 {
        return err("unexpected postings block size");
    }
    for _ in 0..meta.read_len()? {
        let number = meta.read_vint()?;
        let num_terms = meta.read_vlong()?;
        let sum_total_term_freq = meta.read_vlong()?;
        let Some(f) = fields.iter().find(|f| f.number == number) else {
            return err(format!("no field {number}"));
        };
        if f.index_options != IndexOptions::Docs {
            meta.read_vlong()?; // sumDocFreq
        }
        meta.read_vint()?; // docCount
        for _ in 0..2 {
            let n = meta.read_len()?; // minTerm, maxTerm
            meta.read_bytes(n)?;
        }
        let (index_start, root_fp, index_end) =
            (meta.read_vlong()?, meta.read_vlong()?, meta.read_vlong()?);
        if number != info.number {
            continue;
        }
        let index = index_start as usize..index_end as usize;
        if files.tip.get(index.clone()).is_none() {
            return err("trie extends past the index");
        }
        return Ok(FieldTerms {
            files,
            info: info.clone(),
            index,
            root_fp: root_fp as usize,
            stats: (num_terms, sum_total_term_freq as u64),
        });
    }
    err(format!("field {field} has no terms"))
}
/// Appends one field's terms, stats and postings to the shared output files, a term at a time
/// as `terms` yields them in order, like Lucene's `BlockTreeTermsWriter`.
fn write_field<T: std::borrow::Borrow<TermPostings>>(
    info: &FieldInfo,
    terms: impl IntoIterator<Item = Result<T>>,
    field_norms: Option<&[i8]>,
    files: &mut FieldOutputs,
    max: &mut postings::ImpactMax,
) -> crate::Result<()> {
    let FieldOutputs {
        tim,
        tip,
        tmd,
        doc,
        pos,
    } = files;
    let options = info.index_options;
    let unsupported = || {
        crate::err(format!(
            "field {} needs terms and postings without offsets or payloads",
            info.name
        ))
    };
    if info.store_payloads
        || !matches!(
            options,
            IndexOptions::Docs
                | IndexOptions::DocsAndFreqs
                | IndexOptions::DocsAndFreqsAndPositions
        )
    {
        return unsupported();
    }
    let has_freqs = options != IndexOptions::Docs;
    let has_positions = options == IndexOptions::DocsAndFreqsAndPositions;
    let (mut sum_df, mut docs_seen) = (0u64, std::collections::BTreeSet::new());
    let (mut num_terms, mut sum_ttf, mut min_term) = (0i64, 0u64, None);
    let mut tree = BlockTree {
        has_freqs,
        tim,
        pending: vec![],
        lz4: None,
        prefix_starts: vec![],
        last_term: vec![],
    };
    for t in terms {
        let t = t?;
        let t = t.borrow();
        let df = t.docs.len() as u64;
        sum_df += df;
        docs_seen.extend(t.docs.iter().map(|d| d.0));
        let marks = if has_positions {
            let freqs_match = t.positions.len() == t.docs.len()
                && (t.positions.iter().zip(&t.docs)).all(|(p, d)| p.len() == d.1 as usize);
            if !freqs_match {
                return crate::err(format!("positions of {} do not match its freqs", info.name));
            }
            Some(postings::write_positions(pos, &t.positions)?)
        } else {
            None
        };
        let meta = TermMeta {
            doc_freq: df,
            total_term_freq: t.total_term_freq,
            doc_fp: doc.0.len() as u64,
            singleton: (df == 1).then(|| t.docs[0].0),
            pos_fp: marks.as_ref().map(|m| m.start),
            last_pos_block_offset: marks.as_ref().and_then(|m| m.last_block_offset),
        };
        if df > 1 {
            postings::write_docs(doc, &t.docs, has_freqs, field_norms, max, marks.as_ref());
        }
        tree.push_term(&t.term);
        tree.pending.push(Pending::Term(t.term.clone(), meta));
        min_term.get_or_insert_with(|| t.term.clone());
        num_terms += 1;
        sum_ttf += t.total_term_freq;
    }
    let Some(min_term) = min_term else {
        return unsupported();
    };
    let max_term = tree.last_term.clone();
    // An empty term closes every open prefix; then the root block takes what is left.
    tree.push_term(&[]);
    tree.write_blocks(0, tree.pending.len());
    let Some(Pending::Block(root)) = tree.pending.pop() else {
        unreachable!("write_blocks leaves one block");
    };
    tmd.vint(info.number);
    tmd.vlong(num_terms);
    tmd.vlong(sum_ttf as i64);
    if has_freqs {
        tmd.vlong(sum_df as i64);
    }
    tmd.vint(docs_seen.len() as i32);
    for t in [&min_term, &max_term] {
        tmd.vint(t.len() as i32);
        tmd.bytes(t);
    }
    tmd.vlong(tip.0.len() as i64);
    tmd.vlong(trie::save(&root.index, tip) as i64);
    tip.long(0); // Lucene's TrieReader reads a long at a node, possibly past the trie's end
    tmd.vlong(tip.0.len() as i64);
    Ok(())
}

/// Lucene's `minItemsInBlock` and `maxItemsInBlock`: a prefix gets its own block once this
/// many terms and sub-blocks share it, and blocks over the maximum split into floor blocks.
const MIN_ITEMS_IN_BLOCK: usize = 25;
const MAX_ITEMS_IN_BLOCK: usize = 48;

/// Where one term's postings are (`IntBlockTermState`).
struct TermMeta {
    doc_freq: u64,
    total_term_freq: u64,
    doc_fp: u64,
    singleton: Option<u32>,
    /// `.pos` start, for fields with positions.
    pos_fp: Option<u64>,
    last_pos_block_offset: Option<u64>,
}

enum Pending {
    Term(Vec<u8>, TermMeta),
    Block(PendingBlock),
}

/// A written block: its prefix, where it starts, and the trie entries of it and its sub-blocks.
struct PendingBlock {
    prefix: Vec<u8>,
    fp: u64,
    index: Vec<(Vec<u8>, TrieOutput)>,
}

/// `Lucene103BlockTreeTermsWriter.TermsWriter`: a stack of pending terms and blocks; once enough
/// entries share a prefix that the next term leaves, they are written as a block and replaced
/// on the stack by that block.
struct BlockTree<'a> {
    has_freqs: bool,
    tim: &'a mut crate::output::Output,
    pending: Vec<Pending>,
    /// Lucene keeps one LZ4 table per field, made on first use.
    lz4: Option<lz4::HcTable>,
    /// Where in `pending` the entries sharing the last term's first `i + 1` bytes start.
    prefix_starts: Vec<usize>,
    last_term: Vec<u8>,
}

impl BlockTree<'_> {
    /// `pushTerm`: closes the prefixes of the last term that `text` does not share.
    fn push_term(&mut self, text: &[u8]) {
        let common = self
            .last_term
            .iter()
            .zip(text)
            .take_while(|(a, b)| a == b)
            .count();
        for i in (common..self.last_term.len()).rev() {
            let top = self.pending.len() - self.prefix_starts[i];
            if top >= MIN_ITEMS_IN_BLOCK {
                // Lucene also adjusts prefixStarts[i] here, but nothing reads it before the
                // reset below.
                self.write_blocks(i + 1, top);
            }
        }
        self.prefix_starts
            .resize(self.prefix_starts.len().max(text.len()), 0);
        for start in &mut self.prefix_starts[common..text.len()] {
            *start = self.pending.len();
        }
        self.last_term = text.to_vec();
    }

    /// The first byte after `prefix_len` of a pending entry, or `None` for a term equal to the
    /// prefix.
    fn lead_label(&self, entry: &Pending, prefix_len: usize) -> Option<u8> {
        match entry {
            Pending::Term(term, _) => term.get(prefix_len).copied(),
            Pending::Block(b) => Some(b.prefix[prefix_len]),
        }
    }

    /// `writeBlocks`: writes the top `count` pending entries, which share the last term's first
    /// `prefix_len` bytes, as one block or as floor blocks of at least `MIN_ITEMS_IN_BLOCK`.
    fn write_blocks(&mut self, prefix_len: usize, count: usize) {
        let (start, end) = (self.pending.len() - count, self.pending.len());
        // (lead label, fp, has terms, trie entries of its sub-blocks) per block
        let mut blocks: Vec<(Option<u8>, u64, bool, Vec<_>)> = vec![];
        let (mut last_lead, mut next_lead) = (None, None);
        let (mut next_start, mut has_terms, mut has_sub_blocks) = (start, false, false);
        for i in start..end {
            let lead = self.lead_label(&self.pending[i], prefix_len);
            if lead != last_lead {
                if i - next_start >= MIN_ITEMS_IN_BLOCK && end - next_start > MAX_ITEMS_IN_BLOCK {
                    let (fp, sub) = self.write_block(prefix_len, next_start, i, has_sub_blocks);
                    blocks.push((next_lead, fp, has_terms, sub));
                    (has_terms, has_sub_blocks) = (false, false);
                    (next_lead, next_start) = (lead, i);
                }
                last_lead = lead;
            }
            match self.pending[i] {
                Pending::Term(..) => has_terms = true,
                Pending::Block(_) => has_sub_blocks = true,
            }
        }
        let (fp, sub) = self.write_block(prefix_len, next_start, end, has_sub_blocks);
        blocks.push((next_lead, fp, has_terms, sub));

        // `PendingBlock.compileIndex`: floor blocks are reached through the first block's output.
        let (first_fp, first_has_terms) = (blocks[0].1, blocks[0].2);
        let floor = (blocks.len() > 1).then(|| {
            let mut out = crate::output::Output::default();
            out.vint(blocks.len() as i32 - 1);
            for (lead, fp, has_terms, _) in &blocks[1..] {
                out.byte(lead.expect("only the first floor block has an empty suffix"));
                out.vlong((((fp - first_fp) << 1) | u64::from(*has_terms)) as i64);
            }
            out.0
        });
        let prefix = self.last_term[..prefix_len].to_vec();
        let output = TrieOutput {
            fp: first_fp,
            has_terms: first_has_terms,
            floor,
        };
        let mut index = vec![(prefix.clone(), output)];
        for (.., sub) in blocks {
            index.extend(sub);
        }
        self.pending.truncate(start);
        self.pending.push(Pending::Block(PendingBlock {
            prefix,
            fp: first_fp,
            index,
        }));
    }

    /// `writeBlock`: writes `pending[start..end]` as one block, returning its file pointer and
    /// the trie entries of the sub-blocks in it.
    ///
    fn write_block(
        &mut self,
        prefix_len: usize,
        start: usize,
        end: usize,
        has_sub_blocks: bool,
    ) -> (u64, Vec<(Vec<u8>, TrieOutput)>) {
        use crate::output::Output;
        let fp = self.tim.0.len() as u64;
        let (mut suffixes, mut lengths, mut stats, mut metas) = (
            Output::default(),
            Output::default(),
            Output::default(),
            Output::default(),
        );
        let (mut singletons, mut last, mut sub_index) = (0, None, vec![]);
        for entry in &mut self.pending[start..end] {
            match entry {
                Pending::Term(term, m) => {
                    let suffix = &term[prefix_len..];
                    let shift = u32::from(has_sub_blocks);
                    lengths.vint((suffix.len() << shift) as i32);
                    suffixes.bytes(suffix);
                    // `StatsWriter`: runs of singleton terms collapse into one token.
                    if m.doc_freq == 1 && (!self.has_freqs || m.total_term_freq == 1) {
                        singletons += 1;
                    } else {
                        flush_singletons(&mut stats, &mut singletons);
                        stats.vint((m.doc_freq << 1) as i32);
                        if self.has_freqs {
                            stats.vlong((m.total_term_freq - m.doc_freq) as i64);
                        }
                    }
                    encode_term(&mut metas, m, last);
                    last = Some(m);
                }
                Pending::Block(b) => {
                    let suffix = &b.prefix[prefix_len..];
                    lengths.vint(((suffix.len() << 1) | 1) as i32);
                    suffixes.bytes(suffix);
                    lengths.vlong((fp - b.fp) as i64);
                    sub_index.append(&mut b.index);
                }
            }
        }
        flush_singletons(&mut stats, &mut singletons);
        let tim = &mut *self.tim;
        tim.vint((((end - start) << 1) | usize::from(end == self.pending.len())) as i32);
        let leaf = if has_sub_blocks { 0 } else { 4 };
        let (suffixes, n) = (suffixes.0, end - start);
        // Lucene compresses only blocks with long suffixes behind a prefix over 2 bytes: LZ4
        // if it saves over 25%, else lowercase ASCII if it applies.
        let mut compressed = None;
        if suffixes.len() > 2 * n && prefix_len > 2 {
            if suffixes.len() > 6 * n {
                let mut out = Output::default();
                let ht = self.lz4.get_or_insert_with(Default::default);
                lz4::compress(&suffixes, 0, &mut out, ht);
                if out.0.len() < suffixes.len() - (suffixes.len() >> 2) {
                    compressed = Some((2, out.0));
                }
            }
            if compressed.is_none() {
                compressed = compress_lowercase_ascii(&suffixes).map(|c| (1, c));
            }
        }
        let (algorithm, bytes) = compressed.as_ref().map_or((0, &suffixes), |(a, c)| (*a, c));
        tim.vlong(((suffixes.len() << 3) | leaf | algorithm) as i64);
        tim.bytes(bytes);
        let lengths = lengths.0;
        if lengths[1..].iter().all(|&b| b == lengths[0]) {
            // Terms of the same length, like ids, often give one repeated byte.
            tim.vint(((lengths.len() << 1) | 1) as i32);
            tim.byte(lengths[0]);
        } else {
            tim.vint((lengths.len() << 1) as i32);
            tim.bytes(&lengths);
        }
        for part in [&stats, &metas] {
            tim.vint(part.0.len() as i32);
            tim.bytes(&part.0);
        }
        (fp, sub_index)
    }
}

fn flush_singletons(stats: &mut crate::output::Output, singletons: &mut u32) {
    if *singletons > 0 {
        stats.vint((((*singletons - 1) << 1) | 1) as i32);
        *singletons = 0;
    }
}

/// `Lucene104PostingsWriter.encodeTerm`: pointers relative to the previous term in the block
/// (`last`), and a run of singletons as doc id deltas.
fn encode_term(out: &mut crate::output::Output, m: &TermMeta, last: Option<&TermMeta>) {
    let last_fp = last.map_or(0, |l| l.doc_fp);
    match (last.and_then(|l| l.singleton), m.singleton) {
        (Some(prev), Some(doc)) if m.doc_fp == last_fp => {
            let delta = i64::from(doc) - i64::from(prev);
            out.vlong((((delta << 1) ^ (delta >> 63)) << 1) | 1);
        }
        _ => {
            out.vlong(((m.doc_fp - last_fp) << 1) as i64);
            if let Some(doc) = m.singleton {
                out.vint(doc as i32);
            }
        }
    }
    if let Some(fp) = m.pos_fp {
        out.vlong((fp - last.and_then(|l| l.pos_fp).unwrap_or(0)) as i64);
        if let Some(offset) = m.last_pos_block_offset {
            out.vlong(offset as i64);
        }
    }
}

/// The shared `.tim`, `.tip`, `.tmd`, `.doc` and `.pos` outputs.
struct FieldOutputs {
    tim: crate::output::Output,
    tip: crate::output::Output,
    tmd: crate::output::Output,
    doc: crate::output::Output,
    pos: crate::output::Output,
}

/// Writes the terms dictionary and postings of `fields` (docs, freqs and positions) with a
/// [`TermsWriter`]. `norms` are the fields' norms per doc, for the competitive impacts of freq
/// fields.
pub fn write_terms(
    fields: &[(&FieldInfo, &[TermPostings])],
    id: &[u8; codec_util::ID_LEN],
    suffix: &str,
    norms: &[(i32, &[i8])],
) -> crate::Result<TermFiles> {
    let mut w = TermsWriter::new(id, suffix);
    for (info, terms) in fields {
        let field_norms = norms.iter().find(|n| n.0 == info.number).map(|n| n.1);
        w.add(info, terms.iter().map(Ok), field_norms)?;
    }
    Ok(w.finish())
}

/// Writes the terms dictionary and postings a field at a time, in blocks of 25 to 48 entries
/// like Lucene's `Lucene103BlockTreeTermsWriter`.
///
/// ponytail: no offsets or payloads (`.pay`); ES text fields index neither by default.
pub struct TermsWriter {
    id: [u8; codec_util::ID_LEN],
    suffix: String,
    /// `.tmd` holds the fields' entries until [`Self::finish`] puts the header before them.
    files: FieldOutputs,
    fields: i32,
    max: postings::ImpactMax,
}

impl TermsWriter {
    pub fn new(id: &[u8; codec_util::ID_LEN], suffix: &str) -> Self {
        use crate::output::Output;
        let (mut tim, mut tip, mut doc) = (Output::default(), Output::default(), Output::default());
        tim.index_header("BlockTreeTermsDict", 0, id, suffix);
        tip.index_header("BlockTreeTermsIndex", 0, id, suffix);
        doc.index_header("Lucene104PostingsWriterDoc", 0, id, suffix);
        Self {
            id: *id,
            suffix: suffix.into(),
            files: FieldOutputs {
                tim,
                tip,
                tmd: Output::default(),
                doc,
                pos: Output::default(),
            },
            fields: 0,
            max: Default::default(),
        }
    }

    /// A field's terms, in order, written as `terms` yields them; `norms` are its norms per doc,
    /// for the competitive impacts of a freq field.
    pub fn add<T: std::borrow::Borrow<TermPostings>>(
        &mut self,
        info: &FieldInfo,
        terms: impl IntoIterator<Item = Result<T>>,
        norms: Option<&[i8]>,
    ) -> crate::Result<()> {
        // Like Lucene, `.pos` exists when a field has positions (`FieldInfos.hasProx`).
        let pos = &mut self.files.pos;
        if info.index_options >= IndexOptions::DocsAndFreqsAndPositions && pos.0.is_empty() {
            pos.index_header("Lucene104PostingsWriterPos", 0, &self.id, &self.suffix);
        }
        write_field(info, terms, norms, &mut self.files, &mut self.max)?;
        self.fields += 1;
        Ok(())
    }

    pub fn finish(self) -> TermFiles {
        use crate::output::Output;
        let Self {
            id,
            suffix,
            files,
            fields,
            max,
        } = self;
        let FieldOutputs {
            tim,
            tip,
            tmd: entries,
            doc,
            pos,
        } = files;
        let mut tmd = Output::default();
        tmd.index_header("BlockTreeTermsMeta", 0, &id, &suffix);
        tmd.index_header("Lucene104PostingsWriterTerms", 0, &id, &suffix);
        tmd.vint(postings::BLOCK_SIZE as i32);
        tmd.vint(fields);
        tmd.bytes(&entries.0);
        // Full file lengths (footers included): `.tip` first, then `.tim`.
        tmd.long(tip.0.len() as i64 + FOOTER_LEN);
        tmd.long(tim.0.len() as i64 + FOOTER_LEN);
        let doc = doc.finish();
        let mut psm = Output::default();
        psm.index_header("Lucene104PostingsWriterMeta", 0, &id, &suffix);
        for v in [
            max.level0_num,
            max.level0_bytes,
            max.level1_num,
            max.level1_bytes,
        ] {
            psm.bytes(&v.to_le_bytes());
        }
        psm.long(doc.len() as i64);
        let pos = if pos.0.is_empty() {
            vec![]
        } else {
            let pos = pos.finish();
            psm.long(pos.len() as i64);
            pos
        };
        TermFiles {
            tim: tim.finish(),
            tip: tip.finish(),
            tmd: tmd.finish(),
            psm: psm.finish(),
            doc,
            pos,
            pay: vec![],
        }
    }
}

const FOOTER_LEN: i64 = 16;

/// One field's terms in its segment's postings files.
struct FieldTerms<'a> {
    files: Cow<'a, TermFiles>,
    info: FieldInfo,
    /// The field's trie in `.tip`, and where its root node starts there.
    index: std::ops::Range<usize>,
    root_fp: usize,
    /// The committed term count and summed total term freq, checked by a walk of every term.
    stats: (i64, u64),
}

/// Running state while decoding one block's term metadata (`IntBlockTermState`).
#[derive(Default, Clone)]
struct TermState {
    doc_freq: u64,
    total_term_freq: u64,
    doc_start_fp: u64,
    pos_start_fp: u64,
    pay_start_fp: u64,
    singleton_doc_id: i64,
    singleton_run: u64,
}

/// A block entry: a term and its state, or a sub-block's prefix and file pointer.
enum Entry {
    Term(Vec<u8>, TermState),
    Block(Vec<u8>, usize),
}

/// A decoded block's entries yet to visit, and where its next floor block starts unless it is
/// the last one.
struct Block {
    prefix: Vec<u8>,
    depth: usize,
    entries: std::vec::IntoIter<Entry>,
    next: Option<usize>,
}

/// Every term of a field in term order with its postings (`SegmentTermsEnum.next`): a stack of
/// the blocks being walked, each decoded when reached, and each term's postings when yielded.
pub struct TermsIter<'a> {
    field: FieldTerms<'a>,
    stack: Vec<Block>,
    /// Terms yielded and their summed total term freq.
    seen: (i64, u64),
}

impl Iterator for TermsIter<'_> {
    type Item = Result<TermPostings>;

    fn next(&mut self) -> Option<Self::Item> {
        self.step().transpose()
    }
}

impl TermsIter<'_> {
    fn step(&mut self) -> Result<Option<TermPostings>> {
        loop {
            let Some(top) = self.stack.last_mut() else {
                return Ok(None);
            };
            match top.entries.next() {
                Some(Entry::Term(term, s)) => {
                    self.seen.0 += 1;
                    self.seen.1 += s.total_term_freq;
                    return self.field.postings(term, &s, true).map(Some);
                }
                Some(Entry::Block(prefix, fp)) => {
                    let block = self.field.block(fp, prefix, top.depth + 1)?;
                    self.stack.push(block);
                }
                None => {
                    let Block {
                        prefix,
                        depth,
                        next,
                        ..
                    } = self.stack.pop().expect("the stack has a top");
                    if let Some(fp) = next {
                        let block = self.field.block(fp, prefix, depth)?;
                        self.stack.push(block);
                    } else if self.stack.is_empty() && self.seen != self.field.stats {
                        let field = &self.field.info.name;
                        return err(format!(
                            "terms of {field} do not match the committed statistics"
                        ));
                    }
                }
            }
        }
    }
}

impl<'a> FieldTerms<'a> {
    fn trie(&self) -> TrieReader<'_> {
        TrieReader(&self.files.tip[self.index.clone()])
    }

    /// The trie's root node and its output, which points at the root block.
    fn root(&self) -> Result<(Node, NodeOutput)> {
        let mut root = self.trie().load(self.root_fp)?;
        let Some(out) = root.output.take() else {
            return err("trie root has no output");
        };
        Ok((root, out))
    }

    /// Every term, from the root block on.
    fn iter(self) -> Result<TermsIter<'a>> {
        let root = self.block(self.root()?.1.fp as usize, vec![], 0)?;
        Ok(TermsIter {
            field: self,
            stack: vec![root],
            seen: (0, 0),
        })
    }

    /// `SegmentTermsEnum.seekExact`: follows `target` down the trie to the deepest block whose
    /// prefix it starts with, picks that block's floor block for the next byte, and scans it.
    /// The term has its docs and freqs, without positions.
    fn seek(&self, target: &[u8]) -> Result<Option<TermPostings>> {
        let trie = self.trie();
        let (mut node, mut out) = self.root()?;
        let mut prefix_len = 0;
        for (i, &label) in target.iter().enumerate() {
            let Some(mut child) = trie.child(&node, label)? else {
                break;
            };
            if let Some(o) = child.output.take() {
                (out, prefix_len) = (o, i + 1);
            }
            node = child;
        }
        let (mut fp, mut has_terms) = (out.fp, out.has_terms);
        if let (Some(floor), Some(&label)) = (out.floor, target.get(prefix_len)) {
            let input = &mut DataInput::new(trie.0);
            input.pos = floor;
            for _ in 0..input.read_vint()? {
                if label < input.read_byte()? {
                    break;
                }
                let code = input.read_vlong()? as u64;
                (fp, has_terms) = (out.fp + (code >> 1), code & 1 != 0);
            }
        }
        if !has_terms {
            return Ok(None);
        }
        let block = self.block(fp as usize, target[..prefix_len].to_vec(), 0)?;
        for entry in block.entries {
            if let Entry::Term(term, s) = entry
                && term == target
            {
                return self.postings(term, &s, false).map(Some);
            }
        }
        Ok(None)
    }

    /// Decodes the one block at `fp`: its terms' states and its sub-blocks' pointers.
    fn block(&self, fp: usize, prefix: Vec<u8>, depth: usize) -> Result<Block> {
        if depth > MAX_BLOCK_DEPTH {
            return err("terms blocks nest too deeply");
        }
        let input = &mut DataInput::new(&self.files.tim);
        input.pos = fp;
        let code = input.read_vint()?;
        let (count, is_last) = ((code >> 1) as usize, code & 1 != 0);
        let code = input.read_vlong()?;
        let (is_leaf, len, algorithm) = (code & 4 != 0, (code >> 3) as usize, code & 3);
        let suffixes = match algorithm {
            0 => input.read_bytes(len)?.to_vec(),
            1 => lowercase_ascii(input, len)?,
            2 => {
                let mut out = Vec::new();
                lz4::decompress(input, len, &mut out)?;
                out
            }
            a => return err(format!("unknown suffix compression {a}")),
        };
        let n = input.read_vint()?;
        let n_len_bytes = (n >> 1) as usize;
        let length_bytes = if n & 1 != 0 {
            vec![input.read_byte()?; n_len_bytes]
        } else {
            input.read_bytes(n_len_bytes)?.to_vec()
        };
        let stats_len = input.read_len()?;
        let stats = &mut DataInput::new(input.read_bytes(stats_len)?);
        let meta_len = input.read_len()?;
        let metas = &mut DataInput::new(input.read_bytes(meta_len)?);
        let block_end = input.pos;

        let (lengths, suffixes) = (
            &mut DataInput::new(&length_bytes),
            &mut DataInput::new(&suffixes),
        );
        let mut state = TermState::default();
        let mut first_term = true;
        let mut entries = Vec::with_capacity(count);
        for _ in 0..count {
            let (suffix_len, sub) = if is_leaf {
                (lengths.read_len()?, None)
            } else {
                let c = lengths.read_vint()?;
                let sub = (c & 1 != 0).then(|| lengths.read_vlong()).transpose()?;
                ((c >> 1) as usize, sub)
            };
            let mut term = prefix.clone();
            term.extend_from_slice(suffixes.read_bytes(suffix_len)?);
            if let Some(sub_code) = sub {
                // Sub-blocks are written before their parent, so the delta is positive.
                let sub_fp = usize::try_from(fp as i64 - sub_code)
                    .ok()
                    .filter(|_| sub_code > 0)
                    .ok_or_else(|| crate::Error("corrupt sub-block pointer".into()))?;
                entries.push(Entry::Block(term, sub_fp));
                continue;
            }
            self.decode_stats(stats, &mut state)?;
            self.decode_postings(metas, &mut state, first_term)?;
            first_term = false;
            entries.push(Entry::Term(term, state.clone()));
        }
        Ok(Block {
            prefix,
            depth,
            entries: entries.into_iter(),
            next: (!is_last).then_some(block_end),
        })
    }

    /// `term`'s docs and freqs, and its positions if `positions`.
    fn postings(&self, term: Vec<u8>, s: &TermState, positions: bool) -> Result<TermPostings> {
        let docs = self.docs(s)?;
        let positions = match positions {
            true => self.positions(s, &docs)?,
            false => vec![],
        };
        Ok(TermPostings {
            term,
            total_term_freq: s.total_term_freq,
            positions,
            docs,
        })
    }

    /// Stats stream: doc freq and total term freq, with runs of singleton terms collapsed.
    fn decode_stats(&self, stats: &mut DataInput, s: &mut TermState) -> Result<()> {
        if s.singleton_run > 0 {
            (s.doc_freq, s.total_term_freq) = (1, 1);
            s.singleton_run -= 1;
            return Ok(());
        }
        let token = stats.read_vint()? as u32 as u64;
        if token & 1 == 1 {
            (s.doc_freq, s.total_term_freq) = (1, 1);
            s.singleton_run = token >> 1;
        } else {
            s.doc_freq = token >> 1;
            s.total_term_freq = match self.info.index_options {
                IndexOptions::Docs => s.doc_freq,
                _ => s.doc_freq + stats.read_vlong()? as u64,
            };
        }
        Ok(())
    }

    /// `Lucene104PostingsReader.decodeTerm`; file pointers are deltas within a block.
    fn decode_postings(
        &self,
        metas: &mut DataInput,
        s: &mut TermState,
        absolute: bool,
    ) -> Result<()> {
        if absolute {
            (s.doc_start_fp, s.pos_start_fp, s.pay_start_fp) = (0, 0, 0);
        }
        let l = metas.read_vlong()? as u64;
        if l & 1 == 0 {
            s.doc_start_fp += l >> 1;
            s.singleton_doc_id = if s.doc_freq == 1 {
                metas.read_vint()? as i64
            } else {
                -1
            };
        } else {
            if absolute || s.singleton_doc_id == -1 {
                return err("corrupt singleton delta");
            }
            s.singleton_doc_id += zigzag(l >> 1);
        }
        if self.info.index_options >= IndexOptions::DocsAndFreqsAndPositions {
            s.pos_start_fp += metas.read_vlong()? as u64;
            if self.info.index_options >= IndexOptions::DocsAndFreqsAndPositionsAndOffsets
                || self.info.store_payloads
            {
                s.pay_start_fp += metas.read_vlong()? as u64;
            }
            if s.total_term_freq > postings::BLOCK_SIZE as u64 {
                metas.read_vlong()?; // lastPosBlockOffset
            }
        }
        Ok(())
    }

    fn positions(&self, s: &TermState, docs: &[(u32, u32)]) -> Result<Vec<Vec<Position>>> {
        let options = self.info.index_options;
        if options < IndexOptions::DocsAndFreqsAndPositions {
            return Ok(vec![]);
        }
        let freqs: Vec<u32> = docs.iter().map(|(_, f)| *f).collect();
        postings::read_positions(
            &postings::PositionFiles {
                pos: &self.files.pos,
                pay: &self.files.pay,
                pos_fp: s.pos_start_fp as usize,
                pay_fp: s.pay_start_fp as usize,
                offsets: options >= IndexOptions::DocsAndFreqsAndPositionsAndOffsets,
                payloads: self.info.store_payloads,
            },
            &freqs,
        )
    }

    fn docs(&self, s: &TermState) -> Result<Vec<(u32, u32)>> {
        if s.doc_freq == 1 {
            return Ok(vec![(s.singleton_doc_id as u32, s.total_term_freq as u32)]);
        }
        let has_freqs = self.info.index_options != IndexOptions::Docs;
        postings::read_docs(
            &self.files.doc,
            s.doc_start_fp as usize,
            s.doc_freq as usize,
            has_freqs,
        )
    }
}

/// `LowercaseAsciiCompression.compress`: packs 4 bytes in 3, or `None` if under 8 bytes or more
/// than 1 in 32 bytes (counting a filler every 255 bytes between them) fall outside the 64
/// characters around lowercase letters.
fn compress_lowercase_ascii(input: &[u8]) -> Option<Vec<u8>> {
    let len = input.len();
    if len < 8 {
        return None;
    }
    let mut exceptions = vec![];
    let mut prev = 0;
    for (i, &b) in input.iter().enumerate() {
        if !matches!((u32::from(b) + 1) & !0x1f, 0x20 | 0x60) {
            while i - prev > 0xff {
                prev += 0xff;
                exceptions.push((0xff, input[prev]));
            }
            exceptions.push(((i - prev) as u8, b));
            prev = i;
            if exceptions.len() > len >> 5 {
                return None;
            }
        }
    }
    let compressed = len - (len >> 2);
    let mut tmp: Vec<u8> = input
        .iter()
        .map(|&b| {
            let b = u32::from(b) + 1;
            ((b & 0x1f) | ((b & 0x40) >> 1)) as u8
        })
        .collect();
    let mut o = 0;
    for (mask, shift) in [(0x30, 2), (0x0c, 4), (0x03, 6)] {
        for i in compressed..len {
            tmp[o] |= (tmp[i] & mask) << shift;
            o += 1;
        }
    }
    let mut out = crate::output::Output(tmp[..compressed].to_vec());
    out.vint(exceptions.len() as i32);
    for (delta, b) in exceptions {
        out.byte(delta);
        out.byte(b);
    }
    Some(out.0)
}

/// `LowercaseAsciiCompression.decompress`.
fn lowercase_ascii(input: &mut DataInput, len: usize) -> Result<Vec<u8>> {
    let saved = len >> 2;
    let compressed = len - saved;
    let mut out = input.read_bytes(compressed)?.to_vec();
    out.resize(len, 0);
    for i in 0..saved {
        out[compressed + i] = ((out[i] & 0xc0) >> 2)
            | ((out[saved + i] & 0xc0) >> 4)
            | ((out[2 * saved + i] & 0xc0) >> 6);
    }
    for b in &mut out {
        *b = ((*b & 0x1f) | 0x20 | ((*b & 0x20) << 1)).wrapping_sub(1);
    }
    let mut i = 0;
    for _ in 0..input.read_vint()? {
        i += input.read_byte()? as usize;
        *out.get_mut(i)
            .ok_or_else(|| crate::Error("corrupt lowercase exception".into()))? =
            input.read_byte()?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(name: &str, number: i32, index_options: IndexOptions) -> FieldInfo {
        FieldInfo {
            omit_norms: true,
            index_options,
            ..FieldInfo::new(name, number)
        }
    }

    /// Fields "q" (q000..q099, one doc each) and "t" (LZ4 and lowercase-ASCII compressed
    /// blocks) of tests/data/many-terms, written by Lucene 10.5.1. many_terms.rs checks that the
    /// reader returns the terms Lucene was given.
    #[test]
    fn writes_the_blocks_and_trie_lucene_writes() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/many-terms");
        let seg = crate::SegmentInfos::read_latest(&dir)
            .unwrap()
            .segments
            .remove(0);
        let r = SegmentReader::open(&dir, seg).unwrap();
        let info = r.field("q").unwrap();
        let base = r.max_doc() as u32 - 100;
        let terms: Vec<_> = (0..100)
            .map(|i| TermPostings::new(format!("q{i:03}").into(), vec![(base + i, 1)]))
            .collect();
        let suffix = format!(
            "Lucene104_{}",
            info.attributes["PerFieldPostingsFormat.suffix"]
        );
        let t = r.terms("t").unwrap();
        let fields = [(info, &terms[..]), (r.field("t").unwrap(), &t[..])];
        let ours = write_terms(&fields, &r.seg.info.id, &suffix, &[]).unwrap();
        for (ext, ours) in [("tim", &ours.tim), ("tip", &ours.tip)] {
            let lucene = r.cfs_file(&format!("_{suffix}.{ext}")).unwrap();
            assert_eq!(lucene, &ours[..], ".{ext}");
        }
    }

    /// Field "plain" (positions, no offsets or payloads) of tests/data/positions, written by
    /// Lucene 10.5.1. Its postings come last in `.doc` (fields go by name), whose skip data
    /// points into `.pos`, which must match too.
    #[test]
    fn writes_the_positions_lucene_writes() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/positions");
        let seg = crate::SegmentInfos::read_latest(&dir)
            .unwrap()
            .segments
            .remove(0);
        let r = SegmentReader::open(&dir, seg).unwrap();
        let info = r.field("plain").unwrap();
        let terms = r.terms("plain").unwrap();
        let norms = r.norms("plain").unwrap().unwrap();
        let norms: Vec<i8> = norms.into_iter().map(|n| n as i8).collect();
        let suffix = format!(
            "Lucene104_{}",
            info.attributes["PerFieldPostingsFormat.suffix"]
        );
        let fields = [(info, &terms[..])];
        let ours = write_terms(&fields, &r.seg.info.id, &suffix, &[(info.number, &norms)]).unwrap();
        let mut header = crate::output::Output::default();
        header.index_header("Lucene104PostingsWriterDoc", 0, &r.seg.info.id, &suffix);
        let lucene = r.cfs_file(&format!("_{suffix}.doc")).unwrap();
        let body = &ours.doc[header.0.len()..ours.doc.len() - FOOTER_LEN as usize];
        let lucene = &lucene[..lucene.len() - FOOTER_LEN as usize];
        assert!(body.len() > 100, "{}", body.len());
        assert_eq!(&lucene[lucene.len() - body.len()..], body);
        let mut header = crate::output::Output::default();
        header.index_header("Lucene104PostingsWriterPos", 0, &r.seg.info.id, &suffix);
        let lucene = r.cfs_file(&format!("_{suffix}.pos")).unwrap();
        let body = &ours.pos[header.0.len()..ours.pos.len() - FOOTER_LEN as usize];
        let lucene = &lucene[..lucene.len() - FOOTER_LEN as usize];
        assert!(body.len() > 100, "{}", body.len());
        assert_eq!(&lucene[lucene.len() - body.len()..], body);
        let infos = [info.clone()];
        let read = decode_terms(&ours, &r.seg.info.id, &suffix, &infos, info, None).unwrap();
        assert_eq!(read, terms);
    }

    #[test]
    fn positions_round_trip() {
        // Singletons over a block of positions, level-1 skip data (9000 docs), and terms whose
        // positions fill blocks exactly or leave a tail.
        let pos = |ps: &[u32]| -> Vec<Position> {
            ps.iter()
                .map(|&position| Position {
                    position,
                    offsets: None,
                    payload: vec![],
                })
                .collect()
        };
        let term = |t: &str, docs: Vec<(u32, Vec<u32>)>| TermPostings {
            term: t.into(),
            total_term_freq: docs.iter().map(|d| d.1.len() as u64).sum(),
            docs: docs.iter().map(|d| (d.0, d.1.len() as u32)).collect(),
            positions: docs.iter().map(|d| pos(&d.1)).collect(),
        };
        let terms = [
            term("a", vec![(3, vec![0, 0, 7, 70000])]),
            term("b", vec![(4, (0..300).map(|i| i * 2).collect())]),
            term(
                "c",
                (0..9000).map(|d| (d, (0..1 + d % 3).collect())).collect(),
            ),
            term("d", (0..512).map(|d| (d * 2, vec![d % 5])).collect()),
            term("e", vec![(1, vec![5]), (2, vec![1, 2, 3])]),
        ];
        let mut info = field("f", 0, IndexOptions::DocsAndFreqsAndPositions);
        info.omit_norms = false;
        let norms: Vec<i8> = (0..18000).map(|d| (d % 40 + 1) as i8).collect();
        let id = [3u8; 16];
        let files = write_terms(&[(&info, &terms)], &id, "Lucene104_0", &[(0, &norms)]).unwrap();
        let fields = [info.clone()];
        let read = decode_terms(&files, &id, "Lucene104_0", &fields, &info, None).unwrap();
        assert_eq!(read, terms);
    }

    #[test]
    fn lowercase_ascii_round_trips_exceptions() {
        // Upper-case exceptions 600 bytes apart need filler exceptions between them.
        let mut input: Vec<u8> = (0..1300).map(|i| b"abc-xyz_09"[i % 10]).collect();
        input[3] = b'Q';
        input[603] = 0xff;
        let c = compress_lowercase_ascii(&input).unwrap();
        assert!(c.len() < input.len());
        let back = lowercase_ascii(&mut DataInput::new(&c), input.len()).unwrap();
        assert_eq!(back, input);
        assert_eq!(compress_lowercase_ascii(b"short"), None);
        assert_eq!(compress_lowercase_ascii(b"MOSTLY UPPER CASE"), None);
    }

    #[test]
    fn round_trips_floor_blocks_and_nested_sub_blocks() {
        // The empty term, terms that prefix others, 1000 terms under one prefix (floor blocks
        // holding sub-blocks), runs of singletons and terms with several docs and freqs.
        let mut names = vec![String::new(), "a".into(), "ab".into()];
        names.extend((0..1000).map(|i| format!("id{i}")));
        names.extend((0..300).map(|i| format!("z{}", i * 7919 % 1000)));
        names.sort();
        names.dedup();
        let terms: Vec<_> = names
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let i = i as u32;
                let docs = match i % 5 {
                    0 => vec![(i, 1), (i + 5000, 3)],
                    1 => vec![(i, 4)],
                    _ => vec![(i, 1)],
                };
                TermPostings::new(t.clone().into_bytes(), docs)
            })
            .collect();
        let info = field("f", 0, IndexOptions::DocsAndFreqs);
        let id = [7u8; 16];
        let files = write_terms(&[(&info, &terms)], &id, "Lucene104_0", &[]).unwrap();
        let fields = [info.clone()];
        let read = decode_terms(&files, &id, "Lucene104_0", &fields, &info, None).unwrap();
        assert_eq!(read, terms);
    }

    #[test]
    fn looks_up_a_term_by_seeking_through_the_trie() {
        let terms: Vec<_> = (0..1000u32)
            .map(|i| TermPostings::new(format!("t{i:04}").into(), vec![(i, 1)]))
            .collect();
        let info = field("f", 0, IndexOptions::Docs);
        let (id, suffix) = ([9u8; 16], "Lucene104_0");
        let mut files = write_terms(&[(&info, &terms)], &id, suffix, &[]).unwrap();
        // Empty the first block (t0000..t0029, right after the header): a walk over every block
        // would now miss terms, but a lookup elsewhere never reads it.
        let input = &mut DataInput::new(&files.tim);
        codec_util::check_index_header(input, "BlockTreeTermsDict", 0, 0, &id, suffix).unwrap();
        let mut tim = files.tim[..files.tim.len() - FOOTER_LEN as usize].to_vec();
        tim[input.pos] = 1; // no entries, last block
        files.tim = crate::output::Output(tim).finish();
        let fields = [info.clone()];
        let get = |t: &str| decode_terms(&files, &id, suffix, &fields, &info, Some(t.as_bytes()));
        for i in [30, 99, 100, 512, 999] {
            assert_eq!(get(&format!("t{i:04}")).unwrap(), [terms[i].clone()]);
        }
        for absent in ["", "t", "t1", "t0999x", "u"] {
            assert_eq!(get(absent).unwrap(), [], "{absent}");
        }
    }

    #[test]
    fn write_then_read_terms_round_trips() {
        let term = |t: &str, docs: &[(u32, u32)]| TermPostings {
            term: t.into(),
            total_term_freq: docs.iter().map(|d| d.1 as u64).sum(),
            docs: docs.to_vec(),
            positions: vec![],
        };
        let a = [
            term("apple", &[(0, 2), (3, 1), (9, 5)]),
            term("banana", &[(4, 1)]),
            term("cherry", &[(1, 1), (2, 3)]),
        ];
        let b: Vec<_> = (0..48u32)
            .map(|i| term(&format!("k{i:02}"), &[(i, 1), (i + 100, 1)]))
            .collect();
        let (fa, fb) = (
            field("a", 0, IndexOptions::DocsAndFreqs),
            field("b", 1, IndexOptions::Docs),
        );
        let id = [5u8; 16];
        let files = write_terms(&[(&fa, &a), (&fb, &b)], &id, "Lucene104_0", &[]).unwrap();
        let fields = [fa.clone(), fb.clone()];
        let read = |f| decode_terms(&files, &id, "Lucene104_0", &fields, f, None).unwrap();
        assert_eq!(read(&fa), a);
        assert_eq!(read(&fb), b);
    }
}
