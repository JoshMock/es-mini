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

/// Largest impact counts and byte sizes written, for the `.psm` metadata.
#[derive(Default)]
pub struct ImpactMax {
    pub level0_num: i32,
    pub level0_bytes: i32,
    pub level1_num: i32,
    pub level1_bytes: i32,
}

/// Where one term's positions are in `.pos`.
pub struct PosMarks {
    pub start: u64,
    /// `lastPosBlockOffset`: where the vint tail starts, for terms with over a block of positions.
    pub last_block_offset: Option<u64>,
    /// After each full block of docs: the file pointer and how many positions wait in the buffer.
    pub blocks: Vec<(u64, u8)>,
}

/// Lucene's `IndexWriter.MAX_POSITION`.
const MAX_POSITION: u32 = i32::MAX as u32 - 128;

/// `Lucene104PostingsWriter.addPosition` and `finishTerm` without payloads or offsets: position
/// deltas (restarting at each doc) in full PFor blocks of [`BLOCK_SIZE`], then the rest as vints.
pub fn write_positions(out: &mut Output, positions: &[Vec<Position>]) -> Result<PosMarks> {
    let start = out.0.len() as u64;
    let (mut buf, mut blocks, mut total) = (Vec::with_capacity(BLOCK_SIZE), vec![], 0);
    for (i, doc) in positions.iter().enumerate() {
        let mut last = 0;
        for p in doc {
            if p.position < last || p.position > MAX_POSITION {
                return err(format!("position {} out of order or too large", p.position));
            }
            if p.offsets.is_some() || !p.payload.is_empty() {
                return err("offsets and payloads cannot be written");
            }
            buf.push(p.position - last);
            last = p.position;
            if buf.len() == BLOCK_SIZE {
                write_pfor(out, &buf);
                buf.clear();
            }
        }
        total += doc.len();
        if (i + 1) % BLOCK_SIZE == 0 {
            blocks.push((out.0.len() as u64, buf.len() as u8));
        }
    }
    let last_block_offset = (total > BLOCK_SIZE).then(|| out.0.len() as u64 - start);
    buf.iter().for_each(|d| out.vint(*d as i32));
    Ok(PosMarks {
        start,
        last_block_offset,
        blocks,
    })
}

