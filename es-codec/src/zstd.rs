//! Zstandard (RFC 8878) decoder, for Elasticsearch's Zstd stored fields (index.codec:
//! best_compression). Frames without dictionaries only; content checksums are skipped, not
//! verified.

use crate::{Result, err};

/// Decodes every frame of `src`.
pub fn decompress(mut src: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    while !src.is_empty() {
        let magic = le(take(&mut src, 4)?) as u32;
        if magic & 0xFFFF_FFF0 == 0x184D_2A50 {
            let len = le(take(&mut src, 4)?) as usize;
            take(&mut src, len)?;
        } else if magic == 0xFD2F_B528 {
            frame(&mut src, &mut out)?;
        } else {
            return err(format!("bad zstd magic {magic:#x}"));
        }
    }
    Ok(out)
}

/// The next `n` bytes of `src`, which advances past them.
fn take<'a>(src: &mut &'a [u8], n: usize) -> Result<&'a [u8]> {
    if n > src.len() {
        return err("truncated zstd frame");
    }
    let (head, rest) = src.split_at(n);
    *src = rest;
    Ok(head)
}

fn le(bytes: &[u8]) -> u64 {
    bytes.iter().rev().fold(0, |v, &b| v << 8 | u64::from(b))
}

fn frame(src: &mut &[u8], out: &mut Vec<u8>) -> Result<()> {
    let desc = take(src, 1)?[0];
    let single_segment = desc & 0x20 != 0;
    if desc & 0x08 != 0 {
        return err("reserved zstd frame header bit set");
    }
    if !single_segment {
        take(src, 1)?; // window descriptor: the whole output stays in memory anyway
    }
    if le(take(src, [0, 1, 2, 4][usize::from(desc & 3)])?) != 0 {
        return err("zstd dictionaries are unsupported");
    }
    let fcs_len = match desc >> 6 {
        0 => usize::from(single_segment),
        f => 1 << f,
    };
    let fcs = take(src, fcs_len)?;
    let size = le(fcs) + if fcs_len == 2 { 256 } else { 0 };
    let start = out.len();
    if fcs_len > 0 {
        out.reserve(usize::try_from(size).unwrap_or(0).min(1 << 26));
    }
    let mut state = State {
        rep: [1, 4, 8],
        huffman: None,
        tables: [None, None, None],
    };
    loop {
        let header = le(take(src, 3)?) as usize;
        let size = header >> 3;
        match (header >> 1) & 3 {
            0 => out.extend_from_slice(take(src, size)?),
            1 => {
                let b = take(src, 1)?[0];
                out.resize(out.len() + size, b);
            }
            2 => state.block(take(src, size)?, out)?,
            _ => return err("reserved zstd block type"),
        }
        if header & 1 != 0 {
            break;
        }
    }
    if fcs_len > 0 && (out.len() - start) as u64 != size {
        return err("zstd frame content size mismatch");
    }
    if desc & 0x04 != 0 {
        take(src, 4)?; // ponytail: XXH64 content checksum skipped; Lucene's footer CRC covers the file
    }
    Ok(())
}

/// Reads a forward, little-endian bitstream (FSE table descriptions).
struct Forward<'a> {
    data: &'a [u8],
    bit: usize,
}

impl Forward<'_> {
    fn peek(&self, n: u32) -> u32 {
        let byte = self.bit / 8;
        let word = le(&self.data[byte.min(self.data.len())..(byte + 8).min(self.data.len())]);
        ((word >> (self.bit % 8)) & ((1 << n) - 1)) as u32
    }
}

/// Reads a backward bitstream: from the end of the data, past its final-bit flag, to the start.
/// Reading past the start yields zero bits.
struct Backward<'a> {
    data: &'a [u8],
    /// Bits left; negative once reads went past the start.
    left: i64,
}

impl<'a> Backward<'a> {
    fn new(data: &'a [u8]) -> Result<Self> {
        let Some(&last) = data.last().filter(|b| **b != 0) else {
            return err("zstd bitstream without a final bit");
        };
        let left = data.len() as i64 * 8 - i64::from(last.leading_zeros()) - 1;
        Ok(Self { data, left })
    }

