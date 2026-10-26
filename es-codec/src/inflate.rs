//! Raw DEFLATE (RFC 1951) decoder, for Elasticsearch's `DFL\0`-framed cluster state blobs.

use crate::{Result, err};

/// LSB-first bit reader over a 64-bit buffer.
struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
    buf: u64,
    cnt: u32,
}

impl Bits<'_> {
    /// The next `n` (at most 32) bits, zero-padded past the end of the input.
    fn peek(&mut self, n: u32) -> u32 {
        while self.cnt <= 56 {
            let Some(&b) = self.data.get(self.pos) else {
                break;
            };
            self.buf |= u64::from(b) << self.cnt;
            self.cnt += 8;
            self.pos += 1;
        }
        (self.buf & ((1u64 << n) - 1)) as u32
    }

    fn consume(&mut self, n: u32) -> Result<()> {
        if n > self.cnt {
            return err("truncated deflate stream");
        }
        self.buf >>= n;
        self.cnt -= n;
        Ok(())
    }

    fn read(&mut self, n: u32) -> Result<u32> {
        let v = self.peek(n);
        self.consume(n)?;
        Ok(v)
    }

    /// Drops bits up to the next byte boundary and returns its offset in `data`.
    fn align(&mut self) -> usize {
        let pos = self.pos - (self.cnt / 8) as usize;
        (self.pos, self.buf, self.cnt) = (pos, 0, 0);
        pos
    }
}

/// Canonical Huffman code as a single-level lookup table indexed by the next `bits` input bits;
/// each entry is `symbol << 4 | code length`, and 0 marks an unused code.
// ponytail: one table of 2^maxlen entries (up to 32K); use two-level tables if building them shows up in profiles.
struct Huffman {
    table: Vec<u32>,
    bits: u32,
}

impl Huffman {
    fn new(lengths: &[u8]) -> Result<Self> {
        let mut counts = [0u16; 16];
        for &l in lengths {
            counts[usize::from(l)] += 1;
        }
        counts[0] = 0;
        // Kraft check: reject over-subscribed sets, and incomplete ones except the lone-code case.
        let mut left = 1i32;
        for &c in &counts[1..] {
            left = (left << 1) - i32::from(c);
            if left < 0 {
                return err("over-subscribed huffman code");
            }
        }
        if left > 0 && counts[1..].iter().sum::<u16>() > 1 {
            return err("incomplete huffman code");
        }
        let bits = counts.iter().rposition(|&c| c > 0).unwrap_or(0) as u32;
        let mut next = [0u32; 16];
        for len in 1..16 {
            next[len] = (next[len - 1] + u32::from(counts[len - 1])) << 1;
        }
        let mut table = vec![0u32; 1 << bits];
        for (sym, &l) in lengths.iter().enumerate().filter(|(_, l)| **l > 0) {
            let code = next[usize::from(l)];
            next[usize::from(l)] += 1;
            let rev = (code.reverse_bits() >> (32 - u32::from(l))) as usize;
            for e in table.iter_mut().skip(rev).step_by(1 << l) {
                *e = (sym as u32) << 4 | u32::from(l);
            }
        }
        Ok(Self { table, bits })
    }

    fn decode(&self, bits: &mut Bits) -> Result<usize> {
        let e = self.table[bits.peek(self.bits) as usize];
        if e == 0 {
            return err("bad huffman code");
        }
        bits.consume(e & 15)?;
        Ok((e >> 4) as usize)
    }
}

const LEN_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LEN_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

fn codes(bits: &mut Bits, out: &mut Vec<u8>, lit: &Huffman, dist: &Huffman) -> Result<()> {
    loop {
        let sym = lit.decode(bits)?;
        match sym {
            0..=255 => out.push(sym as u8),
            256 => return Ok(()),
            _ => {
                let i = sym - 257;
                if i >= 29 {
                    return err("bad deflate length symbol");
                }
                let len = usize::from(LEN_BASE[i]) + bits.read(u32::from(LEN_EXTRA[i]))? as usize;
                let d = dist.decode(bits)?;
                if d >= 30 {
                    return err("bad deflate distance symbol");
                }
                let dist =
                    usize::from(DIST_BASE[d]) + bits.read(u32::from(DIST_EXTRA[d]))? as usize;
                if dist > out.len() {
                    return err("deflate distance too far back");
                }
                for _ in 0..len {
                    out.push(out[out.len() - dist]);
                }
            }
        }
    }
}

