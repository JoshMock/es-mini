//! Lucene90 doc values (`.dvm` metadata, `.dvd` data, `.dvs` skip index).
//!
//! Only what live docs need so far: which docs have a value for a field. Entry metadata is
//! skipped, not decoded, for everything else.

use crate::{Result, codec_util, err, field_infos::FieldInfo, input::DataInput};

const VERSION_MAX: i32 = 2;
/// Doc id block index of the IndexedDISI terminator block (`NO_MORE_DOCS >>> 16`).
const DISI_END_BLOCK: u16 = 0x7fff;
const TERMS_DICT_BLOCK_LZ4_SHIFT: u32 = 6;

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

/// `DirectMonotonicReader.loadMeta`; the per-block data is not decoded yet.
fn skip_direct_monotonic_meta(
    meta: &mut DataInput,
    num_values: u64,
    block_shift: i32,
) -> Result<()> {
    let blocks = num_values.div_ceil(1u64 << block_shift);
    meta.read_bytes(21 * blocks as usize)?; // min, avg, offset, bits per value
    Ok(())
}

/// Entry types, as written by `Lucene90DocValuesConsumer`.
#[derive(Debug, Clone, Copy)]
enum Kind {
    Numeric,
    Binary,
    Sorted,
    SortedSet,
    SortedNumeric,
}

fn skip_numeric(meta: &mut DataInput) -> Result<(DocsWithField, u64)> {
    let docs = read_docs_with_field(meta)?;
    let num_values = meta.read_long()? as u64;
    let table_size = meta.read_int()?;
    if table_size > 256 {
        return err(format!("invalid table size {table_size}"));
    }
    meta.read_bytes(8 * table_size.max(0) as usize)?; // table
    // bitsPerValue, minValue, gcd, valuesOffset, valuesLength, valueJumpTableOffset
    meta.read_bytes(1 + 5 * 8)?;
    Ok((docs, num_values))
}

fn skip_sorted_numeric(meta: &mut DataInput) -> Result<DocsWithField> {
    let (docs, num_values) = skip_numeric(meta)?;
    let num_docs_with_field = meta.read_int()?;
    if num_docs_with_field as u64 != num_values {
        meta.read_long()?; // addressesOffset
        let block_shift = meta.read_vint()?;
        skip_direct_monotonic_meta(meta, num_docs_with_field as u64 + 1, block_shift)?;
        meta.read_long()?; // addressesLength
    }
    Ok(docs)
}

fn skip_terms_dict(meta: &mut DataInput) -> Result<()> {
    let size = meta.read_vlong()? as u64;
    let block_shift = meta.read_int()?;
    let addresses = size.div_ceil(1 << TERMS_DICT_BLOCK_LZ4_SHIFT);
    skip_direct_monotonic_meta(meta, addresses, block_shift)?;
    // maxTermLength, maxBlockLength, termsData{Offset,Length}, termsAddresses{Offset,Length}
    meta.read_bytes(4 + 4 + 4 * 8)?;
    let index_shift = meta.read_int()?;
    skip_direct_monotonic_meta(meta, 1 + size.div_ceil(1 << index_shift), block_shift)?;
    meta.read_bytes(4 * 8)?; // termsIndex{Offset,Length}, termsIndexAddresses{Offset,Length}
    Ok(())
}

fn read_entry(meta: &mut DataInput, kind: Kind) -> Result<DocsWithField> {
    match kind {
        Kind::Numeric => Ok(skip_numeric(meta)?.0),
        Kind::Binary => {
            meta.read_bytes(16)?; // dataOffset, dataLength
            let docs = read_docs_with_field(meta)?;
            let num_docs_with_field = meta.read_int()? as u64;
            let (min_len, max_len) = (meta.read_int()?, meta.read_int()?);
            if min_len < max_len {
                meta.read_long()?; // addressesOffset
                let block_shift = meta.read_vint()?;
                skip_direct_monotonic_meta(meta, 1 + num_docs_with_field, block_shift)?;
                meta.read_long()?; // addressesLength
            }
            Ok(docs)
        }
        Kind::Sorted => {
            let (docs, _) = skip_numeric(meta)?;
            skip_terms_dict(meta)?;
            Ok(docs)
        }
        Kind::SortedSet => match meta.read_byte()? {
            0 => read_entry(meta, Kind::Sorted),
            1 => {
                let docs = skip_sorted_numeric(meta)?;
                skip_terms_dict(meta)?;
                Ok(docs)
            }
            b => err(format!("invalid multiValued flag {b}")),
        },
        Kind::SortedNumeric => skip_sorted_numeric(meta),
    }
}

/// Decodes an `IndexedDISI` into its sorted doc ids.
fn read_disi(data: &[u8], e: &DocsWithField) -> Result<Vec<u32>> {
    let jump_bytes = 8 * e.jump_table_entries.max(0) as usize;
    let (off, len) = (e.offset as usize, e.length as usize);
    let Some(body) = len
        .checked_sub(jump_bytes)
        .and_then(|n| data.get(off..off.checked_add(n)?))
    else {
        return err("doc ids extend past the end of the data file");
    };
    let input = &mut DataInput::new(body);
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
            if e.dense_rank_power != -1 {
                input.read_bytes(2 << (16 - e.dense_rank_power))?; // rank table
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

/// The doc ids that have a value in doc values field `field_number`.
///
/// `meta` and `data` are a `.dvm`/`.dvd` pair written with `id` and header suffix `suffix`.
pub fn docs_with_field(
    meta: &[u8],
    data: &[u8],
    id: &[u8; codec_util::ID_LEN],
    suffix: &str,
    fields: &[FieldInfo],
    max_doc: i32,
    field_number: i32,
) -> Result<Vec<u32>> {
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
        let kind = match m.read_byte()? {
            0 => Kind::Numeric,
            1 => Kind::Binary,
            2 => Kind::Sorted,
            3 => Kind::SortedSet,
            4 => Kind::SortedNumeric,
            t => return err(format!("invalid doc values type {t}")),
        };
        if info.doc_values_skip_index {
            // offset, length, maxValue, minValue, docCount, maxDocID[, maxValueCount]
            m.read_bytes(if version >= 2 { 36 } else { 32 })?;
        }
        let docs = read_entry(m, kind)?;
        if number != field_number {
            continue;
        }
        return match docs.offset {
            -2 => Ok(vec![]),
            -1 => Ok((0..max_doc as u32).collect()),
            _ => read_disi(data, &docs),
        };
    }
}
