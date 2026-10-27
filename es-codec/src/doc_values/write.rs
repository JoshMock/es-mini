use super::*;
use crate::{
    codec_util,
    direct_monotonic::{direct_bits, pack, write_direct_monotonic},
    lz4,
    output::Output,
};

/// `IndexedDISI` of `docs` (ascending, among `max_doc`): block header, then a sorted list, a
/// bitset (more than 4095 docs) led by its rank table or nothing (all 65536), plus the jump table
/// Lucene adds when there are two or more blocks.
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
            // Rank: docs set before every 2^DENSE_RANK_POWER-th bit, as big-endian u16s.
            let mut set = 0u16;
            for chunk in bits.chunks(1 << (DENSE_RANK_POWER - 6)) {
                data.bytes(&set.to_be_bytes());
                set += chunk.iter().map(|w| w.count_ones() as u16).sum::<u16>();
            }
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
    let (offset, length, jumps, rank_power) = if docs.is_empty() {
        (-2, 0, -1, 0xff)
    } else if docs.len() == max_doc {
        (-1, 0, -1, 0xff)
    } else {
        let start = data.0.len();
        let jumps = write_disi(data, docs);
        let length = (data.0.len() - start) as i64;
        (start as i64, length, jumps, DENSE_RANK_POWER)
    };
    meta.long(offset);
    meta.long(length);
    meta.bytes(&jumps.to_le_bytes());
    meta.byte(rank_power);
}

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 { a } else { gcd(b, a % b) }
}

/// `(min, max - min)` of non-empty `values`.
fn min_range(values: &[i64]) -> (i64, u64) {
    let min = values.iter().copied().min().unwrap_or(0);
    let max = values.iter().copied().max().unwrap_or(0);
    (min, max.wrapping_sub(min) as u64)
}

/// Bits `DirectWriter` packs `values` into, all at once (Lucene's `MinMaxTracker` space).
fn space_in_bits(values: &[i64]) -> u64 {
    match min_range(values).1 {
        0 => 0,
        range => direct_bits(range) as u64 * values.len() as u64,
    }
}

