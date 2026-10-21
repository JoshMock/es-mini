//! Lucene90 doc values (`.dvm` metadata, `.dvd` data).
//!
//! Fields are decoded eagerly into per-doc vectors. The `.dvs` skip index is not used: its
//! metadata is skipped over.

use crate::{
    Result, SegmentReader, codec_util,
    direct_monotonic::{direct_bits, pack, write_direct_monotonic},
    err,
    input::DataInput,
    lz4,
    output::Output,
    segment_infos::radix36,
};

const VERSION_MAX: i32 = 2;
/// Doc id block index of the IndexedDISI terminator block (`NO_MORE_DOCS >>> 16`).
const DISI_END_BLOCK: u16 = 0x7fff;
const TERMS_DICT_BLOCK_LZ4_SHIFT: u32 = 6;
const TERMS_DICT_BLOCK_SIZE: usize = 1 << TERMS_DICT_BLOCK_LZ4_SHIFT;
/// Block shift of the direct-monotonic address tables of a terms dictionary.
const TERMS_DICT_ADDRESS_SHIFT: i32 = 16;
const TERMS_DICT_REVERSE_INDEX_SHIFT: i32 = 10;
const TERMS_DICT_REVERSE_INDEX_MASK: usize = (1 << TERMS_DICT_REVERSE_INDEX_SHIFT) - 1;

/// A field's doc values, one entry per doc id (empty when the doc has no value).
#[derive(Debug, Clone, PartialEq)]
pub enum DocValues {
    /// Numeric and sorted numeric fields; doubles are sortable-long encoded.
    Numbers(Vec<Vec<i64>>),
    /// Sorted and sorted set fields: each doc's ordinals in ascending order, which index
    /// `terms` (the field's distinct values in byte order).
    Ordinals {
        ords: Vec<Vec<usize>>,
        terms: Vec<Vec<u8>>,
    },
    /// Binary fields: one value per doc, `None` where the doc has none.
    Bytes(Vec<Option<Vec<u8>>>),
}

