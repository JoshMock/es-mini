//! Doc and freq streams of Lucene104 postings (`.doc`).
//!
//! A term's docs come as full blocks of [`BLOCK_SIZE`] (each behind a skip header), then a
//! group-varint tail; terms with fewer docs than a block have only the tail. Every
//! `LEVEL1_NUM_DOCS` (32 blocks) docs a level-1 skip header precedes the next blocks. Skip data is
//! skipped, not used: a whole term is decoded front to back.

use crate::{Result, err, input::DataInput, output::Output};

pub const BLOCK_SIZE: usize = 256;
/// Docs per level-1 skip entry (`LEVEL1_FACTOR` 32 blocks).
const LEVEL1_NUM_DOCS: usize = 32 * BLOCK_SIZE;
const NO_MORE_DOCS: i64 = i32::MAX as i64;

/// All `(doc, freq)` of a term whose postings start at `fp` in the `.doc` file. Freqs are 1
/// when the field has none.
pub(crate) fn read_docs(
    file: &[u8],
    fp: usize,
    n: usize,
    has_freqs: bool,
) -> Result<Vec<(u32, u32)>> {
    let input = &mut DataInput::new(file);
    input.pos = fp;
    // `n` comes from the file, so it is not trusted for a preallocation.
    let mut out: Vec<(u32, u32)> = Vec::new();
    let mut prev = -1i64;
    let mut level1_last = if n >= LEVEL1_NUM_DOCS {
        -1
    } else {
        NO_MORE_DOCS
    };
    let (mut level1_upto, mut level1_end) = (0, 0);
    while out.len() < n {
        let left = n - out.len();
        if prev == level1_last {
            if level1_upto > 0 && input.pos != level1_end {
                return err("level 1 skip span does not end where announced");
            }
            level1_upto += LEVEL1_NUM_DOCS;
            if left < LEVEL1_NUM_DOCS {
                level1_last = NO_MORE_DOCS;
            } else {
                level1_last += input.read_vint()? as i64;
                let delta = input.read_vlong()? as usize;
                level1_end = input.pos + delta;
                if has_freqs {
                    let len = input.read_short()?;
                    input.pos += len as usize; // skip data: impacts and positions
                }
            }
        }
        let block: Vec<u32>;
        let mut freqs = vec![1u32; BLOCK_SIZE.min(left)];
        if left >= BLOCK_SIZE {
            let header_len = input.read_vlong()? as usize;
            let header_end = input.pos + header_len;
            let last_doc = prev + read_vint15(input)? as i64;
            input.pos = header_end;
            block = read_full_block(input, prev)?;
            if block[BLOCK_SIZE - 1] as i64 != last_doc {
                return err("block does not end at the doc id its skip header announced");
            }
            if has_freqs {
                freqs = read_pfor(input)?;
            }
        } else {
            let mut docs = read_group_vints(input, left)?;
            if has_freqs {
                for (d, f) in docs.iter_mut().zip(&mut freqs) {
                    *f = if *d & 1 == 1 {
                        1
                    } else {
                        input.read_vint()? as u32
                    };
                    *d >>= 1;
                }
            }
            let mut doc = prev;
            block = docs
                .into_iter()
                .map(|delta| {
                    doc += delta as i64;
                    doc as u32
                })
                .collect();
        }
        prev = *block.last().expect("blocks are not empty") as i64;
        out.extend(block.into_iter().zip(freqs));
    }
    Ok(out)
}

