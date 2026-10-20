//! Raw DEFLATE (RFC 1951) decoder, for Elasticsearch's `DFL\0`-framed cluster state blobs.

use crate::{Result, err};

struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
    bit: u32,
}

impl Bits<'_> {
    fn read(&mut self, n: u32) -> Result<u32> {
        let mut v = 0;
        for i in 0..n {
            let Some(&b) = self.data.get(self.pos) else {
                return err("truncated deflate stream");
            };
            v |= (u32::from(b >> self.bit) & 1) << i;
            self.bit += 1;
            if self.bit == 8 {
                self.bit = 0;
                self.pos += 1;
            }
        }
        Ok(v)
    }
}

/// Canonical Huffman code: symbol counts per length, and symbols sorted by code.
struct Huffman {
    counts: [u16; 16],
    symbols: Vec<u16>,
}

impl Huffman {
    fn new(lengths: &[u8]) -> Self {
        let mut counts = [0u16; 16];
        for &l in lengths {
            counts[usize::from(l)] += 1;
        }
        counts[0] = 0;
        let mut symbols = Vec::new();
        for len in 1..16u8 {
            for (s, &l) in lengths.iter().enumerate() {
                if l == len {
                    symbols.push(s as u16);
                }
            }
        }
        Self { counts, symbols }
    }

    fn decode(&self, bits: &mut Bits) -> Result<usize> {
        let (mut code, mut first, mut index) = (0i32, 0i32, 0i32);
        for len in 1..16 {
            code |= bits.read(1)? as i32;
            let count = i32::from(self.counts[len]);
            if code - count < first {
                return Ok(usize::from(self.symbols[(index + code - first) as usize]));
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        err("bad huffman code")
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
        bit: 0,
    };
    let mut out = Vec::new();
    loop {
        let last = bits.read(1)?;
        match bits.read(2)? {
            0 => {
                if bits.bit != 0 {
                    bits.bit = 0;
                    bits.pos += 1;
                }
                let Some(h) = data.get(bits.pos..bits.pos + 4) else {
                    return err("truncated deflate stored block");
                };
                let len = usize::from(u16::from_le_bytes([h[0], h[1]]));
                let Some(body) = data.get(bits.pos + 4..bits.pos + 4 + len) else {
                    return err("truncated deflate stored block");
                };
                out.extend_from_slice(body);
                bits.pos += 4 + len;
            }
            1 => {
                let mut l = [8u8; 288];
                l[144..256].fill(9);
                l[256..280].fill(7);
                codes(
                    &mut bits,
                    &mut out,
                    &Huffman::new(&l),
                    &Huffman::new(&[5; 30]),
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
                let clh = Huffman::new(&cl);
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
                codes(&mut bits, &mut out, &Huffman::new(l), &Huffman::new(d))?;
            }
            _ => return err("bad deflate block type"),
        }
        if last == 1 {
            return Ok(out);
        }
    }
}