/// Writes one term's docs (ascending) as `read_docs` reads them: full blocks of [`BLOCK_SIZE`]
/// behind skip headers (with level-1 headers around each 32 blocks), then a group-varint tail of
/// doc deltas (low bit set when the freq is 1) and the other freqs. `norms` holds the field's
/// norm per doc for impacts; without it every norm is 1. `pos` adds positions to the skip data.
pub fn write_docs(
    out: &mut Output,
    docs: &[(u32, u32)],
    has_freqs: bool,
    norms: Option<&[i8]>,
    max: &mut ImpactMax,
    pos: Option<&PosMarks>,
) {
    let (mut level0_pos, mut level1_pos) = pos.map_or((0, 0), |p| (p.start, p.start));
    let norm = |d: u32| norms.map_or(1, |n| n[d as usize] as i64);
    let pairs = |docs: &[(u32, u32)]| -> Vec<(u32, i64)> {
        docs.iter().map(|&(d, f)| (f, norm(d))).collect()
    };
    let mut prev = -1i64;
    let (mut group, mut group_pairs, mut group_prev) = (Output::default(), vec![], -1i64);
    let full = docs.len() / BLOCK_SIZE;
    for (i, block) in docs.as_chunks::<BLOCK_SIZE>().0.iter().enumerate() {
        let last = block[BLOCK_SIZE - 1].0 as i64;
        let mut body = Output::default();
        write_full_block(&mut body, block, prev);
        if has_freqs {
            write_pfor(&mut body, &block.iter().map(|d| d.1).collect::<Vec<_>>());
        }
        // After the doc delta and block length: the impacts (when there are freqs), then the body.
        let mut after = Output::default();
        let block_pairs = pairs(block);
        if has_freqs {
            let impacts = competitive(&block_pairs);
            let mut bytes = Output::default();
            write_impacts(&mut bytes, &impacts);
            max.level0_num = max.level0_num.max(impacts.len() as i32);
            max.level0_bytes = max.level0_bytes.max(bytes.0.len() as i32);
            after.vint(bytes.0.len() as i32);
            after.bytes(&bytes.0);
            if let Some(p) = pos {
                let (fp, upto) = p.blocks[i];
                after.vlong((fp - level0_pos) as i64);
                after.byte(upto);
                level0_pos = fp;
            }
        }
        let mut header = Output::default();
        write_vint15(&mut header, (last - prev) as i32);
        write_vlong15(&mut header, (after.0.len() + body.0.len()) as i64);
        header.bytes(&after.0);
        // Blocks of a complete group of 32 wait for their level-1 header; the rest go straight out.
        let in_group = (i / 32 + 1) * 32 <= full;
        let dest = if in_group { &mut group } else { &mut *out };
        dest.vlong(header.0.len() as i64);
        dest.bytes(&header.0);
        dest.bytes(&body.0);
        group_pairs.extend(block_pairs);
        prev = last;
        if in_group && i % 32 == 31 {
            out.vint((last - group_prev) as i32);
            let mut skip = Output::default();
            if has_freqs {
                let impacts = competitive(&group_pairs);
                write_impacts(&mut skip, &impacts);
                let impact_bytes = skip.0.len();
                max.level1_num = max.level1_num.max(impacts.len() as i32);
                max.level1_bytes = max.level1_bytes.max(impact_bytes as i32);
                if let Some(p) = pos {
                    let (fp, upto) = p.blocks[i];
                    skip.vlong((fp - level1_pos) as i64);
                    skip.byte(upto);
                    level1_pos = fp;
                }
                // The header after its length short is the impacts' length short, the impacts
                // and the positions pointer.
                let header = 2 + skip.0.len();
                out.vlong((2 + header + group.0.len()) as i64);
                out.bytes(&(header as i16).to_le_bytes());
                out.bytes(&(impact_bytes as i16).to_le_bytes());
                out.bytes(&skip.0);
            } else {
                out.vlong(group.0.len() as i64);
            }
            out.bytes(&group.0);
            (group, group_pairs, group_prev) = (Output::default(), vec![], last);
        }
    }
    let deltas: Vec<u32> = docs[full * BLOCK_SIZE..]
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
        for &(_, f) in docs[full * BLOCK_SIZE..].iter().filter(|d| d.1 != 1) {
            out.vint(f as i32);
        }
    }
}

/// The `(freq, norm)` pairs no other pair beats (higher or equal freq with a lower or equal
/// norm), ascending by freq and norm: the only ones block-max scoring needs.
fn competitive(pairs: &[(u32, i64)]) -> Vec<(u32, i64)> {
    let mut sorted = pairs.to_vec();
    sorted.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let mut out: Vec<(u32, i64)> = vec![];
    for (f, n) in sorted {
        if out.last().is_none_or(|l| n < l.1) {
            out.push((f, n));
        }
    }
    out.reverse();
    out
}

fn write_impacts(out: &mut Output, impacts: &[(u32, i64)]) {
    let (mut prev_freq, mut prev_norm) = (0, 0);
    for &(f, n) in impacts {
        let freq_delta = (f - prev_freq - 1) as i32;
        let norm_delta = n - prev_norm - 1;
        if norm_delta == 0 {
            out.vint(freq_delta << 1);
        } else {
            out.vint(freq_delta << 1 | 1);
            out.vlong((norm_delta << 1) ^ (norm_delta >> 63));
        }
        (prev_freq, prev_norm) = (f, n);
    }
}

fn write_vint15(out: &mut Output, v: i32) {
    write_vlong15(out, v as i64);
}

fn write_vlong15(out: &mut Output, v: i64) {
    if v & !0x7fff == 0 {
        out.bytes(&(v as i16).to_le_bytes());
    } else {
        out.bytes(&((v & 0x7fff) as i16 | i16::MIN).to_le_bytes());
        out.vlong(v >> 15);
    }
}