    /// The next `n` (at most 56) bits.
    fn peek(&self, n: u32) -> u64 {
        let lo = (self.left - i64::from(n)).max(0) as usize;
        let have = self.left - lo as i64;
        if n == 0 || have <= 0 {
            return 0;
        }
        let bytes = &self.data[lo / 8..(lo / 8 + 8).min(self.data.len())];
        ((le(bytes) >> (lo % 8)) & ((1 << have) - 1)) << (i64::from(n) - have)
    }

    fn read(&mut self, n: u32) -> u64 {
        let v = self.peek(n);
        self.left -= i64::from(n);
        v
    }

    fn overflowed(&self) -> bool {
        self.left < 0
    }
}

/// An FSE decoding table: per state, `(symbol, bits to read, baseline of the next state)`.
#[derive(Clone)]
struct Fse {
    log: u32,
    cells: Vec<(u8, u32, u32)>,
}

impl Fse {
    /// `FSE_buildDTable` over normalized counts (`-1` for "less than 1").
    fn build(norm: &[i32], log: u32) -> Result<Self> {
        let size = 1usize << log;
        let mut cells = vec![(0u8, 0, 0); size];
        let mut next = vec![0u32; norm.len()];
        let mut high = size;
        for (s, &n) in norm.iter().enumerate() {
            if n == -1 {
                high -= 1;
                cells[high].0 = s as u8;
                next[s] = 1;
            } else {
                next[s] = n.max(0) as u32;
            }
        }
        let step = (size >> 1) + (size >> 3) + 3;
        let mut pos = 0;
        for (s, &n) in norm.iter().enumerate() {
            for _ in 0..n.max(0) {
                cells[pos].0 = s as u8;
                pos = (pos + step) & (size - 1);
                while pos >= high {
                    pos = (pos + step) & (size - 1);
                }
            }
        }
        if pos != 0 {
            return err("corrupt zstd FSE distribution");
        }
        for cell in &mut cells {
            let n = &mut next[usize::from(cell.0)];
            let bits = log - (31 - n.leading_zeros());
            cell.1 = bits;
            cell.2 = (*n << bits) - size as u32;
            *n += 1;
        }
        Ok(Self { log, cells })
    }

    /// Reads a table description (`FSE_readNCount`) from the start of `src`: the table and the
    /// bytes it took.
    fn read(src: &[u8], max_symbol: usize, max_log: u32) -> Result<(Self, usize)> {
        let mut r = Forward { data: src, bit: 4 };
        let log = u32::from(*src.first().ok_or_else(truncated)? & 0xF) + 5;
        if log > max_log {
            return err("zstd FSE accuracy log too large");
        }
        let mut remaining = (1i32 << log) + 1;
        let mut threshold = 1i32 << log;
        let mut bits = log + 1;
        let mut norm = Vec::new();
        let mut previous0 = false;
        while remaining > 1 && norm.len() <= max_symbol {
            if previous0 {
                loop {
                    let repeat = r.peek(2);
                    r.bit += 2;
                    norm.extend(std::iter::repeat_n(0, repeat as usize));
                    if repeat != 3 {
                        break;
                    }
                }
                if norm.len() > max_symbol {
                    break;
                }
            }
            let max = (2 * threshold - 1) - remaining;
            let v = r.peek(bits) as i32;
            let mut count;
            if v & (threshold - 1) < max {
                count = v & (threshold - 1);
                r.bit += bits as usize - 1;
            } else {
                count = v & (2 * threshold - 1);
                if count >= threshold {
                    count -= max;
                }
                r.bit += bits as usize;
            }
            count -= 1;
            remaining -= count.abs();
            norm.push(count);
            previous0 = count == 0;
            while remaining < threshold {
                bits -= 1;
                threshold >>= 1;
            }
        }
        let used = r.bit.div_ceil(8);
        if remaining != 1 || norm.len() > max_symbol + 1 || used > src.len() {
            return err("corrupt zstd FSE table description");
        }
        Ok((Self::build(&norm, log)?, used))
    }

    /// A table that always decodes `symbol` (RLE mode).
    fn rle(symbol: u8) -> Self {
        Self {
            log: 0,
            cells: vec![(symbol, 0, 0)],
        }
    }

