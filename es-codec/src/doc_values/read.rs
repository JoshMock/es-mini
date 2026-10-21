use super::*;
use crate::{
    Result, SegmentReader, codec_util, err, input::DataInput, lz4, segment_infos::radix36,
};

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

pub(super) fn decode(
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
