//! Stored fields (`.fdt`, Lucene90StoredFieldsFormat BEST_SPEED: LZ4 with preset dictionary).
use crate::{CompoundFile, Result, SegmentCommitInfo, codec_util, err, input::DataInput};

#[derive(Debug, Clone, PartialEq)]
pub enum StoredValue {
    String(String),
    Bytes(Vec<u8>),
    Int(i32),
    Float(f32),
    Long(i64),
    Double(f64),
}

const TYPE_BITS: u32 = 3;
const NUMERIC_TIME_UNITS: [(u8, i64); 4] = [
    (0x00, 1),
    (0x40, 1000),
    (0x80, 3_600_000),
    (0xC0, 86_400_000),
];

/// Lucene `LZ4.decompress`: reads until `out` has grown by `len` bytes. Matches may reference
/// earlier bytes of `out` (the preset dictionary).
pub(crate) fn lz4(inp: &mut DataInput, len: usize, out: &mut Vec<u8>) -> Result<()> {
    let end = out.len() + len;
    let extra = |inp: &mut DataInput, mut n: usize| -> Result<usize> {
        loop {
            let b = inp.read_byte()?;
            n += b as usize;
            if b != 0xff {
                return Ok(n);
            }
        }
    };
    loop {
        let token = inp.read_byte()? as usize;
        let mut lit = token >> 4;
        if lit == 0xf {
            lit = extra(inp, lit)?;
        }
        out.extend_from_slice(inp.read_bytes(lit)?);
        if out.len() >= end {
            break;
        }
        let dec = inp.read_byte()? as usize | (inp.read_byte()? as usize) << 8;
        let mut m = token & 0xf;
        if m == 0xf {
            m = extra(inp, m)?;
        }
        if dec == 0 || dec > out.len() {
            return err("corrupt LZ4 match offset");
        }
        for _ in 0..m + 4 {
            out.push(out[out.len() - dec]);
        }
        if out.len() >= end {
            break;
        }
    }
    if out.len() != end {
        return err("corrupt LZ4 block length");
    }
    Ok(())
}

/// One `LZ4WithPresetDictDecompressor.decompress` over a whole `len`-byte piece.
///
/// Chunks of only empty docs are compressed too (to an empty dictionary), so this reads a
/// header even when `len` is 0.
fn decompress(inp: &mut DataInput, len: usize) -> Result<Vec<u8>> {
    let dict_len = inp.read_len()?;
    let block_len = inp.read_len()?;
    inp.read_vint()?; // compressed length of the dictionary
    if block_len == 0 && len > dict_len {
        return err("zero LZ4 block length");
    }
    let mut total = dict_len;
    while total < len {
        inp.read_vint()?; // compressed block lengths; decoding the blocks in order makes them moot
        total += block_len;
    }
    let mut out = Vec::with_capacity(len.max(dict_len));
    lz4(inp, dict_len, &mut out)?;
    // Blocks decode against the dictionary only, not earlier blocks.
    let dict = out.clone();
    out.clear();
    let mut done = dict_len;
    while done < len {
        let n = block_len.min(len - done);
        let mut buf = dict.clone();
        lz4(inp, n, &mut buf)?;
        out.extend_from_slice(&buf[dict_len..]);
        done += block_len;
    }
    // The decompressed data starts with the dictionary bytes.
    let mut all = dict;
    all.extend(out);
    all.truncate(len);
    Ok(all)
}

/// `StoredFieldsInts.readInts`.
fn read_ints(inp: &mut DataInput, count: usize) -> Result<Vec<u64>> {
    match inp.read_byte()? {
        0 => Ok(vec![inp.read_vint()? as u64; count]),
        // A 128-value block is n longs: value k is the (k / n)-th bpv-bit slot, from the top, of
        // long k % n. The values after the last full block are plain bytes, shorts, or ints.
        bpv @ (8 | 16 | 32) => {
            let per_long = 64 / bpv as usize;
            let mut out = Vec::with_capacity(count);
            let blocks = count / 128;
            for _ in 0..blocks {
                let longs: Vec<u64> = (0..128 / per_long)
                    .map(|_| inp.read_long().map(|l| l as u64))
                    .collect::<Result<_>>()?;
                let mask = (1u64 << bpv) - 1;
                for part in 0..per_long {
                    let shift = 64 - bpv as usize * (part + 1);
                    out.extend(longs.iter().map(|l| (l >> shift) & mask));
                }
            }
            for _ in blocks * 128..count {
                out.push(match bpv {
                    8 => inp.read_byte()? as u64,
                    16 => u16::from_le_bytes(inp.read_bytes(2)?.try_into().unwrap()) as u64,
                    _ => inp.read_int()? as u32 as u64,
                });
            }
            Ok(out)
        }
        bpv => err(format!("unsupported stored fields bits per value {bpv}")),
    }
}