pub fn inflate(data: &[u8]) -> Result<Vec<u8>> {
    let mut bits = Bits {
        data,
        pos: 0,
        buf: 0,
        cnt: 0,
    };
    let mut out = Vec::new();
    loop {
        let last = bits.read(1)?;
        match bits.read(2)? {
            0 => {
                let pos = bits.align();
                let Some(h) = data.get(pos..pos + 4) else {
                    return err("truncated deflate stored block");
                };
                let len = usize::from(u16::from_le_bytes([h[0], h[1]]));
                if u16::from_le_bytes([h[2], h[3]]) != !(len as u16) {
                    return err("deflate stored block length mismatch");
                }
                let Some(body) = data.get(pos + 4..pos + 4 + len) else {
                    return err("truncated deflate stored block");
                };
                out.extend_from_slice(body);
                bits.pos = pos + 4 + len;
            }
            1 => {
                let mut l = [8u8; 288];
                l[144..256].fill(9);
                l[256..280].fill(7);
                codes(
                    &mut bits,
                    &mut out,
                    &Huffman::new(&l)?,
                    &Huffman::new(&[5; 32])?,
                )?;
            }
            2 => {
                let hlit = bits.read(5)? as usize + 257;
                let hdist = bits.read(5)? as usize + 1;
                let hclen = bits.read(4)? as usize + 4;
                const ORDER: [usize; 19] = [
                    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
                ];
                let mut cl = [0u8; 19];
                for &o in &ORDER[..hclen] {
                    cl[o] = bits.read(3)? as u8;
                }
                let clh = Huffman::new(&cl)?;
                let mut lengths = Vec::new();
                while lengths.len() < hlit + hdist {
                    let sym = clh.decode(&mut bits)?;
                    let (val, rep) = match sym {
                        0..=15 => (sym as u8, 1),
                        16 => match lengths.last() {
                            Some(&p) => (p, 3 + bits.read(2)?),
                            None => return err("deflate repeat with no previous length"),
                        },
                        17 => (0, 3 + bits.read(3)?),
                        _ => (0, 11 + bits.read(7)?),
                    };
                    lengths.extend(std::iter::repeat_n(val, rep as usize));
                }
                lengths.truncate(hlit + hdist);
                let (l, d) = lengths.split_at(hlit);
                codes(&mut bits, &mut out, &Huffman::new(l)?, &Huffman::new(d)?)?;
            }
            _ => return err("bad deflate block type"),
        }
        if last == 1 {
            return Ok(out);
        }
    }
}

/// Frames `data` as an Elasticsearch `DFL\0` blob: one fixed-Huffman LZ77 DEFLATE block, or
/// stored blocks when those are smaller (incompressible data).
pub fn frame(data: &[u8]) -> Vec<u8> {
    let mut out = b"DFL\0".to_vec();
    let fixed = fixed_block(data);
    if fixed.len() < data.len() + 5 * data.len().div_ceil(0xFFFF).max(1) {
        out.extend(fixed);
    } else {
        stored_blocks(data, &mut out);
    }
    out
}

struct BitWriter {
    out: Vec<u8>,
    acc: u64,
    n: u32,
}

impl BitWriter {
    fn put(&mut self, v: u32, bits: u32) {
        self.acc |= u64::from(v) << self.n;
        self.n += bits;
        while self.n >= 8 {
            self.out.push(self.acc as u8);
            self.acc >>= 8;
            self.n -= 8;
        }
    }

    /// Writes a Huffman code, which DEFLATE packs most-significant bit first.
    fn code(&mut self, code: u32, len: u32) {
        self.put(code.reverse_bits() >> (32 - len), len);
    }

    fn literal(&mut self, sym: u32) {
        match sym {
            0..=143 => self.code(0x30 + sym, 8),
            144..=255 => self.code(0x190 + sym - 144, 9),
            256..=279 => self.code(sym - 256, 7),
            _ => self.code(0xC0 + sym - 280, 8),
        }
    }
}

// ponytail: fixed Huffman codes and a single-candidate hash match; add dynamic Huffman blocks and
// hash chains if state file size matters.
fn fixed_block(data: &[u8]) -> Vec<u8> {
    let hash = |i: usize| {
        (u32::from_le_bytes([data[i], data[i + 1], data[i + 2], 0]).wrapping_mul(2654435761) >> 17)
            as usize
    };
    let mut w = BitWriter {
        out: Vec::new(),
        acc: 0,
        n: 0,
    };
    w.put(0b011, 3); // BFINAL, BTYPE=01 (fixed Huffman)
    let mut head = vec![usize::MAX; 1 << 15];
    let mut i = 0;
    while i < data.len() {
        let mut len = 0;
        if i + 3 <= data.len() {
            let cand = std::mem::replace(&mut head[hash(i)], i);
            if cand != usize::MAX && i - cand <= 32768 {
                len = data[cand..]
                    .iter()
                    .zip(&data[i..])
                    .take(258)
                    .take_while(|(a, b)| a == b)
                    .count();
            }
            if len >= 3 {
                let l = LEN_BASE
                    .iter()
                    .rposition(|&b| usize::from(b) <= len)
                    .unwrap_or(0);
                w.literal(257 + l as u32);
                w.put(
                    (len - usize::from(LEN_BASE[l])) as u32,
                    u32::from(LEN_EXTRA[l]),
                );
                let dist = i - cand;
                let d = DIST_BASE
                    .iter()
                    .rposition(|&b| usize::from(b) <= dist)
                    .unwrap_or(0);
                w.code(d as u32, 5);
                w.put(
                    (dist - usize::from(DIST_BASE[d])) as u32,
                    u32::from(DIST_EXTRA[d]),
                );
                for j in i + 1..(i + len).min(data.len() - 2) {
                    head[hash(j)] = j;
                }
                i += len;
                continue;
            }
        }
        w.literal(u32::from(data[i]));
        i += 1;
    }
    w.literal(256);
    w.put(0, 7); // flush the last partial byte
    w.out
}

