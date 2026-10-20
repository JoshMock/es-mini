//! Lucene90 doc values (`.dvm` metadata, `.dvd` data).
//!
//! Fields are decoded eagerly into per-doc vectors. Binary and (single-valued) sorted fields,
//! and the `.dvs` skip index, are not supported: skip-index metadata is skipped over.

use crate::{
    CompoundFile, Result, SegmentCommitInfo,
    codec_util::{self, ID_LEN},
    err,
    field_infos::FieldInfo,
    input::DataInput,
    segment_infos::radix36,
    stored_fields::lz4,
};
use std::path::Path;

const VERSION_MAX: i32 = 2;
/// Doc id block index of the IndexedDISI terminator block (`NO_MORE_DOCS >>> 16`).
const DISI_END_BLOCK: u16 = 0x7fff;
const TERMS_DICT_BLOCK_LZ4_SHIFT: u32 = 6;

/// A field's doc values, one entry per doc id (empty when the doc has no value).
#[derive(Debug, Clone, PartialEq)]
pub enum DocValues {
    /// Numeric and sorted numeric fields; doubles are sortable-long encoded.
    Numbers(Vec<Vec<i64>>),
    /// Sorted set fields, as terms in ordinal order.
    Terms(Vec<Vec<Vec<u8>>>),
}

/// Decodes a field of a segment, from the field's latest doc values generation.
pub fn read_field_values(
    dir: &Path,
    seg: &SegmentCommitInfo,
    cfs: &CompoundFile,
    fields: &[FieldInfo],
    field: &str,
) -> Result<DocValues> {
    let info = fields
        .iter()
        .find(|f| f.name == field)
        .ok_or_else(|| crate::Error(format!("no field {field}")))?;
    let id = &seg.info.id;
    let max_doc = seg.info.max_doc;
    match info.dv_gen {
        -1 => {
            let file = |ext: &str| {
                cfs.file(&format!("_Lucene90_0.{ext}"))
                    .ok_or_else(|| crate::Error(format!("{} has no doc values .{ext}", seg.name)))
            };
            decode(
                file("dvm")?,
                file("dvd")?,
                id,
                "Lucene90_0",
                fields,
                max_doc,
                info.number,
            )
        }
        g => {
            // Updated doc values live outside the compound file, named by generation.
            let suffix = format!("{}_Lucene90_0", radix36(g as u64));
            let file = |ext: &str| {
                let p = dir.join(format!("{}_{suffix}.{ext}", seg.name));
                std::fs::read(&p).map_err(|e| crate::Error(format!("{}: {e}", p.display())))
            };
            decode(
                &file("dvm")?,
                &file("dvd")?,
                id,
                &suffix,
                fields,
                max_doc,
                info.number,
            )
        }
    }
}

/// Location of the `IndexedDISI` listing docs with a value; offset -2 is none, -1 is all docs.
#[derive(Debug, Clone, Copy)]
struct DocsWithField {
    offset: i64,
    length: i64,
    jump_table_entries: i16,
    dense_rank_power: i8,
}

fn read_docs_with_field(meta: &mut DataInput) -> Result<DocsWithField> {
    Ok(DocsWithField {
        offset: meta.read_long()?,
        length: meta.read_long()?,
        jump_table_entries: meta.read_short()?,
        dense_rank_power: meta.read_byte()? as i8,
    })
}

fn slice(data: &[u8], offset: i64, length: i64) -> Result<&[u8]> {
    usize::try_from(offset)
        .ok()
        .zip(usize::try_from(length).ok())
        .and_then(|(o, l)| data.get(o..o.checked_add(l)?))
        .map_or_else(|| err("doc values slice extends past the data file"), Ok)
}