pub(crate) fn zigzag(v: u64) -> i64 {
    (v >> 1) as i64 ^ -((v & 1) as i64)
}

fn read_field(doc: &mut DataInput, ty: u64) -> Result<StoredValue> {
    Ok(match ty {
        0 => StoredValue::String(doc.read_string()?),
        1 => {
            let n = doc.read_len()?;
            StoredValue::Bytes(doc.read_bytes(n)?.to_vec())
        }
        2 => StoredValue::Int(zigzag(doc.read_vint()? as u32 as u64) as i32),
        3 => {
            let b = doc.read_byte()?;
            StoredValue::Float(match b {
                0xff => f32::from_bits(doc.read_int()? as u32),
                b if b & 0x80 != 0 => ((b & 0x7f) as i32 - 1) as f32,
                b => {
                    let mid = u16::from_le_bytes(doc.read_bytes(2)?.try_into().unwrap()) as u32;
                    let lo = doc.read_byte()? as u32;
                    f32::from_bits((b as u32) << 24 | mid << 8 | lo)
                }
            })
        }
        4 => {
            let header = doc.read_byte()?;
            let mut bits = (header & 0x1f) as u64;
            if header & 0x20 != 0 {
                bits |= (doc.read_vlong()? as u64) << 5;
            }
            let unit = NUMERIC_TIME_UNITS
                .iter()
                .find(|(h, _)| *h == header & 0xc0)
                .unwrap()
                .1;
            StoredValue::Long(zigzag(bits) * unit)
        }
        5 => {
            let b = doc.read_byte()?;
            StoredValue::Double(match b {
                0xff => f64::from_bits(doc.read_long()? as u64),
                0xfe => f32::from_bits(doc.read_int()? as u32) as f64,
                b if b & 0x80 != 0 => ((b & 0x7f) as i32 - 1) as f64,
                b => {
                    let hi = doc.read_int()? as u32 as u64;
                    let mid = u16::from_le_bytes(doc.read_bytes(2)?.try_into().unwrap()) as u64;
                    let lo = doc.read_byte()? as u64;
                    f64::from_bits((b as u64) << 56 | hi << 24 | mid << 8 | lo)
                }
            })
        }
        t => return err(format!("unknown stored field type {t}")),
    })
}

/// All stored fields of every doc, indexed by doc id: `(field number, value)`.
pub fn read_stored_fields(
    cfs: &CompoundFile,
    seg: &SegmentCommitInfo,
) -> Result<Vec<Vec<(i32, StoredValue)>>> {
    let missing = |ext: &str| crate::Error(format!("{} has no {ext}", seg.name));
    let id = &seg.info.id;
    let meta = cfs.file(".fdm").ok_or_else(|| missing(".fdm"))?;
    codec_util::check_footer(meta)?;
    let m = &mut DataInput::new(meta);
    codec_util::check_index_header(m, "Lucene90FieldsIndexMeta", 0, 1, id, "")?;
    let chunk_size = m.read_len()?;

    let fdt = cfs.file(".fdt").ok_or_else(|| missing(".fdt"))?;
    codec_util::check_footer(fdt)?;
    let inp = &mut DataInput::new(&fdt[..fdt.len() - codec_util::FOOTER_LEN]);
    codec_util::check_index_header(inp, "Lucene90StoredFieldsFastData", 1, 1, id, "")?;
    let mut docs = Vec::with_capacity(seg.info.max_doc as usize);
    while inp.remaining() > 0 {
        let doc_base = inp.read_len()?;
        let token = inp.read_len()?;
        let (n_docs, sliced) = (token >> 2, token & 1 != 0);
        if doc_base != docs.len() || docs.len() + n_docs > seg.info.max_doc as usize {
            return err(format!("corrupt stored fields chunk at doc {doc_base}"));
        }
        let (counts, lens) = if n_docs == 1 {
            (vec![inp.read_vint()? as u64], vec![inp.read_vint()? as u64])
        } else {
            (read_ints(inp, n_docs)?, read_ints(inp, n_docs)?)
        };
        let total = lens.iter().sum::<u64>() as usize;
        let mut data = Vec::with_capacity(total);
        if sliced {
            while data.len() < total {
                data.extend(decompress(inp, (total - data.len()).min(chunk_size))?);
            }
        } else {
            data = decompress(inp, total)?;
        }
        let mut at = 0;
        for (count, len) in counts.into_iter().zip(lens) {
            let doc = &mut DataInput::new(&data[at..at + len as usize]);
            at += len as usize;
            let mut fields = Vec::new();
            for _ in 0..count {
                let info = doc.read_vlong()? as u64;
                fields.push(((info >> TYPE_BITS) as i32, read_field(doc, info & 7)?));
            }
            docs.push(fields);
        }
    }
    if docs.len() != seg.info.max_doc as usize {
        return err("stored fields do not cover every doc");
    }
    Ok(docs)
}