    fn symbol(&self, state: usize) -> u8 {
        self.cells[state].0
    }

    fn update(&self, state: &mut usize, bits: &mut Backward) {
        let (_, n, base) = self.cells[*state];
        *state = (base as u64 + bits.read(n)) as usize;
    }
}

/// A Huffman decoding table: indexed by the next `max_bits` bits, `(symbol, code length)`.
struct Huffman {
    max_bits: u32,
    cells: Vec<(u8, u32)>,
}

impl Huffman {
    /// Reads a Huffman tree description from the start of `src`: the table and the bytes it took.
    fn read(src: &[u8]) -> Result<(Self, usize)> {
        let header = usize::from(*src.first().ok_or_else(truncated)?);
        let (mut weights, used) = if header < 128 {
            let data = src.get(1..1 + header).ok_or_else(truncated)?;
            let (fse, n) = Fse::read(data, 255, 6)?;
            let mut bits = Backward::new(&data[n..])?;
            let mut states = [bits.read(fse.log) as usize, bits.read(fse.log) as usize];
            let mut weights = Vec::new();
            // The two states take turns until the stream runs out; then the other one's symbol
            // ends the series.
            for i in 0.. {
                if weights.len() > 255 {
                    return err("too many zstd Huffman weights");
                }
                let s = &mut states[i % 2];
                weights.push(fse.symbol(*s));
                fse.update(s, &mut bits);
                if bits.overflowed() {
                    weights.push(fse.symbol(states[(i + 1) % 2]));
                    break;
                }
            }
            (weights, 1 + header)
        } else {
            let n = header - 127;
            let data = src.get(1..1 + n.div_ceil(2)).ok_or_else(truncated)?;
            let weights = (0..n).map(|i| (data[i / 2] >> (4 * (1 - i % 2))) & 0xF);
            (weights.collect(), 1 + n.div_ceil(2))
        };
        let total: u32 = weights
            .iter()
            .filter(|w| **w > 0)
            .map(|w| 1u32.checked_shl(u32::from(*w) - 1).unwrap_or(0))
            .sum();
        if total == 0 || weights.iter().any(|w| *w > 11) {
            return err("corrupt zstd Huffman weights");
        }
        let max_bits = 32 - total.leading_zeros();
        let rest = (1 << max_bits) - total;
        if !rest.is_power_of_two() || max_bits > 11 {
            return err("corrupt zstd Huffman weights");
        }
        weights.push(rest.trailing_zeros() as u8 + 1);
        if weights.len() > 256 {
            return err("too many zstd Huffman weights");
        }
        let mut cells = Vec::with_capacity(1 << max_bits);
        for w in 1..=max_bits as u8 {
            for (s, _) in weights.iter().enumerate().filter(|(_, x)| **x == w) {
                let len = max_bits + 1 - u32::from(w);
                cells.extend(std::iter::repeat_n((s as u8, len), 1 << (w - 1)));
            }
        }
        Ok((Self { max_bits, cells }, used))
    }

    /// Decodes `n` symbols from one stream.
    fn decode(&self, stream: &[u8], n: usize, out: &mut Vec<u8>) -> Result<()> {
        let mut bits = Backward::new(stream)?;
        for _ in 0..n {
            let (s, len) = self.cells[bits.peek(self.max_bits) as usize];
            bits.read(len);
            out.push(s);
        }
        if bits.left != 0 {
            return err("corrupt zstd Huffman stream");
        }
        Ok(())
    }
}

fn truncated() -> crate::Error {
    crate::Error("truncated zstd block".into())
}

/// What a frame's compressed blocks carry over to the next: repeat offsets and entropy tables.
struct State {
    rep: [usize; 3],
    huffman: Option<Huffman>,
    /// Literal length, offset and match length tables.
    tables: [Option<Fse>; 3],
}

/// Literal length codes 16..=35: (baseline, extra bits). Codes below 16 are their own length.
const LL: [(u32, u32); 20] = [
    (16, 1),
    (18, 1),
    (20, 1),
    (22, 1),
    (24, 2),
    (28, 2),
    (32, 3),
    (40, 3),
    (48, 4),
    (64, 6),
    (128, 7),
    (256, 8),
    (512, 9),
    (1024, 10),
    (2048, 11),
    (4096, 12),
    (8192, 13),
    (16384, 14),
    (32768, 15),
    (65536, 16),
];