/// Writes one term's docs (fewer than [`BLOCK_SIZE`], ascending) in the group-varint tail form
/// `read_docs` reads: all doc deltas (low bit set when the freq is 1), then the other freqs.
pub fn write_docs_tail(out: &mut Output, docs: &[(u32, u32)], has_freqs: bool) {
    assert!(docs.len() < BLOCK_SIZE, "full blocks are not written yet");
    let mut prev = -1i64;
    let deltas: Vec<u32> = docs
        .iter()
        .map(|&(d, f)| {
            let delta = (d as i64 - prev) as u32;
            prev = d as i64;
            if has_freqs {
                (delta << 1) | (f == 1) as u32
            } else {
                delta
            }
        })
        .collect();
    let len = |v: u32| (4 - v.leading_zeros() as usize / 8).max(1);
    let (chunks, rest) = deltas.as_chunks::<4>();
    for c in chunks {
        out.byte(c.iter().fold(0, |f, &v| (f << 2) | (len(v) - 1) as u8));
        for &v in c {
            out.bytes(&v.to_le_bytes()[..len(v)]);
        }
    }
    for &v in rest {
        out.vint(v as i32);
    }
    if has_freqs {
        for &(_, f) in docs.iter().filter(|d| d.1 != 1) {
            out.vint(f as i32);
        }
    }
}

fn read_vint15(input: &mut DataInput) -> Result<i32> {
    let s = input.read_short()?;
    Ok(if s >= 0 {
        s as i32
    } else {
        (s as i32 & 0x7fff) | (input.read_vint()? << 15)
    })
}

/// A block of exactly [`BLOCK_SIZE`] doc ids after `prev`: packed deltas, a bit set, or a run.
fn read_full_block(input: &mut DataInput, prev: i64) -> Result<Vec<u32>> {
    let bits = input.read_byte()? as i8;
    if bits > 0 {
        let mut doc = prev;
        return Ok(read_for(input, bits as u32)?
            .into_iter()
            .map(|delta| {
                doc += delta as i64;
                doc as u32
            })
            .collect());
    }
    let base = prev + 1;
    if bits == 0 {
        return Ok((base..base + BLOCK_SIZE as i64).map(|d| d as u32).collect());
    }
    let mut docs = Vec::with_capacity(BLOCK_SIZE);
    for word in 0..-(bits as i32) {
        let w = input.read_long()? as u64;
        docs.extend(
            (0..64)
                .filter(|b| w >> b & 1 == 1)
                .map(|b| (base + word as i64 * 64 + b) as u32),
        );
    }
    if docs.len() != BLOCK_SIZE {
        return err("doc bit set block does not hold a full block of docs");
    }
    Ok(docs)
}

/// `ForUtil.decode`: [`BLOCK_SIZE`] ints of `bits` bits, interleaved across 32-bit words.
fn read_for(input: &mut DataInput, bits: u32) -> Result<Vec<u32>> {
    if !(1..=32).contains(&bits) {
        return err(format!("invalid packed bits per value {bits}"));
    }
    let words: Vec<u32> = (0..bits * 8)
        .map(|_| input.read_int().map(|i| i as u32))
        .collect::<Result<_>>()?;
    // Values narrower than 16 bits share a word: 4 lanes of 8 bits or 2 lanes of 16.
    let lane_bits = match bits {
        ..=8 => 8,
        9..=16 => 16,
        _ => 32,
    };
    let lanes = 32 / lane_bits;
    let mut out = Vec::with_capacity(BLOCK_SIZE);
    for lane in 0..lanes {
        let shift = 32 - lane_bits * (lane + 1);
        let lane_words: Vec<u32> = words
            .iter()
            .map(|w| (w >> shift) & mask(lane_bits))
            .collect();
        out.extend(unpack_lane(
            &lane_words,
            lane_bits,
            bits,
            BLOCK_SIZE / lanes as usize,
        )?);
    }
    Ok(out)
}

fn mask(bits: u32) -> u32 {
    if bits >= 32 { !0 } else { (1 << bits) - 1 }
}