/// Decodes a field of a segment, from the field's latest doc values generation.
pub(crate) fn read_field_values(r: &SegmentReader, field: &str) -> Result<DocValues> {
    let info = r.field(field)?;
    let owned;
    let (meta, data, suffix): (&[u8], &[u8], String) = if info.dv_gen == -1 {
        let name = |ext: &str| r.cfs_file(&format!("_Lucene90_0.{ext}"));
        (name("dvm")?, name("dvd")?, "Lucene90_0".into())
    } else {
        // Updated doc values live outside the compound file, named by generation.
        let suffix = format!("{}_Lucene90_0", radix36(info.dv_gen as u64));
        let name = |ext: &str| r.gen_file(&format!("_{suffix}.{ext}"));
        owned = (name("dvm")?, name("dvd")?);
        (&owned.0, &owned.1, suffix)
    };
    let (id, max_doc) = (&r.seg.info.id, r.seg.info.max_doc);
    let skip = |n| Ok(r.field_by_number(n)?.doc_values_skip_index);
    decode(meta, data, (id, max_doc), &suffix, info.number, &skip)
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
                match self.dense_rank_power {
                    -1 => {}
                    p @ 7..=15 => {
                        input.read_bytes(2 << (16 - p))?; // rank table
                    }
                    p => return err(format!("invalid dense rank power {p}")),
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
    match bits {
        0 => return Ok(0),
        1..=64 => {}
        _ => return err(format!("invalid bits per value {bits}")),
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
    if block_shift > 30 {
        return err(format!("invalid numeric block shift {block_shift}"));
    }
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
            let mut out = Vec::new();
            while (out.len() as u64) < n {
                let bits = input.read_byte()?;
                let delta = input.read_long()?;
                let in_block = (n - out.len() as u64).min(1 << self.block_shift);
                let packed = if bits == 0 {
                    &[][..]
                } else {
                    let len = input.read_int()?;
                    let len = usize::try_from(len).or_else(|_| err("negative block length"))?;
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

/// Per-doc start offsets into a values or bytes slice, `DirectMonotonic`-packed in `.dvd`.
struct Addresses {
    offset: i64,
    mono: DirectMonotonic,
    length: i64,
}

/// `num_docs + 1` addresses: the start of each doc's values and the end of the last.
fn read_addresses(meta: &mut DataInput, num_docs: u64) -> Result<Addresses> {
    let offset = meta.read_long()?;
    let block_shift = meta.read_vint()?;
    let mono = read_direct_monotonic(meta, num_docs + 1, block_shift)?;
    Ok(Addresses {
        offset,
        mono,
        length: meta.read_long()?,
    })
}

impl Addresses {
    fn values(&self, data: &[u8], num_docs: u64) -> Result<Vec<usize>> {
        Ok(self
            .mono
            .values(slice(data, self.offset, self.length)?, num_docs + 1)?
            .into_iter()
            .map(|a| a as usize)
            .collect())
    }
}

/// `SortedNumericEntry`: values plus per-doc value counts. A plain `NumericEntry` is the
/// one-value-per-doc case without addresses.
struct SortedNumeric {
    numeric: Numeric,
    num_docs_with_field: u64,
    addresses: Option<Addresses>,
}

impl From<Numeric> for SortedNumeric {
    fn from(numeric: Numeric) -> Self {
        SortedNumeric {
            num_docs_with_field: numeric.num_values,
            numeric,
            addresses: None,
        }
    }
}

fn read_sorted_numeric(meta: &mut DataInput) -> Result<SortedNumeric> {
    let numeric = read_numeric(meta)?;
    let num_docs_with_field = meta.read_int()? as u64;
    let addresses = (num_docs_with_field != numeric.num_values)
        .then(|| read_addresses(meta, num_docs_with_field))
        .transpose()?;
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
            Some(a) => a.values(data, self.num_docs_with_field)?[1..].to_vec(),
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

/// `BinaryEntry`: values are concatenated; fixed-length unless `min_length < max_length`.
struct Binary {
    data_offset: i64,
    data_length: i64,
    docs: DocsWithField,
    num_docs_with_field: u64,
    length: usize,
    addresses: Option<Addresses>,
}

fn read_binary(meta: &mut DataInput) -> Result<Binary> {
    let (data_offset, data_length) = (meta.read_long()?, meta.read_long()?);
    let docs = read_docs_with_field(meta)?;
    let num_docs_with_field = meta.read_int()? as u64;
    let (min_length, max_length) = (meta.read_int()? as usize, meta.read_int()? as usize);
    let addresses = (min_length < max_length)
        .then(|| read_addresses(meta, num_docs_with_field))
        .transpose()?;
    Ok(Binary {
        data_offset,
        data_length,
        docs,
        num_docs_with_field,
        length: max_length,
        addresses,
    })
}

impl Binary {
    fn per_doc(&self, data: &[u8], max_doc: i32) -> Result<Vec<Option<Vec<u8>>>> {
        let docs = self.docs.doc_ids(data, max_doc)?;
        if docs.len() as u64 != self.num_docs_with_field {
            return err("doc count disagrees with the doc ids");
        }
        let bytes = slice(data, self.data_offset, self.data_length)?;
        let starts: Vec<usize> = match &self.addresses {
            None => (0..=docs.len()).map(|i| i * self.length).collect(),
            Some(a) => a.values(data, self.num_docs_with_field)?,
        };
        let mut out = vec![None; max_doc as usize];
        for (i, doc) in docs.into_iter().enumerate() {
            let value = bytes
                .get(starts[i]..starts[i + 1])
                .ok_or_else(|| crate::Error("bad binary value address".into()))?;
            *out.get_mut(doc as usize)
                .ok_or_else(|| crate::Error("doc id out of range".into()))? = Some(value.to_vec());
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
    if !(0..=30).contains(&index_shift) {
        return err(format!("invalid terms index shift {index_shift}"));
    }
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
        let mut out: Vec<Vec<u8>> = Vec::new();
        while (out.len() as u64) < self.size {
            let len = input.read_len()?;
            let first = input.read_bytes(len)?.to_vec();
            out.push(first.clone());
            // A block holding only its first term has no LZ4 section.
            if input.pos < bytes.len() - 1 {
                let block_len = input.read_len()?;
                let mut buffer = first;
                lz4::decompress(input, block_len, &mut buffer)?;
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
    Numeric(SortedNumeric),
    /// Sorted and sorted set: ordinals as (sorted) numeric.
    SortedSet(SortedNumeric, TermsDict),
    Binary(Binary),
}

fn read_entry(meta: &mut DataInput, kind: u8) -> Result<Entry> {
    Ok(match kind {
        0 => Entry::Numeric(read_numeric(meta)?.into()),
        1 => Entry::Binary(read_binary(meta)?),
        2 => {
            let ords = read_numeric(meta)?.into();
            Entry::SortedSet(ords, read_terms_dict(meta)?)
        }
        3 => {
            let ords = match meta.read_byte()? {
                0 => read_numeric(meta)?.into(),
                1 => read_sorted_numeric(meta)?,
                b => return err(format!("invalid multiValued flag {b}")),
            };
            Entry::SortedSet(ords, read_terms_dict(meta)?)
        }
        4 => Entry::Numeric(read_sorted_numeric(meta)?),
        t => return err(format!("invalid doc values type {t}")),
    })
}

fn decode(
    meta: &[u8],
    data: &[u8],
    (id, max_doc): (&[u8; codec_util::ID_LEN], i32),
    suffix: &str,
    field_number: i32,
    has_skip_index: &dyn Fn(i32) -> Result<bool>,
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
        let kind = m.read_byte()?;
        if has_skip_index(number)? {
            // offset, length, maxValue, minValue, docCount, maxDocID[, maxValueCount]
            m.read_bytes(if version >= 2 { 36 } else { 32 })?;
        }
        let entry = read_entry(m, kind)?;
        if number != field_number {
            continue;
        }
        return match entry {
            Entry::Numeric(n) => Ok(DocValues::Numbers(n.per_doc(data, max_doc)?)),
            Entry::SortedSet(ords, dict) => {
                let terms = dict.terms(data)?;
                let ords = ords
                    .per_doc(data, max_doc)?
                    .into_iter()
                    .map(|doc| {
                        doc.into_iter()
                            .map(|o| {
                                usize::try_from(o)
                                    .ok()
                                    .filter(|o| *o < terms.len())
                                    .ok_or_else(|| crate::Error("ordinal out of range".into()))
                            })
                            .collect()
                    })
                    .collect::<Result<_>>()?;
                Ok(DocValues::Ordinals { ords, terms })
            }
            Entry::Binary(b) => Ok(DocValues::Bytes(b.per_doc(data, max_doc)?)),
        };
    }
}

/// `IndexedDISI` of `docs` (ascending, among `max_doc`): block header, then a sorted list, a
/// bitset (more than 4095 docs) or nothing (all 65536), plus the jump table Lucene adds when
/// there are two or more blocks. No dense-rank table.
fn write_disi(data: &mut Output, docs: &[u32]) -> i16 {
    let origin = data.0.len();
    let mut jumps: Vec<(i32, i32)> = vec![];
    let mut i = 0;
    while i < docs.len() {
        let block = docs[i] >> 16;
        let n = docs[i..].iter().take_while(|d| **d >> 16 == block).count();
        while jumps.len() <= block as usize {
            jumps.push((i as i32, (data.0.len() - origin) as i32));
        }
        data.bytes(&(block as u16).to_le_bytes());
        data.bytes(&((n - 1) as u16).to_le_bytes());
        let lows = docs[i..i + n].iter().map(|d| *d as u16);
        if n <= 4095 {
            lows.for_each(|d| data.bytes(&d.to_le_bytes()));
        } else if n < 1 << 16 {
            let mut bits = [0u64; 1024];
            lows.for_each(|d| bits[d as usize >> 6] |= 1 << (d & 63));
            bits.iter().for_each(|w| data.bytes(&w.to_le_bytes()));
        }
        i += n;
    }
    jumps.push((docs.len() as i32, (data.0.len() - origin) as i32));
    data.bytes(&DISI_END_BLOCK.to_le_bytes());
    data.bytes(&0u16.to_le_bytes());
    data.bytes(&0xffffu16.to_le_bytes());
    if jumps.len() == 2 {
        return 0;
    }
    for (index, offset) in &jumps {
        data.bytes(&index.to_le_bytes());
        data.bytes(&offset.to_le_bytes());
    }
    jumps.len() as i16
}

/// Meta for "which docs have a value": none (-2), all (-1), or an `IndexedDISI` written to `data`.
pub(crate) fn write_docs_with_field(
    meta: &mut Output,
    data: &mut Output,
    docs: &[u32],
    max_doc: usize,
) {
    let (offset, length, jumps) = if docs.is_empty() {
        (-2, 0, -1)
    } else if docs.len() == max_doc {
        (-1, 0, -1)
    } else {
        let start = data.0.len();
        let jumps = write_disi(data, docs);
        (start as i64, (data.0.len() - start) as i64, jumps)
    };
    meta.long(offset);
    meta.long(length);
    meta.bytes(&jumps.to_le_bytes());
    meta.byte(0xff);
}

/// `values` belong to `docs` (ascending), out of `max_doc` docs.
fn write_numeric_entry(
    dvm: &mut Output,
    dvd: &mut Output,
    values: &[i64],
    docs: &[u32],
    max_doc: usize,
) {
    let min = values.iter().copied().min().unwrap_or(0);
    let range = values
        .iter()
        .fold(0, |m, v| m.max(v.wrapping_sub(min) as u64));
    let bits = direct_bits(range);
    write_docs_with_field(dvm, dvd, docs, max_doc);
    dvm.long(values.len() as i64);
    dvm.bytes(&(-1i32).to_le_bytes()); // no table, no blocks
    dvm.byte(bits);
    dvm.long(min);
    dvm.long(1);
    dvm.long(dvd.0.len() as i64);
    let packed: Vec<u64> = values.iter().map(|v| v.wrapping_sub(min) as u64).collect();
    let packed = pack(&packed, bits);
    dvd.bytes(&packed);
    dvm.long(packed.len() as i64);
    dvm.long(-1); // no value jump table
}

/// Docs with at least one value, and the sorted values of all docs back to back.
fn flatten(per_doc: &[Vec<i64>]) -> (Vec<u32>, Vec<i64>) {
    let docs = (0..per_doc.len() as u32)
        .filter(|d| !per_doc[*d as usize].is_empty())
        .collect();
    let mut values = vec![];
    for v in per_doc {
        let from = values.len();
        values.extend(v);
        values[from..].sort_unstable();
    }
    (docs, values)
}

/// Sorted-numeric entry: the values, the number of docs with any, and (if some doc has several)
/// the `DirectMonotonic` start of each doc's values.
fn write_sorted_numeric_entry(dvm: &mut Output, dvd: &mut Output, per_doc: &[Vec<i64>]) {
    const SHIFT: i32 = TERMS_DICT_ADDRESS_SHIFT;
    let (docs, values) = flatten(per_doc);
    write_numeric_entry(dvm, dvd, &values, &docs, per_doc.len());
    dvm.bytes(&(docs.len() as i32).to_le_bytes());
    if docs.len() != values.len() {
        let mut starts = vec![0i64];
        for d in &docs {
            starts.push(starts[starts.len() - 1] + per_doc[*d as usize].len() as i64);
        }
        dvm.long(dvd.0.len() as i64);
        dvm.vint(SHIFT);
        let packed = write_direct_monotonic(dvm, &starts, SHIFT);
        dvd.bytes(&packed);
        dvm.long(packed.len() as i64);
    }
}

/// Terms dictionary: blocks of 64 terms, the first in full, the rest prefix-compressed.
fn write_terms_dict(dvm: &mut Output, dvd: &mut Output, terms: &[&[u8]]) {
    const SHIFT: i32 = TERMS_DICT_ADDRESS_SHIFT;
    let start = dvd.0.len();
    let (mut addresses, mut max_len, mut max_block) = (vec![], 0, 0);
    for block in terms.chunks(TERMS_DICT_BLOCK_SIZE) {
        addresses.push((dvd.0.len() - start) as i64);
        dvd.vint(block[0].len() as i32);
        dvd.bytes(block[0]);
        let mut rest = Output::default();
        for (prev, term) in block.iter().zip(&block[1..]) {
            let prefix = prev.iter().zip(*term).take_while(|(a, b)| a == b).count();
            let suffix = term.len() - prefix;
            rest.byte(prefix.min(15) as u8 | ((suffix - 1).min(15) << 4) as u8);
            if prefix >= 15 {
                rest.vint(prefix as i32 - 15);
            }
            if suffix > 15 {
                rest.vint(suffix as i32 - 16);
            }
            rest.bytes(&term[prefix..]);
        }
        max_len = max_len.max(block.iter().map(|t| t.len()).max().unwrap_or(0));
        if block.len() > 1 {
            max_block = max_block.max(rest.0.len());
            dvd.vint(rest.0.len() as i32);
            lz4::literals(dvd, &rest.0);
        }
    }
    let length = dvd.0.len() - start;
    dvm.vlong(terms.len() as i64);
    dvm.bytes(&SHIFT.to_le_bytes());
    let addr_data = write_direct_monotonic(dvm, &addresses, SHIFT);
    dvm.bytes(&(max_len as i32).to_le_bytes());
    dvm.bytes(&(max_block as i32).to_le_bytes());
    dvm.long(start as i64);
    dvm.long(length as i64);
    dvm.long(dvd.0.len() as i64);
    dvm.long(addr_data.len() as i64);
    dvd.bytes(&addr_data);
    // Reverse index: one sort key per 2^INDEX_SHIFT terms.
    const INDEX_SHIFT: i32 = TERMS_DICT_REVERSE_INDEX_SHIFT;
    dvm.bytes(&INDEX_SHIFT.to_le_bytes());
    let (mut keys, mut offsets, mut prev) = (Output::default(), vec![], &[][..]);
    for (ord, term) in terms.iter().enumerate() {
        match ord & TERMS_DICT_REVERSE_INDEX_MASK {
            0 => {
                offsets.push(keys.0.len() as i64);
                let n = if ord == 0 {
                    0
                } else {
                    (prev.iter().zip(*term).take_while(|(a, b)| a == b).count() + 1).min(term.len())
                };
                keys.bytes(&term[..n]);
            }
            TERMS_DICT_REVERSE_INDEX_MASK => prev = term,
            _ => {}
        }
    }
    offsets.push(keys.0.len() as i64);
    let index_start = dvd.0.len();
    dvd.bytes(&keys.0);
    let addr_start = dvd.0.len();
    let index_addr = write_direct_monotonic(dvm, &offsets, SHIFT);
    // meta order: index, then index addresses
    dvm.long(index_start as i64);
    dvm.long(keys.0.len() as i64);
    dvm.long(addr_start as i64);
    dvm.long(index_addr.len() as i64);
    dvd.bytes(&index_addr);
}

/// Returns `(.dvm, .dvd)` for dense `numeric` fields (one `(field number, value per doc)` each)
/// and `keyword` fields as SORTED_SET (`(field number, terms per doc)`; a doc may have none or
/// several).
///
/// ponytail: no table, gcd, or block encoding; add them when size demands.
pub fn write_doc_values(
    numeric: &[(i32, Vec<i64>)],
    keyword: &[(i32, Vec<Vec<Vec<u8>>>)],
    id: &[u8; codec_util::ID_LEN],
    suffix: &str,
) -> (Vec<u8>, Vec<u8>) {
    write_doc_values_with(numeric, &[], keyword, id, suffix)
}

/// Like [`write_doc_values`], plus `sorted_numeric` fields (what ES uses for `long`, `double`,
/// `boolean`, ...): `(field number, values per doc)`.
pub fn write_doc_values_with(
    numeric: &[(i32, Vec<i64>)],
    sorted_numeric: &[(i32, Vec<Vec<i64>>)],
    keyword: &[(i32, Vec<Vec<Vec<u8>>>)],
    id: &[u8; codec_util::ID_LEN],
    suffix: &str,
) -> (Vec<u8>, Vec<u8>) {
    let (mut dvm, mut dvd) = (Output::default(), Output::default());
    dvm.index_header("Lucene90DocValuesMetadata", VERSION_MAX, id, suffix);
    dvd.index_header("Lucene90DocValuesData", VERSION_MAX, id, suffix);
    for (number, values) in numeric {
        dvm.bytes(&number.to_le_bytes());
        dvm.byte(0);
        let docs: Vec<u32> = (0..values.len() as u32).collect();
        write_numeric_entry(&mut dvm, &mut dvd, values, &docs, values.len());
    }
    for (number, values) in sorted_numeric {
        dvm.bytes(&number.to_le_bytes());
        dvm.byte(4);
        write_sorted_numeric_entry(&mut dvm, &mut dvd, values);
    }
    for (number, values) in keyword {
        let mut terms: Vec<&[u8]> = values.iter().flatten().map(|v| &v[..]).collect();
        terms.sort();
        terms.dedup();
        let ords: Vec<Vec<i64>> = values
            .iter()
            .map(|doc| {
                let mut o: Vec<i64> = doc
                    .iter()
                    .map(|v| terms.binary_search(&&v[..]).unwrap_or(0) as i64)
                    .collect();
                o.sort_unstable();
                o.dedup();
                o
            })
            .collect();
        dvm.bytes(&number.to_le_bytes());
        dvm.byte(3);
        if ords.iter().all(|o| o.len() <= 1) {
            dvm.byte(0); // single-valued
            let (docs, ords) = flatten(&ords);
            write_numeric_entry(&mut dvm, &mut dvd, &ords, &docs, values.len());
        } else {
            dvm.byte(1);
            write_sorted_numeric_entry(&mut dvm, &mut dvd, &ords);
        }
        write_terms_dict(&mut dvm, &mut dvd, &terms);
    }
    dvm.bytes(&(-1i32).to_le_bytes());
    (dvm.finish(), dvd.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_then_read_numeric_round_trips() {
        let id = [3u8; 16];
        let a: Vec<i64> = (0..1000).map(|i| i * 37 - 5000).collect();
        let b = vec![i64::MIN, i64::MAX, 0];
        let c = vec![42; 1000];
        let fields = [(0, a.clone()), (1, c.clone())];
        let (m, d) = write_doc_values(&fields, &[], &id, "Lucene90_0");
        let read = |n, len| decode(&m, &d, (&id, len), "Lucene90_0", n, &|_| Ok(false)).unwrap();
        let wrap = |v: Vec<i64>| DocValues::Numbers(v.into_iter().map(|x| vec![x]).collect());
        assert_eq!(read(0, 1000), wrap(a));
        assert_eq!(read(1, 1000), wrap(c));
        let (m, d) = write_doc_values(&[(7, b.clone())], &[], &id, "Lucene90_0");
        let got = decode(&m, &d, (&id, 3), "Lucene90_0", 7, &|_| Ok(false)).unwrap();
        assert_eq!(got, wrap(b));
    }

    #[test]
    fn write_then_read_sorted_numeric_round_trips() {
        let id = [5u8; 16];
        let v = vec![10, -3, 25, 7];
        let per_doc = v.iter().map(|x| vec![*x]).collect();
        let (m, d) = write_doc_values_with(&[], &[(2, per_doc)], &[], &id, "Lucene90_0");
        let got = decode(&m, &d, (&id, 4), "Lucene90_0", 2, &|_| Ok(false)).unwrap();
        assert_eq!(
            got,
            DocValues::Numbers(v.into_iter().map(|x| vec![x]).collect())
        );
    }

    /// Sparse and multi-valued sorted numerics, incl. a bitset block and several 64K blocks.
    #[test]
    fn write_then_read_sparse_multi_valued_round_trips() {
        let id = [6u8; 16];
        let small = vec![vec![], vec![9, 2, 2], vec![], vec![5]];
        let sparse: Vec<Vec<i64>> = (0..70_000)
            .map(|i: i64| if i % 3 == 0 { vec![i] } else { vec![] })
            .collect();
        let dense_block: Vec<Vec<i64>> = (0..5000).map(|i| vec![i * 2, i]).collect();
        let tail = (0..140_000).map(|i| if i > 139_990 { vec![1] } else { vec![] });
        let far: Vec<Vec<i64>> = tail.collect();
        for per_doc in [small, sparse, dense_block, far] {
            let (m, d) =
                write_doc_values_with(&[], &[(2, per_doc.clone())], &[], &id, "Lucene90_0");
            let got = decode(
                &m,
                &d,
                (&id, per_doc.len() as i32),
                "Lucene90_0",
                2,
                &|_| Ok(false),
            )
            .unwrap();
            let sorted: Vec<Vec<i64>> = per_doc
                .into_iter()
                .map(|mut v| {
                    v.sort();
                    v
                })
                .collect();
            assert_eq!(got, DocValues::Numbers(sorted));
        }
    }

    #[test]
    fn write_then_read_sparse_multi_valued_keyword_round_trips() {
        let id = [8u8; 16];
        let t = |s: &str| s.as_bytes().to_vec();
        let vals = vec![
            vec![t("b"), t("a")],
            vec![],
            vec![t("b")],
            vec![t("b"), t("b")],
        ];
        let (m, d) = write_doc_values(&[], &[(1, vals)], &id, "Lucene90_0");
        let got = decode(&m, &d, (&id, 4), "Lucene90_0", 1, &|_| Ok(false)).unwrap();
        let DocValues::Ordinals { ords, terms } = got else {
            panic!("not ordinals")
        };
        assert_eq!(terms, vec![t("a"), t("b")]);
        assert_eq!(ords, vec![vec![0, 1], vec![], vec![1], vec![1]]);
        let sparse_single = vec![vec![], vec![t("x")]];
        let (m, d) = write_doc_values(&[], &[(1, sparse_single)], &id, "Lucene90_0");
        let got = decode(&m, &d, (&id, 2), "Lucene90_0", 1, &|_| Ok(false)).unwrap();
        let DocValues::Ordinals { ords, .. } = got else {
            panic!("not ordinals")
        };
        assert_eq!(ords, vec![vec![], vec![0]]);
    }

    #[test]
    fn write_then_read_keyword_round_trips() {
        let id = [4u8; 16];
        let vals: Vec<Vec<u8>> = (0..300)
            .map(|i| {
                format!(
                    "a-common-prefix-longer-than-fifteen-bytes-{:03}",
                    i * 7 % 150
                )
                .into_bytes()
            })
            .collect();
        let (m, d) = write_doc_values(
            &[(0, vec![5; 300])],
            &[(1, vals.iter().map(|v| vec![v.clone()]).collect())],
            &id,
            "Lucene90_0",
        );
        let got = decode(&m, &d, (&id, 300), "Lucene90_0", 1, &|_| Ok(false)).unwrap();
        let DocValues::Ordinals { ords, terms } = got else {
            panic!("not ordinals")
        };
        assert_eq!(terms.len(), 150);
        let back: Vec<_> = ords.iter().map(|o| terms[o[0]].clone()).collect();
        assert_eq!(back, vals);
    }

    /// The ES 9.5.5 fixture's `tag` terms (animal, misc, pet) and ordinals, byte for byte.
    #[test]
    fn small_keyword_matches_es_fixture_terms_block() {
        let vals = ["pet", "animal", "misc"]
            .map(|v| v.as_bytes().to_vec())
            .to_vec();
        let vals = vals.into_iter().map(|v| vec![v]).collect();
        let (_, d) = write_doc_values(&[], &[(2, vals)], &[0; 16], "x");
        let block = [
            6, b'a', b'n', b'i', b'm', b'a', b'l', 9, 0x90, 0x30, b'm', b'i', b's', b'c', 0x20,
            b'p', b'e', b't',
        ];
        assert!(d.windows(block.len()).any(|w| w == block));
    }
}