impl DocsWithField {
    /// Decodes the `IndexedDISI` into sorted doc ids.
    fn doc_ids(&self, data: &[u8], max_doc: i32) -> Result<Vec<u32>> {
        match self.offset {
            -2 => return Ok(vec![]),
            -1 => return Ok((0..max_doc as u32).collect()),
            _ => {}
        }
        let jump_bytes = 8 * self.jump_table_entries.max(0) as i64;
        let input = &mut DataInput::new(slice(data, self.offset, self.length - jump_bytes)?);
        let mut docs = Vec::new();
        loop {
            let block = input.read_short()? as u16;
            let card = input.read_short()? as u16 as usize + 1;
            if block == DISI_END_BLOCK {
                return Ok(docs);
            }
            let base = (block as u32) << 16;
            if card > 4095 {
                if card == 1 << 16 {
                    docs.extend(base..base + (1 << 16));
                    continue;
                }
                if self.dense_rank_power != -1 {
                    input.read_bytes(2 << (16 - self.dense_rank_power))?; // rank table
                }
                for word in 0..1024 {
                    let bits = input.read_long()? as u64;
                    docs.extend(
                        (0..64)
                            .filter(|b| bits >> b & 1 == 1)
                            .map(|b| base + word * 64 + b),
                    );
                }
            } else {
                for _ in 0..card {
                    docs.push(base + input.read_short()? as u16 as u32);
                }
            }
        }
    }
}

/// `DirectMonotonicReader.Meta`: per block `(min, avg, offset, bits per value)`.
struct DirectMonotonic {
    block_shift: u32,
    blocks: Vec<(i64, f32, i64, u8)>,
}

fn read_direct_monotonic(
    meta: &mut DataInput,
    num_values: u64,
    block_shift: i32,
) -> Result<DirectMonotonic> {
    if !(2..=22).contains(&block_shift) {
        return err(format!("invalid block shift {block_shift}"));
    }
    let blocks = (0..num_values.div_ceil(1 << block_shift))
        .map(|_| {
            Ok((
                meta.read_long()?,
                f32::from_bits(meta.read_int()? as u32),
                meta.read_long()?,
                meta.read_byte()?,
            ))
        })
        .collect::<Result<_>>()?;
    Ok(DirectMonotonic {
        block_shift: block_shift as u32,
        blocks,
    })
}

/// `DirectReader.get`: LSB-first bit-packed value `index` of `bits` bits, starting at byte 0.
fn direct_get(bytes: &[u8], bits: u8, index: u64) -> Result<i64> {
    if bits == 0 {
        return Ok(0);
    }
    let bit = index * bits as u64;
    let (byte, shift) = ((bit >> 3) as usize, bit & 7);
    let end = (byte + 8).min(bytes.len());
    let Some(chunk) = bytes
        .get(byte..end)
        .filter(|c| c.len() as u64 * 8 >= shift + bits as u64)
    else {
        return err("packed value extends past its slice");
    };
    let mut word = [0u8; 8];
    word[..chunk.len()].copy_from_slice(chunk);
    let mask = if bits == 64 { !0 } else { (1u64 << bits) - 1 };
    Ok((u64::from_le_bytes(word) >> shift & mask) as i64)
}

impl DirectMonotonic {
    /// The first `n` values, packed in `bytes` (the addresses slice).
    fn values(&self, bytes: &[u8], n: u64) -> Result<Vec<i64>> {
        (0..n)
            .map(|i| {
                let (min, avg, offset, bits) = self.blocks[(i >> self.block_shift) as usize];
                let in_block = i & ((1 << self.block_shift) - 1);
                let packed = bytes.get(offset as usize..).unwrap_or_default();
                let delta = direct_get(packed, bits, in_block)?;
                // Java multiplies a float by a long as float, then truncates to long.
                Ok(min + (avg * in_block as f32) as i64 + delta)
            })
            .collect()
    }
}

/// `NumericEntry`: values of docs that have one, in doc order.
struct Numeric {
    docs: DocsWithField,
    num_values: u64,
    table: Option<Vec<i64>>,
    block_shift: i32,
    bits_per_value: u8,
    min_value: i64,
    gcd: i64,
    values_offset: i64,
    values_length: i64,
}