/// A block of exactly [`BLOCK_SIZE`] docs after `prev`: a run, a bit set, or packed deltas,
/// whichever is smallest.
fn write_full_block(out: &mut Output, block: &[(u32, u32)], prev: i64) {
    let range = (block[BLOCK_SIZE - 1].0 as i64 - prev) as usize;
    let mut last = prev;
    let deltas: Vec<u32> = block
        .iter()
        .map(|d| {
            let delta = (d.0 as i64 - last) as u32;
            last = d.0 as i64;
            delta
        })
        .collect();
    let bits = 32 - deltas.iter().fold(0, |a, d| a | d).leading_zeros();
    let words = range.div_ceil(64);
    if range == BLOCK_SIZE {
        out.byte(0);
    } else if words < bits as usize * 4 {
        out.byte(-(words as i8) as u8);
        let mut set = vec![0u64; words];
        for d in block {
            let i = (d.0 as i64 - prev - 1) as usize;
            set[i / 64] |= 1 << (i % 64);
        }
        set.iter().for_each(|w| out.long(*w as i64));
    } else {
        out.byte(bits as u8);
        write_for(out, &deltas, bits);
    }
}

/// `ForUtil.encode`, the inverse of [`read_for`].
fn write_for(out: &mut Output, ints: &[u32], bits: u32) {
    let lane_bits = match bits {
        ..=8 => 8,
        9..=16 => 16,
        _ => 32,
    };
    let lanes = (32 / lane_bits) as usize;
    let mut words = vec![0u32; bits as usize * 8];
    for lane in 0..lanes {
        let shift = 32 - lane_bits * (lane as u32 + 1);
        let count = BLOCK_SIZE / lanes;
        let mut lane_words = vec![0u32; words.len()];
        pack_lane(
            &ints[lane * count..][..count],
            &mut lane_words,
            lane_bits,
            bits,
        );
        for (w, l) in words.iter_mut().zip(lane_words) {
            *w |= l << shift;
        }
    }
    words.iter().for_each(|w| out.bytes(&w.to_le_bytes()));
}

/// Inverse of [`unpack_lane`].
fn pack_lane(ints: &[u32], words: &mut [u32], word_bits: u32, bits: u32) {
    let per_shift = (bits * 8) as usize;
    let mut idx = 0;
    let mut shift = word_bits as i32 - bits as i32;
    while shift >= 0 {
        for w in &mut words[..per_shift] {
            *w |= ints[idx] << shift;
            idx += 1;
        }
        shift -= bits as i32;
    }
    let leftover = (shift + bits as i32) as u32;
    let (mut word, mut remaining) = (0, bits);
    while idx < ints.len() {
        if remaining >= leftover {
            remaining -= leftover;
            words[word] |= (ints[idx] >> remaining) & mask(leftover);
            word += 1;
            if remaining == 0 {
                idx += 1;
                remaining = bits;
            }
        } else {
            words[word] |= (ints[idx] & mask(remaining)) << (leftover - remaining);
            idx += 1;
            let next = bits - leftover + remaining;
            words[word] |= (ints[idx] >> next) & mask(leftover - remaining);
            remaining = next;
            word += 1;
        }
    }
}

