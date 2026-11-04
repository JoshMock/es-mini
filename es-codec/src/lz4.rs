//! Lucene's LZ4 block format, shared by stored fields, the terms dictionary, and doc values.

use crate::{Result, err, input::DataInput, output::Output};

/// Lucene `LZ4.decompress`: reads until `out` has grown by `len` bytes. Matches may reference
/// earlier bytes of `out` (the preset dictionary).
pub(crate) fn decompress(inp: &mut DataInput, len: usize, out: &mut Vec<u8>) -> Result<()> {
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

/// Literal-only LZ4 stream, which any LZ4 decoder accepts.
pub(crate) fn literals(out: &mut Output, lits: &[u8]) {
    encode_literals(out, (lits.len().min(15) << 4) as u8, lits);
}

/// `LZ4.encodeLiterals`.
fn encode_literals(out: &mut Output, token: u8, lits: &[u8]) {
    out.byte(token);
    if lits.len() >= 15 {
        encode_len(out, lits.len() - 15);
    }
    out.bytes(lits);
}

fn encode_len(out: &mut Output, mut l: usize) {
    while l >= 0xff {
        out.byte(0xff);
        l -= 0xff;
    }
    out.byte(l as u8);
}

const MAX_DISTANCE: usize = 1 << 16;
const MASK: usize = MAX_DISTANCE - 1;
const MIN_MATCH: usize = 4;
const LAST_LITERALS: usize = 5;
const MEMORY_USAGE: u32 = 14;
const HASH_LOG_HC: u32 = 15;
const MAX_ATTEMPTS: usize = 256;

/// Lucene `LZ4.HashTable`: finds earlier positions holding the same 4 bytes.
pub(crate) trait HashTable {
    /// Starts compressing `b`, whose first `dict_len` bytes are the dictionary.
    fn reset(&mut self, b: &[u8], dict_len: usize);
    /// An earlier position holding `b[off..off + 4]`; `off` only increases.
    fn get(&mut self, b: &[u8], off: usize) -> Option<usize>;
    /// A position before `off` holding the same 4 bytes.
    fn previous(&mut self, b: &[u8], off: usize) -> Option<usize>;
}

/// Lucene `LZ4.FastCompressionHashTable`: the last position of each hash. Lucene never clears
/// it between inputs (its doc values keep one per terms dictionary), and stale positions that
/// still match the new input are used, so keep one as long as Lucene does.
#[derive(Default)]
pub(crate) struct FastTable {
    /// Lucene's `Table16` or `Table32`, by `bits`; positions start at 0, a valid reference.
    table: Vec<u32>,
    bits: u32,
    hash_log: u32,
}

impl HashTable for FastTable {
    fn reset(&mut self, b: &[u8], dict_len: usize) {
        let bits: u32 = if b.len() - LAST_LITERALS < 1 << 16 {
            16
        } else {
            32
        };
        self.hash_log = MEMORY_USAGE + 3 - bits.ilog2();
        if self.table.len() < 1 << self.hash_log || self.bits < bits {
            (self.table, self.bits) = (vec![0; 1 << self.hash_log], bits);
        }
        // `initDictionary`
        for i in 0..dict_len {
            self.table[hash(read_int(b, i), self.hash_log)] = i as u32;
        }
    }

    fn get(&mut self, b: &[u8], off: usize) -> Option<usize> {
        let v = read_int(b, off);
        let r = std::mem::replace(&mut self.table[hash(v, self.hash_log)], off as u32) as usize;
        (r < off && off - r < MAX_DISTANCE && read_int(b, r) == v).then_some(r)
    }

    fn previous(&mut self, _: &[u8], _: usize) -> Option<usize> {
        None
    }
}

/// Lucene `LZ4.HighCompressionHashTable`. Lucene keeps one per terms field and does not clear
/// its heads between short inputs, so stale heads can change which match is found: keep one as
/// long as Lucene does to write the same bytes.
pub(crate) struct HcTable {
    /// The last input's length (Lucene's `end`; `base` is always 0 here).
    end: usize,
    hash: Vec<i32>,
    chain: Vec<u16>,
    next: usize,
    attempts: usize,
}

impl Default for HcTable {
    fn default() -> Self {
        HcTable {
            end: 0,
            hash: vec![-1; 1 << HASH_LOG_HC],
            chain: vec![0xffff; MAX_DISTANCE],
            next: 0,
            attempts: 0,
        }
    }
}

/// Native-endian in Lucene; little-endian on every platform Elasticsearch ships for.
fn read_int(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}

fn hash(v: u32, bits: u32) -> usize {
    (v.wrapping_mul(0x9E37_79B1) >> (32 - bits)) as usize
}

impl HcTable {
    fn add_hash(&mut self, b: &[u8], off: usize) {
        let h = hash(read_int(b, off), HASH_LOG_HC);
        let delta = off as i64 - i64::from(self.hash[h]);
        let delta = if delta <= 0 || delta >= MAX_DISTANCE as i64 {
            MAX_DISTANCE - 1
        } else {
            delta as usize
        };
        self.chain[off & MASK] = delta as u16;
        self.hash[h] = off as i32;
    }

    /// Follows the chain from `r` to an earlier position holding `v`, within `min..`.
    fn walk(&mut self, b: &[u8], v: u32, mut r: i64, min: i64) -> Option<usize> {
        while r >= min && self.attempts < MAX_ATTEMPTS {
            if read_int(b, r as usize) == v {
                return Some(r as usize);
            }
            r -= i64::from(self.chain[r as usize & MASK]);
            self.attempts += 1;
        }
        None
    }
}

impl HashTable for HcTable {
    /// `initDictionary` is left to the first `get`, which adds the same hashes.
    fn reset(&mut self, b: &[u8], _: usize) {
        if self.end < MAX_DISTANCE {
            self.chain[..self.end].fill(0xffff);
        } else {
            self.hash.fill(-1);
            self.chain.fill(0xffff);
        }
        self.next = 0;
        self.end = b.len();
    }

    fn get(&mut self, b: &[u8], off: usize) -> Option<usize> {
        while self.next < off {
            self.add_hash(b, self.next);
            self.next += 1;
        }
        let v = read_int(b, off);
        self.attempts = 0;
        let r = i64::from(self.hash[hash(v, HASH_LOG_HC)]);
        if r >= off as i64 {
            return None; // left from a previous input
        }
        self.walk(b, v, r, off.saturating_sub(MAX_DISTANCE - 1) as i64)
    }

    fn previous(&mut self, b: &[u8], off: usize) -> Option<usize> {
        let r = off as i64 - i64::from(self.chain[off & MASK]);
        self.walk(b, read_int(b, off), r, 0)
    }
}

/// Lucene `LZ4.compressWithDictionary`: compresses `b[dict_len..]`, whose matches may reference
/// the dictionary `b[..dict_len]`.
pub(crate) fn compress(b: &[u8], dict_len: usize, out: &mut Output, ht: &mut impl HashTable) {
    let len = b.len();
    let (mut off, mut anchor) = (dict_len, dict_len);
    if len - dict_len > LAST_LITERALS + MIN_MATCH {
        let limit = len - LAST_LITERALS;
        let match_limit = limit - MIN_MATCH;
        ht.reset(b, dict_len);
        // `Arrays.mismatch` over `[o1, limit)` and `[o2, limit)`.
        let common = |o1: usize, o2: usize| {
            (0..limit - o2)
                .take_while(|&k| b[o1 + k] == b[o2 + k])
                .count()
        };
        'main: while off <= limit {
            let mut found = loop {
                if off >= match_limit {
                    break 'main;
                }
                if let Some(r) = ht.get(b, off) {
                    break r;
                }
                off += 1;
            };
            let mut match_len = MIN_MATCH + common(found + MIN_MATCH, off + MIN_MATCH);
            let min = off.saturating_sub(MAX_DISTANCE - 1);
            let mut r = ht.previous(b, found);
            while let Some(c) = r.filter(|&c| c >= min) {
                let l = MIN_MATCH + common(c + MIN_MATCH, off + MIN_MATCH);
                if l > match_len {
                    (found, match_len) = (c, l);
                }
                r = ht.previous(b, c);
            }
            // `encodeSequence`
            let lits = &b[anchor..off];
            let token = (lits.len().min(15) << 4) | (match_len - MIN_MATCH).min(15);
            encode_literals(out, token as u8, lits);
            let dec = off - found;
            out.bytes(&(dec as u16).to_le_bytes());
            if match_len >= MIN_MATCH + 15 {
                encode_len(out, match_len - 15 - MIN_MATCH);
            }
            off += match_len;
            anchor = off;
        }
    }
    literals(out, &b[anchor..]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literals_and_matches() {
        // 5 literals "abcde", then a match of 4 + 2 bytes at offset 3: "cdecde".
        let block = [0x52, b'a', b'b', b'c', b'd', b'e', 3, 0];
        let mut out = Vec::new();
        decompress(&mut DataInput::new(&block), 11, &mut out).unwrap();
        assert_eq!(out, b"abcdecdecde");
        assert!(decompress(&mut DataInput::new(&[0x10, b'a', 9, 0]), 10, &mut vec![]).is_err());
    }

    #[test]
    fn compress_round_trips_and_finds_matches() {
        let mut ht = HcTable::default();
        let long: Vec<u8> = (0..70_000u32)
            .map(|i| ((i % 251) ^ (i / 997)) as u8)
            .collect();
        for input in [
            &b"abcdefabcdefabcdefabcdef"[..],
            b"short",
            b"0123456789",
            &long,
        ] {
            let mut c = Output::default();
            compress(input, 0, &mut c, &mut ht);
            let mut out = Vec::new();
            decompress(&mut DataInput::new(&c.0), input.len(), &mut out).unwrap();
            assert_eq!(out, input);
        }
        let mut c = Output::default();
        compress(&[b'z'; 100], 0, &mut c, &mut ht);
        // 1 literal, a 94-byte match at offset 1, then the last 5 bytes as literals.
        assert_eq!(
            c.0,
            [0x1f, b'z', 1, 0, 75, 0x50, b'z', b'z', b'z', b'z', b'z']
        );
    }

    #[test]
    fn long_literal_and_long_match_continuations() {
        // 300 literals: token 0xF0, extra 255 + 30 (15+255+30 = 300); then the
        // final-literal-only block ends the stream.
        let lits: Vec<u8> = (0..300).map(|i| i as u8).collect();
        let mut block = vec![0xf0, 0xff, 30];
        block.extend_from_slice(&lits);
        let mut out = Vec::new();
        decompress(&mut DataInput::new(&block), 300, &mut out).unwrap();
        assert_eq!(out, lits);
        // The writer's literal encoding must round-trip through the reader.
        let mut w = Output::default();
        literals(&mut w, &lits);
        let mut out = Vec::new();
        decompress(&mut DataInput::new(&w.0), 300, &mut out).unwrap();
        assert_eq!(out, lits);
        // 1 literal, then a match of 4 + 15 + 255 + 5 = 279 bytes at offset 1.
        let block = [0x1f, b'x', 1, 0, 0xff, 5];
        let mut out = Vec::new();
        decompress(&mut DataInput::new(&block), 280, &mut out).unwrap();
        assert_eq!(out, vec![b'x'; 280]);
    }
}