fn read_numeric(meta: &mut DataInput) -> Result<Numeric> {
    let docs = read_docs_with_field(meta)?;
    let num_values = meta.read_long()? as u64;
    let table_size = meta.read_int()?;
    if table_size > 256 {
        return err(format!("invalid table size {table_size}"));
    }
    let table = (table_size >= 0)
        .then(|| {
            (0..table_size)
                .map(|_| meta.read_long())
                .collect::<Result<Vec<_>>>()
        })
        .transpose()?;
    let block_shift = if table_size < -1 { -2 - table_size } else { -1 };
    let n = Numeric {
        docs,
        num_values,
        table,
        block_shift,
        bits_per_value: meta.read_byte()?,
        min_value: meta.read_long()?,
        gcd: meta.read_long()?,
        values_offset: meta.read_long()?,
        values_length: meta.read_long()?,
    };
    meta.read_long()?; // valueJumpTableOffset: blocks are decoded in order instead
    Ok(n)
}

impl Numeric {
    fn values(&self, data: &[u8]) -> Result<Vec<i64>> {
        let n = self.num_values;
        if self.bits_per_value == 0 {
            return Ok(vec![self.min_value; n as usize]);
        }
        let bytes = slice(data, self.values_offset, self.values_length)?;
        if self.block_shift >= 0 {
            // Blocks of varying bits per value: `bpv, delta[, length, packed values]`.
            let input = &mut DataInput::new(bytes);
            let mut out = Vec::with_capacity(n as usize);
            while (out.len() as u64) < n {
                let bits = input.read_byte()?;
                let delta = input.read_long()?;
                let in_block = (n - out.len() as u64).min(1 << self.block_shift);
                let packed = if bits == 0 {
                    &[][..]
                } else {
                    let len = input.read_len()?;
                    input.read_bytes(len)?
                };
                for i in 0..in_block {
                    out.push(
                        self.gcd
                            .wrapping_mul(direct_get(packed, bits, i)?)
                            .wrapping_add(delta),
                    );
                }
            }
            return Ok(out);
        }
        (0..n)
            .map(|i| {
                let v = direct_get(bytes, self.bits_per_value, i)?;
                match &self.table {
                    Some(t) => t
                        .get(v as usize)
                        .copied()
                        .map_or_else(|| err("bad table index"), Ok),
                    None => Ok(self.gcd.wrapping_mul(v).wrapping_add(self.min_value)),
                }
            })
            .collect()
    }
}

/// `SortedNumericEntry`: values plus per-doc value counts.
struct SortedNumeric {
    numeric: Numeric,
    num_docs_with_field: u64,
    addresses: Option<(i64, DirectMonotonic, i64)>,
}

fn read_sorted_numeric(meta: &mut DataInput) -> Result<SortedNumeric> {
    let numeric = read_numeric(meta)?;
    let num_docs_with_field = meta.read_int()? as u64;
    let addresses = if num_docs_with_field != numeric.num_values {
        let offset = meta.read_long()?;
        let block_shift = meta.read_vint()?;
        let mono = read_direct_monotonic(meta, num_docs_with_field + 1, block_shift)?;
        Some((offset, mono, meta.read_long()?))
    } else {
        None
    };
    Ok(SortedNumeric {
        numeric,
        num_docs_with_field,
        addresses,
    })
}

impl SortedNumeric {
    fn per_doc(&self, data: &[u8], max_doc: i32) -> Result<Vec<Vec<i64>>> {
        let docs = self.numeric.docs.doc_ids(data, max_doc)?;
        if docs.len() as u64 != self.num_docs_with_field {
            return err("doc count disagrees with the doc ids");
        }
        let values = self.numeric.values(data)?;
        let ends: Vec<usize> = match &self.addresses {
            None => (1..=docs.len()).collect(),
            Some((offset, mono, length)) => mono
                .values(slice(data, *offset, *length)?, self.num_docs_with_field + 1)?
                .into_iter()
                .skip(1)
                .map(|a| a as usize)
                .collect(),
        };
        let mut out = vec![vec![]; max_doc as usize];
        let mut start = 0;
        for (doc, end) in docs.into_iter().zip(ends) {
            *out.get_mut(doc as usize)
                .ok_or_else(|| crate::Error("doc id out of range".into()))? = values
                .get(start..end)
                .ok_or_else(|| crate::Error("bad value address".into()))?
                .to_vec();
            start = end;
        }
        Ok(out)
    }
}