/// Match length codes 32..=52: (baseline, extra bits). Codes below 32 are length code + 3.
const ML: [(u32, u32); 21] = [
    (35, 1),
    (37, 1),
    (39, 1),
    (41, 1),
    (43, 2),
    (47, 2),
    (51, 3),
    (59, 3),
    (67, 4),
    (83, 4),
    (99, 5),
    (131, 7),
    (259, 8),
    (515, 9),
    (1027, 10),
    (2051, 11),
    (4099, 12),
    (8195, 13),
    (16387, 14),
    (32771, 15),
    (65539, 16),
];

/// Predefined distributions (RFC 8878 3.1.1.3.2.2): literal lengths, offsets, match lengths.
const LL_DEFAULT: [i32; 36] = [
    4, 3, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 3, 2, 1, 1, 1, 1, 1,
    -1, -1, -1, -1,
];
const OF_DEFAULT: [i32; 29] = [
    1, 1, 1, 1, 1, 1, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1,
];
const ML_DEFAULT: [i32; 53] = [
    1, 4, 3, 2, 2, 2, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1, -1, -1,
];

impl State {
    fn block(&mut self, src: &[u8], out: &mut Vec<u8>) -> Result<()> {
        let (literals, used) = self.literals(src)?;
        let src = &src[used..];
        let (&b0, _) = src.split_first().ok_or_else(truncated)?;
        let (count, mut at) = match b0 {
            0 => (0, 1),
            1..128 => (usize::from(b0), 1),
            128..255 => {
                let b1 = *src.get(1).ok_or_else(truncated)?;
                ((usize::from(b0) - 128) << 8 | usize::from(b1), 2)
            }
            255 => (
                le(src.get(1..3).ok_or_else(truncated)?) as usize + 0x7F00,
                3,
            ),
        };
        if count == 0 {
            out.extend_from_slice(&literals);
            return Ok(());
        }
        let modes = *src.get(at).ok_or_else(truncated)?;
        at += 1;
        // (shift of its mode, default distribution and log, max symbol, max log)
        let kinds: [(u8, &[i32], u32, usize, u32); 3] = [
            (6, &LL_DEFAULT, 6, 35, 9),
            (4, &OF_DEFAULT, 5, 31, 8),
            (2, &ML_DEFAULT, 6, 52, 9),
        ];
        for (i, (shift, default, log, max_symbol, max_log)) in kinds.into_iter().enumerate() {
            let rest = src.get(at..).ok_or_else(truncated)?;
            self.tables[i] = Some(match (modes >> shift) & 3 {
                0 => Fse::build(default, log)?,
                1 => {
                    at += 1;
                    Fse::rle(*rest.first().ok_or_else(truncated)?)
                }
                2 => {
                    let (t, n) = Fse::read(rest, max_symbol, max_log)?;
                    at += n;
                    t
                }
                _ => self.tables[i]
                    .clone()
                    .ok_or_else(|| crate::Error("zstd repeat table without a table".into()))?,
            });
        }
        let [Some(ll), Some(of), Some(ml)] = &self.tables else {
            unreachable!("every table was just set")
        };
        let mut bits = Backward::new(src.get(at..).ok_or_else(truncated)?)?;
        let mut ll_state = bits.read(ll.log) as usize;
        let mut of_state = bits.read(of.log) as usize;
        let mut ml_state = bits.read(ml.log) as usize;
        let mut lit = 0;
        for i in 0..count {
            let of_code = u32::from(of.symbol(of_state));
            let ml_code = usize::from(ml.symbol(ml_state));
            let ll_code = usize::from(ll.symbol(ll_state));
            if of_code > 31 || ml_code > 52 || ll_code > 35 {
                return err("corrupt zstd sequence code");
            }
            let offset_value = (1usize << of_code) + bits.read(of_code) as usize;
            let match_len = match ml_code {
                0..32 => ml_code + 3,
                c => ML[c - 32].0 as usize + bits.read(ML[c - 32].1) as usize,
            };
            let lit_len = match ll_code {
                0..16 => ll_code,
                c => LL[c - 16].0 as usize + bits.read(LL[c - 16].1) as usize,
            };
            let offset = repeat_offset(&mut self.rep, offset_value, lit_len);
            if i + 1 < count {
                ll.update(&mut ll_state, &mut bits);
                ml.update(&mut ml_state, &mut bits);
                of.update(&mut of_state, &mut bits);
            }
            out.extend_from_slice(literals.get(lit..lit + lit_len).ok_or_else(truncated)?);
            lit += lit_len;
            let from = out
                .len()
                .checked_sub(offset)
                .filter(|_| offset > 0)
                .ok_or_else(|| crate::Error("zstd match before the start".into()))?;
            for j in from..from + match_len {
                out.push(out[j]);
            }
        }
        if bits.left != 0 {
            return err("corrupt zstd sequences bitstream");
        }
        out.extend_from_slice(&literals[lit..]);
        Ok(())
    }

