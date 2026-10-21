//! Terms dictionary and postings (Lucene103 block tree + Lucene104 postings).
//!
//! Terms are enumerated by walking blocks from the root, so the `.tip` trie is only consulted
//! for the root block's file pointer. Impacts and skip data are skipped.

pub use crate::postings::Position;
use crate::{
    Result, SegmentReader, codec_util, err,
    field_infos::{FieldInfo, IndexOptions},
    input::{DataInput, zigzag},
    lz4, postings,
};

/// Deepest nesting of sub-blocks accepted before a `.tim` is called corrupt.
// ponytail: a crafted index could otherwise recurse until the stack overflows; real block trees
// are a handful of levels deep.
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

pub(crate) fn read_terms(r: &SegmentReader, field: &str) -> Result<Vec<TermPostings>> {
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
    let files = TermFiles {
        tim: file("tim")?.to_vec(),
        tip: file("tip")?.to_vec(),
        tmd: file("tmd")?.to_vec(),
        doc: file("doc")?.to_vec(),
        pos: file("pos").unwrap_or_default().to_vec(),
        pay: file("pay").unwrap_or_default().to_vec(),
        psm: Vec::new(),
    };
    decode_terms(&files, &r.seg.info.id, &suffix, &r.fields, info)
}

/// The postings files of one segment's postings format instance.
#[derive(Default)]
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
) -> Result<Vec<TermPostings>> {
    let field = info.name.as_str();
    let (tim, tip, tmd, doc, pos, pay) = (
        &files.tim[..],
        &files.tip[..],
        &files.tmd[..],
        &files.doc[..],
        &files.pos[..],
        &files.pay[..],
    );
    for (name, f, codec) in [
        ("tim", tim, "BlockTreeTermsDict"),
        ("tip", tip, "BlockTreeTermsIndex"),
        ("doc", doc, "Lucene104PostingsWriterDoc"),
        ("pos", pos, "Lucene104PostingsWriterPos"),
        ("pay", pay, "Lucene104PostingsWriterPay"),
    ] {
        if f.is_empty() {
            continue;
        }
        codec_util::check_footer(f)?;
        codec_util::check_index_header(&mut DataInput::new(f), codec, 0, 0, id, suffix)
            .map_err(|e| crate::Error(format!(".{name}: {e}")))?;
    }

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
        let root_block = trie_root_output_fp(
            tip,
            index_start as usize,
            index_end as usize,
            root_fp as usize,
        )?;
        let mut walker = Walker {
            tim,
            doc,
            pos,
            pay,
            info,
            out: Vec::new(),
        };
        walker.block(root_block as usize, &[], 0)?;
        let total: u64 = walker.out.iter().map(|t| t.total_term_freq).sum();
        if walker.out.len() as i64 != num_terms || total != sum_total_term_freq as u64 {
            return err(format!(
                "terms of {field} do not match the committed statistics"
            ));
        }
        return Ok(walker.out);
    }
    err(format!("field {field} has no terms"))
}

