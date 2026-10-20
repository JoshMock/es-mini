//! Terms dictionary and postings (Lucene103 block tree + Lucene104 postings).
//!
//! Terms are enumerated by walking blocks from the root, so the `.tip` trie is only consulted
//! for the root block's file pointer. Postings are decoded for terms with fewer than
//! [`BLOCK_SIZE`] docs (singleton or group-varint tail); larger terms use packed blocks, which
//! are not ported yet. Positions, offsets, payloads, and impacts are skipped.

use crate::{
    CompoundFile, Result, SegmentCommitInfo, codec_util, err,
    field_infos::{FieldInfo, IndexOptions},
    input::DataInput,
    stored_fields::{lz4, zigzag},
};

/// Doc count at which postings switch from group-varint to packed blocks.
pub const BLOCK_SIZE: u64 = 256;

/// One term of a field and its postings. `freq` is 1 for every doc when the field omits freqs.
#[derive(Debug, Clone, PartialEq)]
pub struct TermPostings {
    pub term: Vec<u8>,
    pub total_term_freq: u64,
    /// `(doc id, freq)`, ascending by doc id.
    pub docs: Vec<(u32, u32)>,
}

/// Every term of `field` in term order with its postings.
pub fn read_terms(
    cfs: &CompoundFile,
    seg: &SegmentCommitInfo,
    fields: &[FieldInfo],
    field: &str,
) -> Result<Vec<TermPostings>> {
    let info = fields
        .iter()
        .find(|f| f.name == field)
        .ok_or_else(|| crate::Error(format!("no field {field}")))?;
    let attr = |k: &str| info.attributes.get(&format!("PerFieldPostingsFormat.{k}"));
    let (Some(format), Some(suffix)) = (attr("format"), attr("suffix")) else {
        return err(format!("field {field} has no postings"));
    };
    if format != "Lucene104" {
        return err(format!("postings format {format} is not supported"));
    }
    let suffix = format!("{format}_{suffix}");
    let file = |ext: &str| {
        cfs.file(&format!("_{suffix}.{ext}"))
            .ok_or_else(|| crate::Error(format!("{} has no .{ext} for {suffix}", seg.name)))
    };
    let id = &seg.info.id;
    let (tim, tip, tmd, doc) = (file("tim")?, file("tip")?, file("tmd")?, file("doc")?);
    for (name, f, codec) in [
        ("tim", tim, "BlockTreeTermsDict"),
        ("tip", tip, "BlockTreeTermsIndex"),
        ("doc", doc, "Lucene104PostingsWriterDoc"),
    ] {
        codec_util::check_footer(f)?;
        codec_util::check_index_header(&mut DataInput::new(f), codec, 0, 0, id, &suffix)
            .map_err(|e| crate::Error(format!(".{name}: {e}")))?;
    }

    codec_util::check_footer(tmd)?;
    let meta = &mut DataInput::new(tmd);
    codec_util::check_index_header(meta, "BlockTreeTermsMeta", 0, 0, id, &suffix)?;
    codec_util::check_index_header(meta, "Lucene104PostingsWriterTerms", 0, 0, id, &suffix)?;
    if meta.read_vint()? as u64 != BLOCK_SIZE {
        return err("unexpected postings block size");
    }
    for _ in 0..meta.read_len()? {
        let number = meta.read_vint()?;
        let num_terms = meta.read_vlong()?;
        let sum_total_term_freq = meta.read_vlong()?;
        let this = fields
            .iter()
            .find(|f| f.number == number)
            .ok_or_else(|| crate::Error(format!("terms for unknown field {number}")))?;
        if this.index_options != IndexOptions::Docs {
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
            info,
            out: Vec::new(),
        };
        walker.block(root_block as usize, &[])?;
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
fn trie_root_output_fp(tip: &[u8], start: usize, end: usize, root: usize) -> Result<u64> {
    let node = start + root;
    let long_at = |pos: usize| -> Result<u64> {
        match tip.get(pos..pos + 8).filter(|_| pos + 8 <= end + 8) {
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
        // SIGN_SINGLE_CHILD_WITHOUT_OUTPUT
        1 => err("trie root has no output"),
        // SIGN_SINGLE_CHILD_WITH_OUTPUT
        2 => {
            let child_delta_bytes_minus_1 = (flags >> 2) & 7;
            let n = (flags >> 5) & 7;
            let at = node + child_delta_bytes_minus_1 as usize + 3;
            Ok((long_at(at)? & mask(n)) >> 2)
        }
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
    info: &'a FieldInfo,
    out: Vec<TermPostings>,
}

/// Running state while decoding one block's term metadata (`IntBlockTermState`).
#[derive(Default)]
struct TermState {
    doc_freq: u64,
    total_term_freq: u64,
    doc_start_fp: u64,
    singleton_doc_id: i64,
    singleton_run: u64,
}

impl Walker<'_> {
    /// Decodes the block at `fp` and its floor blocks, recursing into sub-blocks in term order.
    fn block(&mut self, mut fp: usize, prefix: &[u8]) -> Result<()> {
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
                    let mut out = Vec::with_capacity(len);
                    lz4(input, len, &mut out)?;
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
                    let sub_fp = (fp as i64 - sub_code) as usize;
                    self.block(sub_fp, &term)?;
                    continue;
                }
                self.decode_stats(stats, &mut state)?;
                self.decode_postings(metas, &mut state, first_term)?;
                first_term = false;
                let docs = self.docs(&state)?;
                self.out.push(TermPostings {
                    term,
                    total_term_freq: state.total_term_freq,
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
            s.doc_start_fp = 0;
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
            metas.read_vlong()?; // posStartFP
            if self.info.index_options >= IndexOptions::DocsAndFreqsAndPositionsAndOffsets
                || self.info.store_payloads
            {
                metas.read_vlong()?; // payStartFP
            }
            if s.total_term_freq > BLOCK_SIZE {
                metas.read_vlong()?; // lastPosBlockOffset
            }
        }
        Ok(())
    }

    fn docs(&self, s: &TermState) -> Result<Vec<(u32, u32)>> {
        if s.doc_freq == 1 {
            return Ok(vec![(s.singleton_doc_id as u32, s.total_term_freq as u32)]);
        }
        if s.doc_freq >= BLOCK_SIZE {
            return err("terms with 256 or more docs use packed postings blocks, not ported yet");
        }
        let n = s.doc_freq as usize;
        let input = &mut DataInput::new(self.doc);
        input.pos = s.doc_start_fp as usize;
        let mut docs = read_group_vints(input, n)?;
        let has_freqs = self.info.index_options != IndexOptions::Docs;
        let mut freqs = vec![1; n];
        if has_freqs {
            for (d, f) in docs.iter_mut().zip(&mut freqs) {
                *f = if *d & 1 == 1 {
                    1
                } else {
                    input.read_vint()? as u32
                };
                *d >>= 1;
            }
        }
        let mut doc = -1i64;
        Ok(docs
            .into_iter()
            .zip(freqs)
            .map(|(delta, freq)| {
                doc += delta as i64;
                (doc as u32, freq)
            })
            .collect())
    }
}

/// `GroupVIntUtil.readGroupVInts`: groups of four ints behind a length byte, then plain vints.
fn read_group_vints(input: &mut DataInput, n: usize) -> Result<Vec<u32>> {
    let mut out = Vec::with_capacity(n);
    while out.len() + 4 <= n {
        let flag = input.read_byte()?;
        for shift in [6, 4, 2, 0] {
            let bytes = ((flag >> shift) & 3) as usize + 1;
            let mut v = [0u8; 4];
            v[..bytes].copy_from_slice(input.read_bytes(bytes)?);
            out.push(u32::from_le_bytes(v));
        }
    }
    while out.len() < n {
        out.push(input.read_vint()? as u32);
    }
    Ok(out)
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

    #[test]
    fn group_varints_then_plain_vints() {
        // Lengths 1, 2, 3, 4 bytes (flag 0b00_01_10_11), then the tail as vints 5 and 200.
        let mut b = vec![
            0b0001_1011,
            1,
            0x2c,
            0x01,
            0x70,
            0x11,
            0x01,
            0xff,
            0xff,
            0xff,
            0x7f,
        ];
        b.extend([5, 0xc8, 0x01]);
        let got = read_group_vints(&mut DataInput::new(&b), 6).unwrap();
        assert_eq!(got, [1, 300, 70000, 0x7fff_ffff, 5, 200]);
    }
}