/// `values` belong to `docs` (ascending), out of `max_doc` docs. Picks the encoding
/// Lucene90DocValuesConsumer.writeValues picks: a table of up to 256 distinct values (never for
/// `ords`), blocks of 2^NUMERIC_BLOCK_SHIFT values with their own bits per value when that saves
/// 10% or more, or one `DirectWriter` run of `(v - min) / gcd`.
fn write_numeric_entry(
    dvm: &mut Output,
    dvd: &mut Output,
    values: &[i64],
    docs: &[u32],
    max_doc: usize,
    ords: bool,
) {
    let first = values.first().copied().unwrap_or(0);
    // Values beyond +-2^62 could overflow `v - first`; Lucene gives up on a GCD then.
    let mut gcd = values.iter().fold(0, |g, v| match g {
        1 => 1,
        _ if *v < i64::MIN / 2 || *v > i64::MAX / 2 => 1,
        _ => gcd(g, (v - first).unsigned_abs()),
    });
    let (mut min, range) = min_range(values);
    let max = min.wrapping_add(range as i64);
    let mut unique = values.to_vec();
    unique.sort_unstable();
    unique.dedup();
    let (mut bits, mut table, mut blocks) = (0, None, false);
    write_docs_with_field(dvm, dvd, docs, max_doc);
    dvm.long(values.len() as i64);
    if range == 0 {
        dvm.int(-1);
    } else if !ords
        && unique.len() <= 256
        && direct_bits(unique.len() as u64 - 1) < direct_bits(range / gcd)
    {
        bits = direct_bits(unique.len() as u64 - 1);
        dvm.int(unique.len() as i32);
        unique.iter().for_each(|v| dvm.long(*v));
        (min, gcd, table) = (0, 1, Some(unique));
    } else {
        let block_space: u64 = values.chunks(NUMERIC_BLOCK_SIZE).map(space_in_bits).sum();
        blocks = block_space as f64 / space_in_bits(values) as f64 <= 0.9;
        if blocks {
            bits = 0xff;
            dvm.int(-2 - NUMERIC_BLOCK_SHIFT);
        } else {
            bits = direct_bits(range / gcd);
            if gcd == 1 && min > 0 && direct_bits(max as u64) == bits {
                min = 0;
            }
            dvm.int(-1);
        }
    }
    dvm.byte(bits);
    dvm.long(min);
    dvm.long(gcd as i64);
    let start = dvd.0.len();
    dvm.long(start as i64);
    let encode = |v: &i64| match &table {
        Some(t) => t.binary_search(v).unwrap_or(0) as u64,
        None => v.wrapping_sub(min) as u64 / gcd,
    };
    let mut jump_table = -1;
    if blocks {
        let mut offsets = vec![];
        for block in values.chunks(NUMERIC_BLOCK_SIZE) {
            offsets.push(dvd.0.len() as i64);
            let (min, range) = min_range(block);
            if range == 0 {
                dvd.byte(0);
                dvd.long(min);
            } else {
                let bits = direct_bits(range / gcd);
                let deltas: Vec<u64> = block
                    .iter()
                    .map(|v| v.wrapping_sub(min) as u64 / gcd)
                    .collect();
                let packed = pack(&deltas, bits);
                dvd.byte(bits);
                dvd.long(min);
                dvd.int(packed.len() as i32);
                dvd.bytes(&packed);
            }
        }
        jump_table = dvd.0.len() as i64;
        offsets.iter().for_each(|o| dvd.long(*o));
        dvd.long(jump_table);
    } else if bits != 0 {
        dvd.bytes(&pack(&values.iter().map(encode).collect::<Vec<_>>(), bits));
    }
    dvm.long((dvd.0.len() - start) as i64);
    dvm.long(jump_table);
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
fn write_sorted_numeric_entry(
    dvm: &mut Output,
    dvd: &mut Output,
    per_doc: &[Vec<i64>],
    ords: bool,
) {
    let (docs, values) = flatten(per_doc);
    write_numeric_entry(dvm, dvd, &values, &docs, per_doc.len(), ords);
    dvm.bytes(&(docs.len() as i32).to_le_bytes());
    if docs.len() != values.len() {
        let mut starts = vec![0i64];
        for d in &docs {
            starts.push(starts[starts.len() - 1] + per_doc[*d as usize].len() as i64);
        }
        dvm.long(dvd.0.len() as i64);
        dvm.vint(DIRECT_MONOTONIC_BLOCK_SHIFT);
        let packed = write_direct_monotonic(dvm, &starts, DIRECT_MONOTONIC_BLOCK_SHIFT);
        dvd.bytes(&packed);
        dvm.long(packed.len() as i64);
    }
}

/// Terms dictionary: blocks of 64 terms, the first in full, the rest prefix-compressed.
fn write_terms_dict(dvm: &mut Output, dvd: &mut Output, terms: &[&[u8]]) {
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
    dvm.bytes(&DIRECT_MONOTONIC_BLOCK_SHIFT.to_le_bytes());
    let addr_data = write_direct_monotonic(dvm, &addresses, DIRECT_MONOTONIC_BLOCK_SHIFT);
    dvm.bytes(&(max_len as i32).to_le_bytes());
    dvm.bytes(&(max_block as i32).to_le_bytes());
    dvm.long(start as i64);
    dvm.long(length as i64);
    dvm.long(dvd.0.len() as i64);
    dvm.long(addr_data.len() as i64);
    dvd.bytes(&addr_data);
    // Reverse index: one sort key per 2^TERMS_DICT_REVERSE_INDEX_SHIFT terms.
    dvm.bytes(&TERMS_DICT_REVERSE_INDEX_SHIFT.to_le_bytes());
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
    let index_addr = write_direct_monotonic(dvm, &offsets, DIRECT_MONOTONIC_BLOCK_SHIFT);
    // meta order: index, then index addresses
    dvm.long(index_start as i64);
    dvm.long(keys.0.len() as i64);
    dvm.long(addr_start as i64);
    dvm.long(index_addr.len() as i64);
    dvd.bytes(&index_addr);
}

/// `Lucene90DocValuesConsumer.addBinaryField`: the values back to back, the docs with one, and
/// (if their lengths differ) each value's `DirectMonotonic` start.
fn write_binary_entry(dvm: &mut Output, dvd: &mut Output, per_doc: &[Option<Vec<u8>>]) {
    let start = dvd.0.len();
    dvm.long(start as i64);
    let values: Vec<&[u8]> = per_doc.iter().flatten().map(Vec::as_slice).collect();
    values.iter().for_each(|v| dvd.bytes(v));
    dvm.long((dvd.0.len() - start) as i64);
    let docs: Vec<u32> = (0..per_doc.len() as u32)
        .filter(|d| per_doc[*d as usize].is_some())
        .collect();
    write_docs_with_field(dvm, dvd, &docs, per_doc.len());
    let lens = values.iter().map(|v| v.len() as i32);
    let (min, max) = (lens.clone().min(), lens.max().unwrap_or(0));
    dvm.bytes(&(docs.len() as i32).to_le_bytes());
    dvm.bytes(&min.unwrap_or(i32::MAX).to_le_bytes());
    dvm.bytes(&max.to_le_bytes());
    if min.is_some_and(|min| max > min) {
        let mut starts = vec![0i64];
        for v in &values {
            starts.push(starts[starts.len() - 1] + v.len() as i64);
        }
        dvm.long(dvd.0.len() as i64);
        dvm.vint(DIRECT_MONOTONIC_BLOCK_SHIFT);
        let packed = write_direct_monotonic(dvm, &starts, DIRECT_MONOTONIC_BLOCK_SHIFT);
        dvd.bytes(&packed);
        dvm.long(packed.len() as i64);
    }
}

/// Returns `(.dvm, .dvd)` for `numeric` fields (`(field number, values per doc)`; a doc has at
/// most one) and `keyword` fields as SORTED_SET (`(field number, terms per doc)`; a doc may have
/// none or several).
pub fn write_doc_values(
    numeric: &[(i32, Vec<Vec<i64>>)],
    keyword: &[(i32, Vec<Vec<Vec<u8>>>)],
    id: &[u8; codec_util::ID_LEN],
    suffix: &str,
) -> (Vec<u8>, Vec<u8>) {
    write_doc_values_with(numeric, &[], keyword, &[], id, suffix)
}

/// Like [`write_doc_values`], plus `sorted_numeric` fields (what ES uses for `long`, `double`,
/// `boolean`, ...): `(field number, values per doc)`, and `binary` fields (`(field number, value
/// per doc)`).
pub fn write_doc_values_with(
    numeric: &[(i32, Vec<Vec<i64>>)],
    sorted_numeric: &[(i32, Vec<Vec<i64>>)],
    keyword: &[(i32, Vec<Vec<Vec<u8>>>)],
    binary: &[(i32, Vec<Option<Vec<u8>>>)],
    id: &[u8; codec_util::ID_LEN],
    suffix: &str,
) -> (Vec<u8>, Vec<u8>) {
    let (mut dvm, mut dvd) = (Output::default(), Output::default());
    dvm.index_header("Lucene90DocValuesMetadata", VERSION_MAX, id, suffix);
    dvd.index_header("Lucene90DocValuesData", VERSION_MAX, id, suffix);
    for (number, values) in numeric {
        dvm.bytes(&number.to_le_bytes());
        dvm.byte(0);
        let (docs, flat) = flatten(values);
        write_numeric_entry(&mut dvm, &mut dvd, &flat, &docs, values.len(), false);
    }
    for (number, values) in sorted_numeric {
        dvm.bytes(&number.to_le_bytes());
        dvm.byte(4);
        write_sorted_numeric_entry(&mut dvm, &mut dvd, values, false);
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
            write_numeric_entry(&mut dvm, &mut dvd, &ords, &docs, values.len(), true);
        } else {
            dvm.byte(1);
            write_sorted_numeric_entry(&mut dvm, &mut dvd, &ords, true);
        }
        write_terms_dict(&mut dvm, &mut dvd, &terms);
    }
    for (number, values) in binary {
        dvm.bytes(&number.to_le_bytes());
        dvm.byte(1);
        write_binary_entry(&mut dvm, &mut dvd, values);
    }
    dvm.bytes(&(-1i32).to_le_bytes());
    (dvm.finish(), dvd.finish())
}
