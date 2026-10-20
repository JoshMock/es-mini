//! Stored fields (`.fdt`, Lucene90StoredFieldsFormat BEST_SPEED: LZ4 with preset dictionary).
use crate::{
    Result, SegmentReader, codec_util, err,
    input::{DataInput, zigzag},
    lz4,
};

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
    // The decompressed data starts with the dictionary bytes; each block decodes against the
    // dictionary only, not earlier blocks.
    let mut out = Vec::new();
    lz4::decompress(inp, dict_len, &mut out)?;
    while out.len() < len {
        let mut buf = out[..dict_len].to_vec();
        lz4::decompress(inp, block_len.min(len - out.len()), &mut buf)?;
        out.extend_from_slice(&buf[dict_len..]);
    }
    out.truncate(len);
    Ok(out)
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
            let mask = (1u64 << bpv) - 1;
            let blocks = count / 128;
            for _ in 0..blocks {
                let longs: Vec<u64> = (0..128 / per_long)
                    .map(|_| inp.read_long().map(|l| l as u64))
                    .collect::<Result<_>>()?;
                for part in 0..per_long {
                    let shift = 64 - bpv as usize * (part + 1);
                    out.extend(longs.iter().map(|l| (l >> shift) & mask));
                }
            }
            for _ in blocks * 128..count {
                out.push(match bpv {
                    8 => inp.read_byte()? as u64,
                    16 => inp.read_short()? as u16 as u64,
                    _ => inp.read_int()? as u32 as u64,
                });
            }
            Ok(out)
        }
        bpv => err(format!("unsupported stored fields bits per value {bpv}")),
    }
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
                    let mid = doc.read_short()? as u16 as u32;
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
            let unit = match header >> 6 {
                0 => 1,
                1 => 1000,
                2 => 3_600_000,
                _ => 86_400_000,
            };
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
                    let mid = doc.read_short()? as u16 as u64;
                    let lo = doc.read_byte()? as u64;
                    f64::from_bits((b as u64) << 56 | hi << 24 | mid << 8 | lo)
                }
            })
        }
        t => return err(format!("unknown stored field type {t}")),
    })
}

pub(crate) fn read_stored_fields(r: &SegmentReader) -> Result<Vec<Vec<(i32, StoredValue)>>> {
    let id = &r.seg.info.id;
    let meta = r.cfs_file(".fdm")?;
    codec_util::check_footer(meta)?;
    let m = &mut DataInput::new(meta);
    codec_util::check_index_header(m, "Lucene90FieldsIndexMeta", 0, 1, id, "")?;
    let chunk_size = m.read_len()?;

    let fdt = r.cfs_file(".fdt")?;
    codec_util::check_footer(fdt)?;
    let inp = &mut DataInput::new(&fdt[..fdt.len() - codec_util::FOOTER_LEN]);
    codec_util::check_index_header(inp, "Lucene90StoredFieldsFastData", 1, 1, id, "")?;
    let mut docs = Vec::with_capacity(r.max_doc());
    while inp.remaining() > 0 {
        let doc_base = inp.read_len()?;
        let token = inp.read_len()?;
        let (n_docs, sliced) = (token >> 2, token & 1 != 0);
        if doc_base != docs.len() || docs.len() + n_docs > r.max_doc() {
            return err(format!("corrupt stored fields chunk at doc {doc_base}"));
        }
        let (counts, lens) = if n_docs == 1 {
            (vec![inp.read_vint()? as u64], vec![inp.read_vint()? as u64])
        } else {
            (read_ints(inp, n_docs)?, read_ints(inp, n_docs)?)
        };
        let total = lens
            .iter()
            .try_fold(0usize, |a, l| a.checked_add(usize::try_from(*l).ok()?))
            .ok_or_else(|| crate::Error("corrupt stored fields doc lengths".into()))?;
        let mut data = Vec::new();
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
    if docs.len() != r.max_doc() {
        return err("stored fields do not cover every doc");
    }
    Ok(docs)
}