    /// Decodes a block's literals section: the literals and the bytes it took.
    fn literals(&mut self, src: &[u8]) -> Result<(Vec<u8>, usize)> {
        let b0 = *src.first().ok_or_else(truncated)?;
        let format = (b0 >> 2) & 3;
        let header = |n: usize| src.get(..n).map(le).ok_or_else(truncated);
        match b0 & 3 {
            ty @ (0 | 1) => {
                let (size, at) = match format {
                    0 | 2 => (usize::from(b0 >> 3), 1),
                    1 => ((header(2)? >> 4) as usize, 2),
                    _ => ((header(3)? >> 4) as usize, 3),
                };
                if ty == 0 {
                    let lits = src.get(at..at + size).ok_or_else(truncated)?;
                    Ok((lits.to_vec(), at + size))
                } else {
                    Ok((vec![*src.get(at).ok_or_else(truncated)?; size], at + 1))
                }
            }
            ty => {
                let (at, bits) = match format {
                    0 | 1 => (3, 10),
                    2 => (4, 14),
                    _ => (5, 18),
                };
                let h = header(at)?;
                let mask = (1 << bits) - 1;
                let size = ((h >> 4) & mask) as usize;
                let compressed = ((h >> (4 + bits)) & mask) as usize;
                let mut data = src.get(at..at + compressed).ok_or_else(truncated)?;
                if ty == 2 {
                    let (table, n) = Huffman::read(data)?;
                    self.huffman = Some(table);
                    data = &data[n..];
                }
                let huffman = (self.huffman.as_ref())
                    .ok_or_else(|| crate::Error("zstd treeless literals without a table".into()))?;
                let mut lits = Vec::with_capacity(size);
                if format == 0 {
                    huffman.decode(data, size, &mut lits)?;
                } else {
                    let jump = data.get(..6).ok_or_else(truncated)?;
                    let lens =
                        [le(&jump[..2]), le(&jump[2..4]), le(&jump[4..])].map(|l| l as usize);
                    let mut streams = &data[6..];
                    let each = size.div_ceil(4);
                    if 3 * each > size {
                        return err("too few zstd literals for four streams");
                    }
                    for (i, n) in [each, each, each, size - 3 * each].into_iter().enumerate() {
                        let len = if i < 3 { lens[i] } else { streams.len() };
                        let stream = take(&mut streams, len)?;
                        huffman.decode(stream, n, &mut lits)?;
                    }
                }
                Ok((lits, at + compressed))
            }
        }
    }
}

/// Resolves an offset value against the repeat offsets and updates them.
fn repeat_offset(reps: &mut [usize; 3], value: usize, lit_len: usize) -> usize {
    let rep = *reps;
    if value > 3 {
        *reps = [value - 3, rep[0], rep[1]];
        return value - 3;
    }
    match value - 1 + usize::from(lit_len == 0) {
        0 => rep[0],
        1 => {
            *reps = [rep[1], rep[0], rep[2]];
            rep[1]
        }
        idx => {
            let offset = if idx == 2 {
                rep[2]
            } else {
                rep[0].saturating_sub(1)
            };
            *reps = [offset, rep[0], rep[1]];
            offset
        }
    }
}
