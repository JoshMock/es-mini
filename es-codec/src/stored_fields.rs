//! Stored fields (`.fdt`, Lucene90StoredFieldsFormat BEST_SPEED: LZ4 with preset dictionary).
use crate::{
    Result, SegmentReader, codec_util,
    direct_monotonic::{read_direct_monotonic, write_direct_monotonic},
    err,
    input::{DataInput, zigzag},
    lz4,
    output::Output,
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

/// One doc's stored fields: `(field number, value)`.
pub type Doc = Vec<(i32, StoredValue)>;

/// `FieldsIndexReader`: the first doc and `.fdt` start of every chunk, plus one past the end.
pub(crate) struct FieldsIndex {
    chunk_size: usize,
    docs: Vec<i64>,
    pointers: Vec<i64>,
}

/// Reads the fields index from `.fdm` and `.fdx`.
pub(crate) fn read_fields_index(
    fdm: &[u8],
    fdx: &[u8],
    id: &[u8; codec_util::ID_LEN],
    max_doc: usize,
) -> Result<FieldsIndex> {
    codec_util::check_footer(fdm)?;
    let m = &mut DataInput::new(fdm);
    codec_util::check_index_header(m, "Lucene90FieldsIndexMeta", 0, 1, id, "")?;
    let chunk_size = m.read_len()?;
    if m.read_int()? as usize != max_doc {
        return err("stored fields index does not cover every doc");
    }
    let block_shift = m.read_int()?;
    let n =
        u64::try_from(m.read_int()?).map_err(|_| crate::Error("negative chunk count".into()))?;
    codec_util::check_footer(fdx)?;
    codec_util::check_index_header(
        &mut DataInput::new(fdx),
        "Lucene90FieldsIndexIdx",
        0,
        1,
        id,
        "",
    )?;
    let mut column = || -> Result<Vec<i64>> {
        let start = m.read_long()? as usize;
        let mono = read_direct_monotonic(m, n, block_shift)?;
        mono.values(fdx.get(start..).unwrap_or_default(), n)
    };
    let (docs, pointers) = (column()?, column()?);
    if docs.first() != Some(&0) || docs.last() != Some(&(max_doc as i64)) {
        return err("corrupt stored fields index");
    }
    Ok(FieldsIndex {
        chunk_size,
        docs,
        pointers,
    })
}

impl FieldsIndex {
    /// Chunks holding docs `keep` accepts, in order: `(first doc, .fdt offset, .fdt length)`.
    pub(crate) fn chunks(
        &self,
        keep: impl Fn(usize) -> bool,
    ) -> impl Iterator<Item = (usize, u64, u64)> {
        (0..self.docs.len() - 1)
            .filter(move |i| (self.docs[*i] as usize..self.docs[i + 1] as usize).any(&keep))
            .map(|i| {
                let (from, to) = (self.pointers[i], self.pointers[i + 1]);
                (
                    self.docs[i] as usize,
                    from as u64,
                    (to - from).max(0) as u64,
                )
            })
    }

    /// Decodes one chunk's bytes, which must start with `doc_base`.
    pub(crate) fn read_chunk(&self, chunk: &[u8], doc_base: usize) -> Result<Vec<Doc>> {
        let inp = &mut DataInput::new(chunk);
        let base = inp.read_len()?;
        let token = inp.read_len()?;
        let (n_docs, sliced) = (token >> 2, token & 1 != 0);
        if base != doc_base || n_docs == 0 {
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
                data.extend(decompress(inp, (total - data.len()).min(self.chunk_size))?);
            }
        } else {
            data = decompress(inp, total)?;
        }
        let mut at = 0;
        let mut docs = Vec::with_capacity(n_docs);
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
        Ok(docs)
    }
}