/// `TrieReader.load` for the root node: the file pointer of the root block in `.tim`.
/// Writes the terms dictionary and docs-only/freqs postings of `fields` (no positions): one leaf block per field. Lucene's writer caps
/// leaf blocks at 48 terms for lookup speed; its reader accepts any count.
///
/// `norms` are the fields' norms per doc, for the competitive impacts of freq fields.
///
/// ponytail: no floor blocks, prefix sharing, suffix compression, singleton runs, or `.pos`/`.pay`.
pub fn write_terms(
    fields: &[(&FieldInfo, &[TermPostings])],
    id: &[u8; codec_util::ID_LEN],
    suffix: &str,
    norms: &[(i32, &[i8])],
) -> crate::Result<TermFiles> {
    use crate::output::Output;
    let (mut tim, mut tip, mut tmd, mut doc) = (
        Output::default(),
        Output::default(),
        Output::default(),
        Output::default(),
    );
    tim.index_header("BlockTreeTermsDict", 0, id, suffix);
    tip.index_header("BlockTreeTermsIndex", 0, id, suffix);
    doc.index_header("Lucene104PostingsWriterDoc", 0, id, suffix);
    tmd.index_header("BlockTreeTermsMeta", 0, id, suffix);
    tmd.index_header("Lucene104PostingsWriterTerms", 0, id, suffix);
    tmd.vint(postings::BLOCK_SIZE as i32);
    tmd.vint(fields.len() as i32);
    let mut max = postings::ImpactMax::default();
    for (info, terms) in fields {
        let field_norms = norms.iter().find(|n| n.0 == info.number).map(|n| n.1);
        let options = info.index_options;
        if terms.is_empty() || !matches!(options, IndexOptions::Docs | IndexOptions::DocsAndFreqs) {
            return crate::err(format!(
                "field {} needs terms and docs-only or freqs postings",
                info.name
            ));
        }
        let has_freqs = options != IndexOptions::Docs;
        let (mut suffixes, mut lengths, mut stats, mut metas) = (
            Output::default(),
            Output::default(),
            Output::default(),
            Output::default(),
        );
        let (mut last_fp, mut sum_df, mut docs_seen) =
            (0u64, 0u64, std::collections::BTreeSet::new());
        for t in *terms {
            suffixes.bytes(&t.term);
            lengths.vint(t.term.len() as i32);
            let df = t.docs.len() as u64;
            sum_df += df;
            docs_seen.extend(t.docs.iter().map(|d| d.0));
            stats.vint((df << 1) as i32);
            if has_freqs {
                stats.vlong((t.total_term_freq - df) as i64);
            }
            if df == 1 {
                metas.vlong(0);
                metas.vint(t.docs[0].0 as i32);
            } else {
                let fp = doc.0.len() as u64;
                metas.vlong(((fp - last_fp) << 1) as i64);
                last_fp = fp;
                postings::write_docs(&mut doc, &t.docs, has_freqs, field_norms, &mut max);
            }
        }
        let block_fp = tim.0.len() as i64;
        tim.vint(((terms.len() << 1) | 1) as i32);
        tim.vlong(((suffixes.0.len() << 3) | 4) as i64); // leaf, uncompressed
        tim.bytes(&suffixes.0);
        tim.vint((lengths.0.len() << 1) as i32);
        tim.bytes(&lengths.0);
        for part in [&stats, &metas] {
            tim.vint(part.0.len() as i32);
            tim.bytes(&part.0);
        }
        let index_start = tip.0.len() as i64;
        let fp_bytes = (8 - (block_fp as u64).leading_zeros() as usize / 8).max(1);
        tip.byte((((fp_bytes - 1) << 2) | 0x20) as u8); // leaf node with terms
        tip.bytes(&(block_fp as u64).to_le_bytes()[..fp_bytes]);
        tip.bytes(&[0; 8]); // Lucene's TrieReader reads a long at the node, possibly past its end
        tmd.vint(info.number);
        tmd.vlong(terms.len() as i64);
        tmd.vlong(terms.iter().map(|t| t.total_term_freq).sum::<u64>() as i64);
        if has_freqs {
            tmd.vlong(sum_df as i64);
        }
        tmd.vint(docs_seen.len() as i32);
        for t in [&terms[0], &terms[terms.len() - 1]] {
            tmd.vint(t.term.len() as i32);
            tmd.bytes(&t.term);
        }
        tmd.vlong(index_start);
        tmd.vlong(0);
        tmd.vlong(tip.0.len() as i64);
    }
    // Full file lengths (footers included): `.tip` first, then `.tim`.
    tmd.long(tip.0.len() as i64 + FOOTER_LEN);
    tmd.long(tim.0.len() as i64 + FOOTER_LEN);
    let doc = doc.finish();
    let mut psm = Output::default();
    psm.index_header("Lucene104PostingsWriterMeta", 0, id, suffix);
    for v in [
        max.level0_num,
        max.level0_bytes,
        max.level1_num,
        max.level1_bytes,
    ] {
        psm.bytes(&v.to_le_bytes());
    }
    psm.long(doc.len() as i64);
    Ok(TermFiles {
        tim: tim.finish(),
        tip: tip.finish(),
        tmd: tmd.finish(),
        psm: psm.finish(),
        doc,
        ..Default::default()
    })
}

const FOOTER_LEN: i64 = 16;

