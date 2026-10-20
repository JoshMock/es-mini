//! Doc and freq streams of Lucene104 postings (`.doc`).
//!
//! A term's docs come as full blocks of [`BLOCK_SIZE`] (each behind a skip header), then a
//! group-varint tail; terms with fewer docs than a block have only the tail. Every
//! `LEVEL1_NUM_DOCS` (32 blocks) docs a level-1 skip header precedes the next blocks. Skip data is
//! skipped, not used: a whole term is decoded front to back.

use crate::{Result, err, input::DataInput};

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
    let mut out: Vec<(u32, u32)> = Vec::with_capacity(n);
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
}