/// The stored fields of the docs `keep` accepts, with their doc ids, decoded a chunk at a time.
/// Chunks without such docs are not read.
pub(crate) fn read_documents<'a>(
    r: &'a SegmentReader,
    keep: impl Fn(usize) -> bool + 'a,
) -> Result<impl Iterator<Item = Result<(usize, Doc)>> + 'a> {
    let id = &r.seg.info.id;
    let index = read_fields_index(&r.cfs_file(".fdm")?, &r.cfs_file(".fdx")?, id, r.max_doc())?;
    let header = r.cfs_slice(".fdt", 0, index.pointers[0] as u64)?;
    codec_util::check_index_header(
        &mut DataInput::new(&header),
        "Lucene90StoredFieldsFastData",
        1,
        1,
        id,
        "",
    )?;
    let chunks: Vec<_> = index.chunks(&keep).collect();
    Ok(chunks.into_iter().flat_map(move |(base, offset, len)| {
        let docs = r
            .cfs_slice(".fdt", offset, len)
            .and_then(|c| index.read_chunk(&c, base));
        match docs {
            Ok(docs) => (base..)
                .zip(docs)
                .filter(|(d, _)| keep(*d))
                .map(Ok)
                .collect(),
            Err(e) => vec![Err(e)],
        }
    }))
}

const BLOCK_SHIFT: i32 = 10;
const CHUNK_SIZE: usize = 80 * 1024;
const MAX_DOCS_PER_CHUNK: usize = 1024;

fn write_field(out: &mut Output, number: i32, v: &StoredValue) {
    let tag = |ty: i64| ((number as i64) << TYPE_BITS) | ty;
    let zz = |v: i64| ((v << 1) ^ (v >> 63)) as u64;
    match v {
        StoredValue::String(s) => {
            out.vlong(tag(0));
            out.string(s);
        }
        StoredValue::Bytes(b) => {
            out.vlong(tag(1));
            out.vint(b.len() as i32);
            out.bytes(b);
        }
        StoredValue::Int(i) => {
            out.vlong(tag(2));
            out.vint(zz(*i as i64) as u32 as i32);
        }
        StoredValue::Float(f) => {
            out.vlong(tag(3));
            out.byte(0xff);
            out.bytes(&f.to_bits().to_le_bytes());
        }
        StoredValue::Long(l) => {
            // Millisecond unit, so no date-style scaling.
            out.vlong(tag(4));
            let z = zz(*l);
            let more = z >> 5 != 0;
            out.byte((z & 0x1f) as u8 | if more { 0x20 } else { 0 });
            if more {
                out.vlong((z >> 5) as i64);
            }
        }
        StoredValue::Double(d) => {
            out.vlong(tag(5));
            out.byte(0xff);
            out.long(d.to_bits() as i64);
        }
    }
}

/// `StoredFieldsInts.writeInts` for `count >= 2`.
fn write_ints(out: &mut Output, vals: &[usize]) {
    if vals.iter().all(|v| *v == vals[0]) {
        out.byte(0);
        out.vint(vals[0] as i32);
        return;
    }
    out.byte(32);
    let blocks = vals.len() / 128;
    for b in vals[..blocks * 128].chunks(128) {
        for k in 0..64 {
            out.long((b[k] as i64) << 32 | b[k + 64] as i64);
        }
    }
    for v in &vals[blocks * 128..] {
        out.bytes(&(*v as i32).to_le_bytes());
    }
}

/// One `LZ4WithPresetDictCompressor.compress` of `data` with an empty dictionary and a single
/// literal-only block.
fn compress(out: &mut Output, data: &[u8]) {
    let mut block = Output::default();
    lz4::literals(&mut block, data);
    out.vint(0);
    out.vint(data.len() as i32);
    out.vint(1);
    out.vint(block.0.len() as i32);
    out.byte(0); // empty dictionary
    out.bytes(&block.0);
}