fn trie_root_output_fp(tip: &[u8], start: usize, end: usize, root: usize) -> Result<u64> {
    let Some(node) = start.checked_add(root) else {
        return err("trie root extends past the index");
    };
    let long_at = |pos: usize| -> Result<u64> {
        // Like Lucene, read 8 bytes at a node near the end of the index: they run into what follows.
        match tip.get(pos..pos + 8).filter(|_| pos <= end) {
            Some(b) => Ok(u64::from_le_bytes(b.try_into().unwrap())),
            None => err("trie node extends past the index"),
        }
    };
    let mask = |bytes_minus_1: u64| match bytes_minus_1 {
        7 => !0,
        n => (1u64 << (8 * (n + 1))) - 1,
    };
    let first = long_at(node)?;
    let flags = first as u32 as u64;
    match flags & 3 {
        // SIGN_NO_CHILDREN: leaf
        0 => {
            let n = (flags >> 2) & 7;
            Ok(if n <= 6 {
                (first >> 8) & mask(n)
            } else {
                long_at(node + 1)?
            })
        }
        // SIGN_SINGLE_CHILD_WITH_OUTPUT
        1 => {
            let child_delta_bytes_minus_1 = (flags >> 2) & 7;
            let n = (flags >> 5) & 7;
            let at = node + child_delta_bytes_minus_1 as usize + 3;
            Ok((long_at(at)? & mask(n)) >> 2)
        }
        // SIGN_SINGLE_CHILD_WITHOUT_OUTPUT
        2 => err("trie root has no output"),
        // SIGN_MULTI_CHILDREN
        _ => {
            if flags & 0x20 == 0 {
                return err("trie root has no output");
            }
            let n = (flags >> 6) & 7;
            let l = if n <= 4 {
                first >> 24
            } else {
                long_at(node + 3)?
            };
            Ok((l & mask(n)) >> 2)
        }
    }
}

struct Walker<'a> {
    tim: &'a [u8],
    doc: &'a [u8],
    pos: &'a [u8],
    pay: &'a [u8],
    info: &'a FieldInfo,
    out: Vec<TermPostings>,
}

/// Running state while decoding one block's term metadata (`IntBlockTermState`).
#[derive(Default)]
struct TermState {
    doc_freq: u64,
    total_term_freq: u64,
    doc_start_fp: u64,
    pos_start_fp: u64,
    pay_start_fp: u64,
    singleton_doc_id: i64,
    singleton_run: u64,
}

impl Walker<'_> {
    /// Decodes the block at `fp` and its floor blocks, recursing into sub-blocks in term order.
    fn block(&mut self, mut fp: usize, prefix: &[u8], depth: usize) -> Result<()> {
        if depth > MAX_BLOCK_DEPTH {
            return err("terms blocks nest too deeply");
        }
        loop {
            let input = &mut DataInput::new(self.tim);
            input.pos = fp;
            let code = input.read_vint()?;
            let (entries, is_last) = ((code >> 1) as usize, code & 1 != 0);
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
            for _ in 0..entries {
                let (suffix_len, sub) = if is_leaf {
                    (lengths.read_len()?, None)
                } else {
                    let c = lengths.read_vint()?;
                    let sub = (c & 1 != 0).then(|| lengths.read_vlong()).transpose()?;
                    ((c >> 1) as usize, sub)
                };
                let mut term = prefix.to_vec();
                term.extend_from_slice(suffixes.read_bytes(suffix_len)?);
                if let Some(sub_code) = sub {
                    // Sub-blocks are written before their parent, so the delta is positive.
                    let sub_fp = usize::try_from(fp as i64 - sub_code)
                        .ok()
                        .filter(|_| sub_code > 0)
                        .ok_or_else(|| crate::Error("corrupt sub-block pointer".into()))?;
                    self.block(sub_fp, &term, depth + 1)?;
                    continue;
                }
                self.decode_stats(stats, &mut state)?;
                self.decode_postings(metas, &mut state, first_term)?;
                first_term = false;
                let docs = self.docs(&state)?;
                let positions = self.positions(&state, &docs)?;
                self.out.push(TermPostings {
                    term,
                    total_term_freq: state.total_term_freq,
                    positions,
                    docs,
                });
            }
            if is_last {
                return Ok(());
            }
            fp = block_end;
        }
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
                pos: self.pos,
                pay: self.pay,
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
            self.doc,
            s.doc_start_fp as usize,
            s.doc_freq as usize,
            has_freqs,
        )
    }
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
    use crate::field_infos::DocValuesType;

    fn field(name: &str, number: i32, index_options: IndexOptions) -> FieldInfo {
        FieldInfo {
            name: name.into(),
            number,
            store_term_vector: false,
            omit_norms: true,
            store_payloads: false,
            soft_deletes: false,
            parent: false,
            index_options,
            doc_values: DocValuesType::None,
            doc_values_skip_index: false,
            dv_gen: -1,
            attributes: Default::default(),
            point_dimensions: 0,
            point_index_dimensions: 0,
            point_num_bytes: 0,
            vector_dimension: 0,
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
        let read = |f| decode_terms(&files, &id, "Lucene104_0", &fields, f).unwrap();
        assert_eq!(read(&fa), a);
        assert_eq!(read(&fb), b);
    }
}