/// Inverse of `ForUtil.encode(ints, bits, primitiveSize, ..)` for one lane of `count` values:
/// whole values fill each word from the top, then the leftover low bits of consecutive words
/// hold the remaining values, split wherever a value does not fit.
fn unpack_lane(words: &[u32], word_bits: u32, bits: u32, count: usize) -> Result<Vec<u32>> {
    let per_shift = (bits * 8) as usize;
    let mut out = vec![0u32; count];
    let mut idx = 0;
    let mut shift = word_bits as i32 - bits as i32;
    while shift >= 0 {
        for w in &words[..per_shift] {
            out[idx] = (w >> shift) & mask(bits);
            idx += 1;
        }
        shift -= bits as i32;
    }
    let leftover = (shift + bits as i32) as u32;
    let (mut word, mut remaining) = (0, bits);
    while idx < count {
        let Some(&w) = words.get(word) else {
            return err("packed block ran out of words");
        };
        if remaining >= leftover {
            remaining -= leftover;
            out[idx] |= (w & mask(leftover)) << remaining;
            word += 1;
            if remaining == 0 {
                idx += 1;
                remaining = bits;
            }
        } else {
            out[idx] |= (w >> (leftover - remaining)) & mask(remaining);
            idx += 1;
            let next = bits - leftover + remaining;
            let Some(slot) = out.get_mut(idx) else {
                return err("packed block overflows");
            };
            *slot |= (w & mask(leftover - remaining)) << next;
            remaining = next;
            word += 1;
        }
    }
    Ok(out)
}

/// `PForUtil.decode`: a [`ForUtil`](read_for) block patched with a few high-bit exceptions.
fn read_pfor(input: &mut DataInput) -> Result<Vec<u32>> {
    let token = input.read_byte()? as u32;
    let (bits, exceptions) = (token & 0x1f, token >> 5);
    let mut ints = match bits {
        0 => vec![input.read_vint()? as u32; BLOCK_SIZE],
        _ => read_for(input, bits)?,
    };
    for _ in 0..exceptions {
        let (i, high) = (input.read_byte()? as usize, input.read_byte()? as u32);
        ints[i] |= high << bits;
    }
    Ok(ints)
}

/// One occurrence of a term in a doc.
#[derive(Debug, Clone, PartialEq)]
pub struct Position {
    pub position: u32,
    /// `(start, end)` character offsets, for fields indexed with offsets.
    pub offsets: Option<(u32, u32)>,
    /// Empty when the occurrence has no payload.
    pub payload: Vec<u8>,
}

/// Where a term's positions are, and what its field stores beside them.
pub(crate) struct PositionFiles<'a> {
    pub pos: &'a [u8],
    pub pay: &'a [u8],
    pub pos_fp: usize,
    pub pay_fp: usize,
    pub offsets: bool,
    pub payloads: bool,
}

/// One occurrence as stored: deltas, not yet resolved against its doc.
#[derive(Default, Clone)]
struct Raw {
    delta: u32,
    payload: Vec<u8>,
    start_delta: u32,
    offset_len: u32,
}