/// Returns `(.fdm, .fdt, .fdx)`. Each doc is `(field number, value)`.
pub fn write_stored_fields(
    docs: &[Vec<(i32, StoredValue)>],
    id: &[u8; codec_util::ID_LEN],
) -> Result<(Vec<u8>, Vec<u8>, Vec<u8>)> {
    let mut fdt = Output::default();
    fdt.index_header("Lucene90StoredFieldsFastData", 1, id, "");
    let (mut base, mut chunk_docs, mut chunk_ptrs) = (0, vec![], vec![]);
    while base < docs.len() {
        chunk_docs.push(base as i64);
        chunk_ptrs.push(fdt.0.len() as i64);
        let (mut data, mut counts, mut lens) = (Output::default(), vec![], vec![]);
        while base + counts.len() < docs.len()
            && counts.len() < MAX_DOCS_PER_CHUNK
            && data.0.len() < CHUNK_SIZE
        {
            let doc = &docs[base + counts.len()];
            let before = data.0.len();
            doc.iter().for_each(|(n, v)| write_field(&mut data, *n, v));
            counts.push(doc.len());
            lens.push(data.0.len() - before);
        }
        if data.0.len() >= 2 * CHUNK_SIZE {
            return err("stored fields chunk too large to write");
        }
        fdt.vint(base as i32);
        fdt.vint((counts.len() << 2) as i32);
        if counts.len() == 1 {
            fdt.vint(counts[0] as i32);
            fdt.vint(lens[0] as i32);
        } else {
            write_ints(&mut fdt, &counts);
            write_ints(&mut fdt, &lens);
        }
        compress(&mut fdt, &data.0);
        base += counts.len();
    }
    // The doc-to-chunk and chunk-to-pointer indexes (DirectMonotonic) live in `.fdx`; `.fdm` holds
    // their metadata. Only the final chunk is dirty (force-flushed when the segment finishes).
    let max_pointer = fdt.0.len() as i64;
    chunk_docs.push(docs.len() as i64);
    chunk_ptrs.push(max_pointer);
    let mut fdx = Output::default();
    fdx.index_header("Lucene90FieldsIndexIdx", 0, id, "");
    let mut fdm = Output::default();
    fdm.index_header("Lucene90FieldsIndexMeta", 1, id, "");
    fdm.vint(CHUNK_SIZE as i32);
    let num_chunks = chunk_docs.len() as i32 - 1;
    for v in [docs.len() as i32, BLOCK_SHIFT, num_chunks + 1] {
        fdm.bytes(&v.to_le_bytes());
    }
    for values in [&chunk_docs, &chunk_ptrs] {
        fdm.long(fdx.0.len() as i64);
        let packed = write_direct_monotonic(&mut fdm, values, BLOCK_SHIFT);
        fdx.bytes(&packed);
    }
    fdm.long(fdx.0.len() as i64);
    fdm.long(max_pointer);
    fdm.vlong(num_chunks as i64);
    fdm.vlong((num_chunks > 0) as i64);
    fdm.vlong(
        chunk_docs
            .iter()
            .rev()
            .nth(1)
            .map_or(0, |last| docs.len() as i64 - last),
    );
    Ok((fdm.finish(), fdt.finish(), fdx.finish()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_then_read_round_trips() {
        let docs: Vec<Vec<(i32, StoredValue)>> = (0..3000)
            .map(|i| {
                let mut d = vec![(0, StoredValue::String(format!("doc-{i}")))];
                if i % 3 == 0 {
                    d.push((1, StoredValue::Long(-(i as i64) * 1_000_003)));
                    d.push((2, StoredValue::Double(i as f64 / 7.0)));
                    d.push((3, StoredValue::Float(-1.5)));
                    d.push((4, StoredValue::Int(-i)));
                    d.push((5, StoredValue::Bytes(vec![i as u8; 300])));
                }
                d
            })
            .collect();
        let id = [7u8; 16];
        let read = |n: usize, keep: &dyn Fn(usize) -> bool| {
            let (fdm, fdt, fdx) = write_stored_fields(&docs[..n], &id).unwrap();
            let index = read_fields_index(&fdm, &fdx, &id, n).unwrap();
            let chunks: Vec<_> = index.chunks(keep).collect();
            let docs: Vec<Doc> = chunks
                .iter()
                .flat_map(|&(base, at, len)| {
                    let chunk = &fdt[at as usize..(at + len) as usize];
                    index.read_chunk(chunk, base).unwrap()
                })
                .collect();
            (chunks.len(), docs)
        };
        let (chunks, all) = read(3000, &|_| true);
        assert_eq!(all, docs);
        assert!(chunks > 2, "{chunks} chunks");
        // Only the chunk holding the asked-for docs is read.
        let (n, part) = read(3000, &|d| (1500..1502).contains(&d));
        assert_eq!(n, 1);
        assert!(part.len() < 3000 && part.windows(2).any(|w| w == &docs[1500..1502]));
        assert_eq!(read(1, &|d| d == 0), (1, docs[..1].to_vec()));
        // Chunks between the asked-for docs are skipped.
        let (n, part) = read(3000, &|d| d == 0 || d == 2999);
        assert_eq!(n, 2);
        assert!(part.contains(&docs[0]) && part.contains(&docs[2999]));
    }
}