/// `PForUtil.encode`: up to 7 values too wide for the block keep their high bits (at most 8) as
/// patched exceptions, so a few huge freqs do not widen the whole block.
fn write_pfor(out: &mut Output, ints: &[u32]) {
    const MAX_EXCEPTIONS: usize = 7;
    let bits_of = |v: u32| (32 - v.leading_zeros()).max(1);
    let mut histogram = [0usize; 33];
    ints.iter()
        .for_each(|&v| histogram[bits_of(v) as usize] += 1);
    let max_bits = ints.iter().map(|&v| bits_of(v)).max().unwrap_or(1);
    assert!(max_bits < 32, "freqs are below 2^31");
    let (mut bits, mut exceptions, mut cumulative) = (max_bits, 0, 0);
    for b in (max_bits.saturating_sub(8)..=max_bits).rev() {
        if cumulative > MAX_EXCEPTIONS {
            break;
        }
        (bits, exceptions) = (b, cumulative);
        cumulative += histogram[b as usize];
    }
    let mut ints = ints.to_vec();
    let mut patches = Vec::with_capacity(exceptions * 2);
    for (i, v) in ints.iter_mut().enumerate() {
        if *v >> bits != 0 {
            patches.extend([i as u8, (*v >> bits) as u8]);
            *v &= mask(bits);
        }
    }
    if ints.iter().all(|&i| i == ints[0]) && max_bits <= 8 {
        patches
            .iter_mut()
            .skip(1)
            .step_by(2)
            .for_each(|p| *p <<= bits);
        out.byte((exceptions << 5) as u8);
        out.vint(ints[0] as i32);
    } else {
        out.byte((exceptions << 5) as u8 | bits as u8);
        write_for(out, &ints, bits);
    }
    out.bytes(&patches);
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
            write_docs(
                &mut out,
                &docs,
                has_freqs,
                None,
                &mut ImpactMax::default(),
                None,
            );
            assert_eq!(read_docs(&out.0, 0, docs.len(), has_freqs).unwrap(), docs);
        }
    }

    #[test]
    fn full_blocks_round_trip() {
        // Dense runs, bit sets, packed deltas, varied freqs, and level-1 skip data (20000 docs).
        let docs_for = |gap: &dyn Fn(u32) -> u32| {
            let mut d = 0;
            (0..20000u32)
                .map(|i| {
                    d += gap(i);
                    (
                        d,
                        1 + i % 7 * (i % 11) + if i % 997 == 0 { 1000 } else { 0 },
                    )
                })
                .collect::<Vec<_>>()
        };
        let norms: Vec<i8> = (0..20_000_000).map(|i| (i % 50) as i8).collect();
        for docs in [
            docs_for(&|_| 1),
            docs_for(&|i| 1 + i % 3),
            docs_for(&|i| 1 + (i * 7919) % 500),
            docs_for(&|i| if i % 300 == 0 { 70000 } else { 1 }),
        ] {
            for n in [255, 256, 257, 512, 8191, 8192, 8448, 20000] {
                for has_freqs in [true, false] {
                    let want: Vec<_> = docs[..n]
                        .iter()
                        .map(|&(d, f)| (d, if has_freqs { f } else { 1 }))
                        .collect();
                    let mut out = Output::default();
                    let mut max = ImpactMax::default();
                    write_docs(&mut out, &want, has_freqs, Some(&norms), &mut max, None);
                    assert_eq!(read_docs(&out.0, 0, n, has_freqs).unwrap(), want, "{n}");
                }
            }
        }
    }

    #[test]
    fn packed_blocks_round_trip_at_every_width() {
        for bits in 1..=32u32 {
            let ints: Vec<u32> = (0..BLOCK_SIZE as u64)
                .map(|i| ((i * 2654435761) as u32) & mask(bits))
                .collect();
            let mut out = Output::default();
            write_for(&mut out, &ints, bits);
            assert_eq!(read_for(&mut DataInput::new(&out.0), bits).unwrap(), ints);
        }
    }

    #[test]
    fn huge_freqs_become_patched_exceptions() {
        // (block, expected token, expected size): small values plus a few wide outliers keep
        // the narrow width and patch the outliers' high bits; too many outliers widen the
        // block; equal small values with outliers patch over a single vint.
        let block = |base: u32, outliers: &[(usize, u32)]| {
            let mut b: Vec<u32> = (0..BLOCK_SIZE as u32).map(|i| base + i % 3).collect();
            outliers.iter().for_each(|&(i, v)| b[i] = v);
            b
        };
        let mut flat = vec![1; BLOCK_SIZE];
        flat[9] = 201;
        let cases = [
            (block(1, &[(5, 600), (200, 1000)]), 2 << 5 | 2, 1 + 64 + 4),
            (
                block(1, &(0..8).map(|i| (i, 300)).collect::<Vec<_>>()),
                9,
                1 + 288,
            ),
            (flat, 1 << 5, 1 + 1 + 2),
            (vec![1000; BLOCK_SIZE], 10, 1 + 320),
        ];
        for (ints, token, len) in cases {
            let mut out = Output::default();
            write_pfor(&mut out, &ints);
            assert_eq!((out.0[0], out.0.len()), (token, len));
            assert_eq!(read_pfor(&mut DataInput::new(&out.0)).unwrap(), ints);
        }
    }
}