/// Positions, offsets, and payloads for every doc of a term, given each doc's freq. Full blocks
/// of [`BLOCK_SIZE`] occurrences are packed in `.pos` (payloads and offsets in `.pay`); the
/// remaining `total % BLOCK_SIZE` are varints in `.pos`.
pub(crate) fn read_positions(f: &PositionFiles, freqs: &[u32]) -> Result<Vec<Vec<Position>>> {
    let total: usize = freqs.iter().map(|f| *f as usize).sum();
    let pos = &mut DataInput::new(f.pos);
    pos.pos = f.pos_fp;
    let pay = &mut DataInput::new(f.pay);
    pay.pos = f.pay_fp;
    let mut raw: Vec<Raw> = Vec::new();
    for _ in 0..total / BLOCK_SIZE {
        let mut block = vec![Raw::default(); BLOCK_SIZE];
        for (r, d) in block.iter_mut().zip(read_pfor(pos)?) {
            r.delta = d;
        }
        if f.payloads {
            let lengths = read_pfor(pay)?;
            let len = pay.read_len()?;
            let mut bytes = DataInput::new(pay.read_bytes(len)?);
            for (r, l) in block.iter_mut().zip(lengths) {
                r.payload = bytes.read_bytes(l as usize)?.to_vec();
            }
        }
        if f.offsets {
            let (starts, lengths) = (read_pfor(pay)?, read_pfor(pay)?);
            for (r, (s, l)) in block.iter_mut().zip(starts.into_iter().zip(lengths)) {
                (r.start_delta, r.offset_len) = (s, l);
            }
        }
        raw.extend(block);
    }
    // The tail repeats the previous payload or offset length unless a flag bit says otherwise.
    let (mut payload_len, mut offset_len) = (0, 0);
    for _ in 0..total % BLOCK_SIZE {
        let mut r = Raw::default();
        let code = pos.read_vint()? as u32;
        if f.payloads {
            if code & 1 != 0 {
                payload_len = pos.read_len()?;
            }
            r.delta = code >> 1;
            r.payload = pos.read_bytes(payload_len)?.to_vec();
        } else {
            r.delta = code;
        }
        if f.offsets {
            let code = pos.read_vint()? as u32;
            if code & 1 != 0 {
                offset_len =
                    u32::try_from(pos.read_vint()?).or_else(|_| err("negative offset length"))?;
            }
            (r.start_delta, r.offset_len) = (code >> 1, offset_len);
        }
        raw.push(r);
    }
    let mut raw = raw.into_iter();
    freqs
        .iter()
        .map(|&freq| {
            let (mut position, mut last_start) = (0, 0);
            (0..freq)
                .map(|_| {
                    let r = raw.next().expect("counted above");
                    position += r.delta;
                    let offsets = f.offsets.then(|| {
                        last_start += r.start_delta;
                        (last_start, last_start + r.offset_len)
                    });
                    Ok(Position {
                        position,
                        offsets,
                        payload: r.payload,
                    })
                })
                .collect()
        })
        .collect()
}

/// `GroupVIntUtil.readGroupVInts`: groups of four ints behind a length byte, then plain vints.
fn read_group_vints(input: &mut DataInput, n: usize) -> Result<Vec<u32>> {
    let mut out = Vec::with_capacity(n);
    while out.len() + 4 <= n {
        let flag = input.read_byte()?;
        for shift in [6, 4, 2, 0] {
            let bytes = ((flag >> shift) & 3) as usize + 1;
            let mut v = [0u8; 4];
            v[..bytes].copy_from_slice(input.read_bytes(bytes)?);
            out.push(u32::from_le_bytes(v));
        }
    }
    while out.len() < n {
        out.push(input.read_vint()? as u32);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_varints_then_plain_vints() {
        // Lengths 1, 2, 3, 4 bytes (flag 0b00_01_10_11), then the tail as vints 5 and 200.
        let mut b = vec![
            0b0001_1011,
            1,
            0x2c,
            0x01,
            0x70,
            0x11,
            0x01,
            0xff,
            0xff,
            0xff,
            0x7f,
        ];
        b.extend([5, 0xc8, 0x01]);
        let got = read_group_vints(&mut DataInput::new(&b), 6).unwrap();
        assert_eq!(got, [1, 300, 70000, 0x7fff_ffff, 5, 200]);
    }

    #[test]
    fn docs_tail_round_trips() {
        for has_freqs in [true, false] {
            let docs: Vec<(u32, u32)> = [0u32, 1, 5, 300, 70000, 70001, 1 << 24, 1 << 25, 1 << 26]
                .iter()
                .enumerate()
                .map(|(i, &d)| (d, if has_freqs { 1 + (i as u32 % 3) } else { 1 }))
                .collect();
            let mut out = Output::default();
            write_docs_tail(&mut out, &docs, has_freqs);
            assert_eq!(read_docs(&out.0, 0, docs.len(), has_freqs).unwrap(), docs);
        }
    }
}