fn stored_blocks(data: &[u8], out: &mut Vec<u8>) {
    let mut blocks = data.chunks(0xFFFF).peekable();
    if blocks.peek().is_none() {
        out.extend([1, 0, 0, 0xFF, 0xFF]);
    }
    while let Some(b) = blocks.next() {
        let len = b.len() as u16;
        out.push(u8::from(blocks.peek().is_none()));
        out.extend(len.to_le_bytes());
        out.extend((!len).to_le_bytes());
        out.extend(b);
    }
}

#[cfg(test)]
mod frame_tests {
    use super::*;

    #[test]
    fn malformed_input_errors() {
        // stored block with wrong NLEN
        assert!(inflate(&[1, 1, 0, 0, 0, 0]).is_err());
        // dynamic block with all-zero-length-1 code lengths (over-subscribed)
        assert!(Huffman::new(&[1, 1, 1]).is_err());
        assert!(Huffman::new(&[2, 2]).is_err());
        assert!(Huffman::new(&[1]).is_ok());
        for seed in 0..2000u32 {
            let d: Vec<u8> = (0..40)
                .map(|i| (seed.wrapping_mul(2654435761).wrapping_add(i * 97) >> 7) as u8)
                .collect();
            let _ = inflate(&d);
        }
    }

    #[test]
    fn decodes_zlib_fixed_and_dynamic_blocks() {
        // Python: zlib.compressobj(9, zlib.DEFLATED, -15) output.
        let fixed = [
            75, 76, 132, 129, 36, 16, 72, 78, 78, 78, 73, 73, 213, 81, 72, 76, 74, 78, 73, 85, 84,
            72, 164, 153, 44, 0,
        ];
        assert_eq!(
            inflate(&fixed).unwrap(),
            b"aaaaaaaaaabbbbbcccdde, abcde! ".repeat(4)
        );
        let dynamic = [
            61, 79, 137, 17, 4, 49, 8, 106, 133, 214, 0, 237, 191, 133, 3, 179, 115, 102, 242, 41,
            2, 146, 226, 206, 154, 92, 218, 28, 49, 137, 132, 179, 192, 217, 254, 146, 116, 211,
            130, 65, 105, 225, 7, 150, 56, 32, 97, 109, 18, 10, 6, 215, 10, 169, 45, 200, 55, 72,
            5, 176, 114, 218, 167, 213, 71, 63, 165, 99, 25, 114, 148, 116, 180, 155, 167, 195, 82,
            161, 10, 222, 158, 136, 97, 202, 116, 21, 126, 93, 254, 88, 10, 84, 145, 49, 36, 212,
            196, 195, 28, 184, 190, 190, 144, 98, 74, 213, 172, 202, 77, 119, 87, 43, 250, 79, 237,
            55, 57, 207, 168, 157, 81, 191, 66, 252, 69, 255, 7,
        ];
        assert_eq!(dynamic[0] >> 1 & 3, 2);
        let out = inflate(&dynamic).unwrap();
        assert_eq!(out.len(), 300);
        assert!(out.starts_with(b"aabaedecaaeaccadbaabaaaacaca adeab"));
        assert!(out.ends_with(b"abaaabbdcbc"));
    }

    #[test]
    fn frame_compresses_repetitive_data() {
        let data = br#"{"index":{"name":"fixture","settings":{}}},"#.repeat(200);
        let framed = frame(&data);
        assert!(framed.len() < data.len() / 10, "{} bytes", framed.len());
        assert_eq!(inflate(&framed[4..]).unwrap(), data);
    }

    #[test]
    fn frame_round_trips() {
        let noise: Vec<u8> = (0..70_000u32)
            .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
            .collect();
        let mixed: Vec<u8> = (0..100_000u32)
            .map(|i| ((i % 251) ^ (i / 1000)) as u8)
            .collect();
        for data in [noise, mixed] {
            assert_eq!(inflate(&frame(&data)[4..]).unwrap(), data);
        }
        for n in [0, 1, 0xFFFF, 0x10000, 150_000] {
            let data: Vec<u8> = (0..n).map(|i| (i * 7) as u8).collect();
            let framed = frame(&data);
            assert_eq!(
                inflate(framed.strip_prefix(b"DFL\0").unwrap()).unwrap(),
                data
            );
        }
    }
}