/// `TermsDictEntry`: where the LZ4-blocked terms live.
struct TermsDict {
    size: u64,
    data_offset: i64,
    data_length: i64,
}

fn read_terms_dict(meta: &mut DataInput) -> Result<TermsDict> {
    let size = meta.read_vlong()? as u64;
    let block_shift = meta.read_int()?;
    read_direct_monotonic(
        meta,
        size.div_ceil(1 << TERMS_DICT_BLOCK_LZ4_SHIFT),
        block_shift,
    )?;
    meta.read_bytes(8)?; // maxTermLength, maxBlockLength
    let (data_offset, data_length) = (meta.read_long()?, meta.read_long()?);
    meta.read_bytes(16)?; // termsAddresses{Offset,Length}
    let index_shift = meta.read_int()?;
    read_direct_monotonic(meta, 1 + size.div_ceil(1 << index_shift), block_shift)?;
    meta.read_bytes(32)?; // termsIndex{Offset,Length}, termsIndexAddresses{Offset,Length}
    Ok(TermsDict {
        size,
        data_offset,
        data_length,
    })
}

impl TermsDict {
    /// All terms in ordinal order: blocks of 64 terms, the first in full and the rest
    /// prefix-compressed inside an LZ4 block.
    fn terms(&self, data: &[u8]) -> Result<Vec<Vec<u8>>> {
        let bytes = slice(data, self.data_offset, self.data_length)?;
        let input = &mut DataInput::new(bytes);
        let mut out: Vec<Vec<u8>> = Vec::with_capacity(self.size as usize);
        while (out.len() as u64) < self.size {
            let len = input.read_len()?;
            let first = input.read_bytes(len)?.to_vec();
            out.push(first.clone());
            // A block holding only its first term has no LZ4 section.
            if input.pos < bytes.len() - 1 {
                let block_len = input.read_len()?;
                let mut buffer = first;
                lz4(input, block_len, &mut buffer)?;
                self.decode_rest(&mut DataInput::new(&buffer[len..]), &mut out)?;
            } else if (out.len() as u64) < self.size {
                return err("terms block ended early");
            }
        }
        Ok(out)
    }

    /// The other terms of a block, up to 63 or the dictionary size.
    fn decode_rest(&self, block: &mut DataInput, out: &mut Vec<Vec<u8>>) -> Result<()> {
        while !out.len().is_multiple_of(64) && (out.len() as u64) < self.size {
            let token = block.read_byte()? as usize;
            let mut prefix = token & 0xf;
            let mut suffix = 1 + (token >> 4);
            if prefix == 15 {
                prefix += block.read_len()?;
            }
            if suffix == 16 {
                suffix += block.read_len()?;
            }
            let previous = out
                .last()
                .ok_or_else(|| crate::Error("no previous term".into()))?;
            let Some(head) = previous.get(..prefix) else {
                return err("term prefix longer than the previous term");
            };
            let mut term = head.to_vec();
            term.extend_from_slice(block.read_bytes(suffix)?);
            out.push(term);
        }
        Ok(())
    }
}

