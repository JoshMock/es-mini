use super::*;
use crate::{
    codec_util,
    direct_monotonic::{direct_bits, pack, write_direct_monotonic},
    lz4,
    output::Output,
};

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
    let (docs, values) = flatten(per_doc);
    write_numeric_entry(dvm, dvd, &values, &docs, per_doc.len());
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