enum Entry {
    Numeric(Numeric),
    SortedNumeric(SortedNumeric),
    /// Multi-valued sorted set: ordinals as sorted numeric.
    SortedSet(SortedNumeric, TermsDict),
    /// Single-valued sorted set (or sorted): one ordinal per doc.
    SortedSetSingle(Numeric, TermsDict),
    Unsupported(&'static str),
}

fn read_entry(meta: &mut DataInput, kind: u8) -> Result<Entry> {
    Ok(match kind {
        0 => Entry::Numeric(read_numeric(meta)?),
        1 => {
            meta.read_bytes(16)?; // dataOffset, dataLength
            read_docs_with_field(meta)?;
            let num_docs_with_field = meta.read_int()? as u64;
            let (min_len, max_len) = (meta.read_int()?, meta.read_int()?);
            if min_len < max_len {
                meta.read_long()?; // addressesOffset
                let block_shift = meta.read_vint()?;
                read_direct_monotonic(meta, 1 + num_docs_with_field, block_shift)?;
                meta.read_long()?; // addressesLength
            }
            Entry::Unsupported("binary")
        }
        2 => {
            let ords = read_numeric(meta)?;
            Entry::SortedSetSingle(ords, read_terms_dict(meta)?)
        }
        3 => match meta.read_byte()? {
            0 => {
                let ords = read_numeric(meta)?;
                Entry::SortedSetSingle(ords, read_terms_dict(meta)?)
            }
            1 => {
                let ords = read_sorted_numeric(meta)?;
                Entry::SortedSet(ords, read_terms_dict(meta)?)
            }
            b => return err(format!("invalid multiValued flag {b}")),
        },
        4 => Entry::SortedNumeric(read_sorted_numeric(meta)?),
        t => return err(format!("invalid doc values type {t}")),
    })
}

fn decode(
    meta: &[u8],
    data: &[u8],
    id: &[u8; ID_LEN],
    suffix: &str,
    fields: &[FieldInfo],
    max_doc: i32,
    field_number: i32,
) -> Result<DocValues> {
    codec_util::check_footer(meta)?;
    let m = &mut DataInput::new(meta);
    let version =
        codec_util::check_index_header(m, "Lucene90DocValuesMetadata", 0, VERSION_MAX, id, suffix)?;
    codec_util::check_footer(data)?;
    let d = &mut DataInput::new(data);
    if codec_util::check_index_header(d, "Lucene90DocValuesData", 0, VERSION_MAX, id, suffix)?
        != version
    {
        return err("doc values meta and data versions differ");
    }
    loop {
        let number = m.read_int()?;
        if number == -1 {
            return err(format!("no doc values for field {field_number}"));
        }
        let info = fields
            .iter()
            .find(|f| f.number == number)
            .ok_or_else(|| crate::Error(format!("doc values for unknown field {number}")))?;
        let kind = m.read_byte()?;
        if info.doc_values_skip_index {
            // offset, length, maxValue, minValue, docCount, maxDocID[, maxValueCount]
            m.read_bytes(if version >= 2 { 36 } else { 32 })?;
        }
        let entry = read_entry(m, kind)?;
        if number != field_number {
            continue;
        }
        return match entry {
            Entry::Numeric(n) => {
                let values = n.values(data)?;
                let docs = n.docs.doc_ids(data, max_doc)?;
                if docs.len() != values.len() {
                    return err("numeric doc count disagrees with value count");
                }
                let mut out = vec![vec![]; max_doc as usize];
                for (doc, v) in docs.into_iter().zip(values) {
                    out[doc as usize] = vec![v];
                }
                Ok(DocValues::Numbers(out))
            }
            Entry::SortedNumeric(s) => Ok(DocValues::Numbers(s.per_doc(data, max_doc)?)),
            Entry::SortedSet(ords, terms) => Ok(DocValues::Terms(resolve(
                ords.per_doc(data, max_doc)?,
                terms.terms(data)?,
            )?)),
            Entry::SortedSetSingle(ords, terms) => {
                let single = SortedNumeric {
                    num_docs_with_field: ords.num_values,
                    numeric: ords,
                    addresses: None,
                };
                Ok(DocValues::Terms(resolve(
                    single.per_doc(data, max_doc)?,
                    terms.terms(data)?,
                )?))
            }
            Entry::Unsupported(what) => err(format!("{what} doc values are not supported")),
        };
    }
}

fn resolve(ords: Vec<Vec<i64>>, terms: Vec<Vec<u8>>) -> Result<Vec<Vec<Vec<u8>>>> {
    ords.into_iter()
        .map(|doc| {
            doc.into_iter()
                .map(|o| {
                    terms
                        .get(o as usize)
                        .cloned()
                        .ok_or_else(|| crate::Error("ordinal out of range".into()))
                })
                .collect()
        })
        .collect()
}
