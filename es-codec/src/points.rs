//! Lucene 10 BKD points (`.kdm`/`.kdi`/`.kdd`): a reader of one-dimensional trees that narrows a
//! range by walking them, and their writer (`BKDWriter` for one dimension).

use crate::{
    Result, SegmentReader,
    codec_util::{self, ID_LEN},
    err,
    input::DataInput,
    output::Output,
};
use std::cmp::Ordering;

/// The value of sortable big-endian point bytes 1 to 8 wide, as doc values hold it (inverse of
/// [`sortable_bytes`]; doubles and floats come back as their sortable bits).
pub fn decode(v: &[u8]) -> i64 {
    let shift = 64 - 8 * v.len() as u32;
    let raw = v.iter().fold(0u64, |a, b| a << 8 | u64::from(*b));
    ((raw << shift) as i64 ^ i64::MIN) >> shift
}

/// [`SegmentReader::point_docs`].
// ponytail: reads the whole .kdd per call; slice out the leaves it visits if files get huge.
pub(crate) fn read_point_docs(
    r: &SegmentReader,
    field: &str,
    place: impl Fn(i64) -> Ordering,
) -> Result<Vec<u32>> {
    let info = r.field(field)?;
    if (info.point_dimensions, info.point_index_dimensions) != (1, 1) {
        return err(format!("{field} has no one-dimensional points"));
    }
    let (meta, index, data) = (
        r.cfs_file(".kdm")?,
        r.cfs_file(".kdi")?,
        r.cfs_file(".kdd")?,
    );
    let id = &r.seg.info.id;
    let m = &mut DataInput::new(&meta);
    for (file, codec) in [(&meta, "Meta"), (&index, "Index"), (&data, "Data")] {
        codec_util::check_footer(file)?;
        let name = format!("Lucene90PointsFormat{codec}");
        // version 0 (BKD 9) encodes 24-bit doc ids differently; callers fall back to doc values
        codec_util::check_index_header(&mut DataInput::new(file), &name, 1, 1, id, "")?;
    }
    codec_util::check_index_header(m, "Lucene90PointsFormatMeta", 1, 1, id, "")?;
    loop {
        let number = m.read_int()?;
        if number == -1 {
            return err(format!("{} has no points for {field}", r.seg.name));
        }
        codec_util::check_header(m, "BKD", 10, 10)?;
        m.read_vint()?; // numDims
        let dims = m.read_len()?;
        let max_leaf = m.read_len()?;
        let width = m.read_len()?;
        let leaves = m.read_len()?;
        let lo = m.read_bytes(dims * width)?;
        let hi = m.read_bytes(dims * width)?;
        m.read_vlong()?; // point count
        m.read_vint()?; // doc count
        let len = m.read_len()?;
        m.read_long()?; // first leaf
        let start = usize::try_from(m.read_long()?).or_else(|_| err("negative index start"))?;
        if number != info.number {
            continue;
        }
        if !(1..=8).contains(&width) {
            return err(format!("{field} has {width}-byte points"));
        }
        let Some(index) = index.get(start..start.saturating_add(len)) else {
            return err(format!("points index of {field} extends past its file"));
        };
        let tree = Tree {
            index,
            data: &data,
            leaves,
            max_leaf,
            max_doc: r.seg.info.max_doc.max(0) as u32,
            width,
            place,
        };
        let mut out = Vec::new();
        tree.node(1, 0, 0, &vec![0; width], (lo, hi), &mut out)?;
        return Ok(out);
    }
}

/// A one-dimensional BKD tree: `BKDReader`'s packed index (`.kdi`) over leaves in `.kdd`.
struct Tree<'a, F> {
    index: &'a [u8],
    data: &'a [u8],
    leaves: usize,
    max_leaf: usize,
    max_doc: u32,
    width: usize,
    place: F,
}

impl<F: Fn(i64) -> Ordering> Tree<'_, F> {
    /// Visits node `id` (the root is 1, children of `n` are `2n` and `2n + 1`, and ids at and
    /// past `leaves` are leaves), whose packed data starts at `pos`, below a node whose leftmost
    /// leaf is at `fp`. `split` is the nearest ancestor's split value; `lo..=hi` bound the
    /// node's values.
    fn node(
        &self,
        id: usize,
        pos: usize,
        fp: u64,
        split: &[u8],
        (lo, hi): (&[u8], &[u8]),
        out: &mut Vec<u32>,
    ) -> Result<()> {
        let is_left = id.is_multiple_of(2);
        let (lo_at, hi_at) = ((self.place)(decode(lo)), (self.place)(decode(hi)));
        if hi_at == Ordering::Less || lo_at == Ordering::Greater {
            return Ok(());
        }
        let r = &mut DataInput::new(self.index);
        r.pos = pos;
        // a left child starts at its parent's leftmost leaf; others store the distance
        let fp = if is_left {
            fp
        } else {
            fp.wrapping_add(r.read_vlong()? as u64)
        };
        if id >= self.leaves {
            return self.leaf(
                fp,
                lo_at == Ordering::Equal && hi_at == Ordering::Equal,
                out,
            );
        }
        let w = self.width;
        let code = usize::try_from(r.read_vint()?).or_else(|_| err("negative split code"))?;
        let (prefix, delta) = (code % (w + 1), (code / (w + 1)) as u8);
        let mut split = split.to_vec();
        if prefix < w {
            // the first differing byte is stored as its distance from the ancestor's, downward
            // in left children
            split[prefix] = if is_left {
                split[prefix].wrapping_sub(delta)
            } else {
                split[prefix].wrapping_add(delta)
            };
            split[prefix + 1..].copy_from_slice(r.read_bytes(w - prefix - 1)?);
        }
        let left_len = if id * 2 < self.leaves {
            r.read_len()?
        } else {
            0
        };
        let left = r.pos;
        self.node(id * 2, left, fp, &split, (lo, &split), out)?;
        let right = left.saturating_add(left_len);
        self.node(id * 2 + 1, right, fp, &split, (&split, hi), out)
    }

    /// Adds a leaf's docs whose value is in range, or all of them with `all`.
    fn leaf(&self, fp: u64, all: bool, out: &mut Vec<u32>) -> Result<()> {
        let r = &mut DataInput::new(self.data);
        r.pos = usize::try_from(fp).or_else(|_| err("bad leaf pointer"))?;
        let count = r.read_len()?;
        if count > self.max_leaf {
            return err(format!("points leaf of {count} exceeds {}", self.max_leaf));
        }
        let docs = read_doc_ids(r, count)?;
        if docs.iter().any(|d| *d >= self.max_doc) {
            return err("points doc id past the segment");
        }
        if all {
            out.extend(docs);
            return Ok(());
        }
        let w = self.width;
        let prefix = r.read_len()?;
        let mut value = r.read_bytes(prefix)?.to_vec();
        value.resize(w, 0);
        let keep = |v: &[u8]| (self.place)(decode(v)) == Ordering::Equal;
        let mut i = 0;
        match r.read_byte()? {
            // all equal
            0xff if prefix == w => {
                if keep(&value) {
                    out.extend(docs);
                }
                i = count;
            }
            // low cardinality: (run length, suffix) per distinct value
            0xfe if prefix < w => {
                while i < count {
                    let run = r.read_len()?;
                    value[prefix..].copy_from_slice(r.read_bytes(w - prefix)?);
                    let Some(run_docs) = docs.get(i..i.saturating_add(run)) else {
                        break;
                    };
                    if keep(&value) {
                        out.extend(run_docs);
                    }
                    i += run;
                }
            }
            // high cardinality: runs sharing the first byte after the prefix
            0 if prefix < w => {
                while i < count {
                    value[prefix] = r.read_byte()?;
                    let run = usize::from(r.read_byte()?);
                    for doc in docs.iter().skip(i).take(run) {
                        value[prefix + 1..].copy_from_slice(r.read_bytes(w - prefix - 1)?);
                        if keep(&value) {
                            out.push(*doc);
                        }
                    }
                    i += run;
                }
            }
            b => {
                return err(format!(
                    "bad points leaf encoding {b} after a {prefix}-byte prefix"
                ));
            }
        }
        if i != count {
            return err("points leaf runs do not add up to its count");
        }
        Ok(())
    }
}

/// `DocIdsWriter.readInts`: `count` doc ids of a leaf.
fn read_doc_ids(r: &mut DataInput, count: usize) -> Result<Vec<u32>> {
    let mut ids = vec![0u32; count];
    // 2 bytes, then the third byte above them
    let short_byte = |r: &mut DataInput| -> Result<u32> {
        Ok(u32::from(r.read_short()? as u16) | u32::from(r.read_byte()?) << 16)
    };
    match r.read_byte()? {
        // contiguous
        0xfe => {
            let start = r.read_vint()? as u32;
            (ids.iter_mut().zip(0..)).for_each(|(id, i)| *id = start.wrapping_add(i));
        }
        // bitset
        0xff => {
            let base = (r.read_vint()? as u32).wrapping_mul(64);
            let mut n = 0;
            for word in 0..r.read_len()? as u32 {
                let mut bits = r.read_long()? as u64;
                while bits != 0 {
                    let Some(id) = ids.get_mut(n) else {
                        return err("points bitset has more docs than its leaf");
                    };
                    *id = base.wrapping_add(word * 64 + bits.trailing_zeros());
                    bits &= bits - 1;
                    n += 1;
                }
            }
            if n != count {
                return err("points bitset has fewer docs than its leaf");
            }
        }
        // deltas from the minimum, two 16-bit halves per int
        16 => {
            let min = r.read_vint()? as u32;
            let half = count / 2;
            for i in 0..half {
                let l = r.read_int()? as u32;
                ids[i] = (l >> 16).wrapping_add(min);
                ids[i + half] = (l & 0xffff).wrapping_add(min);
            }
            if count % 2 == 1 {
                ids[count - 1] = u32::from(r.read_short()? as u16).wrapping_add(min);
            }
        }
        // 21 bits: the top of two ints per id, a third id in their low bits, then three per long
        21 => {
            let third = (count / 3) & !15;
            let ints = (0..third * 2)
                .map(|_| Ok(r.read_int()? as u32))
                .collect::<Result<Vec<_>>>()?;
            for i in 0..third * 2 {
                ids[i] = ints[i] >> 11;
            }
            for i in 0..third {
                ids[i + third * 2] = (ints[i] & 0x7ff) | (ints[i + third] & 0x7ff) << 11;
            }
            let mut i = third * 3;
            while i + 2 < count {
                let l = r.read_long()? as u64;
                for (k, id) in ids[i..i + 3].iter_mut().enumerate() {
                    *id = (l >> (21 * k) & 0x1f_ffff) as u32;
                }
                i += 3;
            }
            for id in &mut ids[i..] {
                *id = short_byte(r)?;
            }
        }
        // 24 bits: the top of three ints per id, a fourth id in their low bytes
        24 => {
            let quarter = count / 4;
            let ints = (0..quarter * 3)
                .map(|_| Ok(r.read_int()? as u32))
                .collect::<Result<Vec<_>>>()?;
            for i in 0..quarter * 3 {
                ids[i] = ints[i] >> 8;
            }
            for i in 0..quarter {
                ids[i + quarter * 3] = (ints[i] & 0xff)
                    | (ints[i + quarter] & 0xff) << 8
                    | (ints[i + quarter * 2] & 0xff) << 16;
            }
            for id in &mut ids[quarter * 4..] {
                *id = short_byte(r)?;
            }
        }
        32 => {
            for id in &mut ids {
                *id = r.read_int()? as u32;
            }
        }
        // legacy vint deltas
        0 => {
            let mut doc = 0u32;
            for id in &mut ids {
                doc = doc.wrapping_add(r.read_vint()? as u32);
                *id = doc;
            }
        }
        b => return err(format!("bad points doc id encoding {b}")),
    }
    Ok(ids)
}

/// Sortable big-endian bytes for `v` in a `width`-byte field (`LongPoint` is 8, `IntPoint` is 4,
/// `HalfFloatPoint` is 2).
pub fn sortable_bytes(v: i64, width: usize) -> Vec<u8> {
    let be = ((v as u64) ^ (1 << 63)).to_be_bytes();
    if width == 8 {
        be.to_vec()
    } else if width == 2 {
        (((v as i16) as u16) ^ (1 << 15)).to_be_bytes().to_vec()
    } else {
        (((v as i32) as u32) ^ (1 << 31)).to_be_bytes().to_vec()
    }
}

/// `(doc, sortable value bytes)` pairs of one field, in doc order.
pub type PointValues = Vec<(u32, Vec<u8>)>;

/// Points per leaf (`BKDConfig.DEFAULT_MAX_POINTS_IN_LEAF_NODE`).
const LEAF: usize = 512;

/// `(meta, index, data)` files, as Lucene's `OneDimensionBKDWriter` writes them. `fields` is
/// `(field number, (doc, value) pairs)`; every value in a field has the same width, and a doc
/// may have none or several, but a field has at least one.
pub fn write_points(
    fields: &[(i32, PointValues)],
    id: &[u8; ID_LEN],
) -> crate::Result<(Vec<u8>, Vec<u8>, Vec<u8>)> {
    let (mut meta, mut index, mut data) = (Output::default(), Output::default(), Output::default());
    meta.index_header("Lucene90PointsFormatMeta", 1, id, "");
    index.index_header("Lucene90PointsFormatIndex", 1, id, "");
    data.index_header("Lucene90PointsFormatData", 1, id, "");
    for (number, pairs) in fields {
        let Some(width) = pairs.first().map(|p| p.1.len()) else {
            return crate::err(format!("points field {number} has no values"));
        };
        if pairs.iter().any(|p| p.1.len() != width) {
            return crate::err(format!("points field {number} has values of mixed widths"));
        }
        // by value, then doc; pairs are in doc order
        let mut sorted: Vec<(&[u8], u32)> = pairs.iter().map(|p| (p.1.as_slice(), p.0)).collect();
        sorted.sort();
        let data_start = data.0.len() as u64;
        let mut fps = Vec::new();
        for leaf in sorted.chunks(LEAF) {
            fps.push(data.0.len() as u64);
            write_leaf(&mut data, leaf, width);
        }
        let splits: Vec<&[u8]> = sorted.chunks(LEAF).skip(1).map(|l| l[0].0).collect();
        let packed = pack_index(&fps, &splits, 0, &mut vec![0; width], false, 0, fps.len());
        meta.bytes(&number.to_le_bytes());
        meta.header("BKD", 10);
        meta.vint(1);
        meta.vint(1);
        meta.vint(LEAF as i32);
        meta.vint(width as i32);
        meta.vint(fps.len() as i32);
        meta.bytes(sorted[0].0);
        meta.bytes(sorted[sorted.len() - 1].0);
        meta.vlong(sorted.len() as i64);
        let mut docs: Vec<u32> = pairs.iter().map(|p| p.0).collect();
        docs.dedup(); // pairs are in doc order
        meta.vint(docs.len() as i32);
        meta.vint(packed.len() as i32);
        meta.long(data_start as i64);
        meta.long(index.0.len() as i64);
        index.bytes(&packed);
    }
    let (index, data) = (index.finish(), data.finish());
    meta.bytes(&(-1i32).to_le_bytes());
    meta.long(index.len() as i64);
    meta.long(data.len() as i64);
    Ok((meta.finish(), index, data))
}

/// `OneDimensionBKDWriter.writeLeafBlock`: doc ids, the common prefix, then the values all
/// equal, by distinct value (low cardinality), or by runs of their first differing byte,
/// whichever is smaller.
fn write_leaf(out: &mut Output, leaf: &[(&[u8], u32)], w: usize) {
    out.vint(leaf.len() as i32);
    write_doc_ids(out, &leaf.iter().map(|p| p.1).collect::<Vec<_>>());
    let (first, last) = (leaf[0].0, leaf[leaf.len() - 1].0);
    let prefix = first.iter().zip(last).take_while(|(a, b)| a == b).count();
    out.vint(prefix as i32);
    out.bytes(&first[..prefix]);
    if prefix == w {
        out.byte(0xff);
        return;
    }
    let distinct = 1 + leaf.windows(2).filter(|p| p[0].0 != p[1].0).count();
    // runs of at most 255 values sharing the byte after the prefix
    let runs = |out: Option<&mut Output>| {
        let (mut i, mut n, mut out) = (0, 0, out);
        while i < leaf.len() {
            let b = leaf[i].0[prefix];
            let run = (leaf[i..].iter().take(255))
                .take_while(|p| p.0[prefix] == b)
                .count();
            if let Some(out) = out.as_deref_mut() {
                out.byte(b);
                out.byte(run as u8);
                leaf[i..i + run]
                    .iter()
                    .for_each(|p| out.bytes(&p.0[prefix + 1..]));
            }
            (i, n) = (i + run, n + 1);
        }
        n
    };
    let (high, low) = if distinct == leaf.len() {
        (0, 1)
    } else {
        (
            leaf.len() * (w - prefix - 1) + 2 * runs(None),
            distinct * (w - prefix + 1),
        )
    };
    if low <= high {
        out.byte(0xfe);
        for group in leaf.chunk_by(|a, b| a.0 == b.0) {
            out.vint(group.len() as i32);
            out.bytes(&group[0].0[prefix..]);
        }
    } else {
        out.byte(0);
        runs(Some(out));
    }
}

/// `DocIdsWriter.writeDocIds` (BKD version 10): the most compact of its encodings.
fn write_doc_ids(out: &mut Output, ids: &[u32]) {
    let (min, max) = (ids.iter().min(), ids.iter().max());
    let (min, max) = (*min.unwrap_or(&0), *max.unwrap_or(&0));
    let (n, span) = (ids.len(), (max - min + 1) as usize);
    let short = |out: &mut Output, v: u32| out.bytes(&(v as u16).to_le_bytes());
    if ids.windows(2).all(|w| w[0] < w[1]) {
        if span == n {
            out.byte(0xfe);
            out.vint(min as i32);
            return;
        }
        if span <= n << 4 {
            out.byte(0xff);
            let base = min / 64;
            let mut words = vec![0u64; (max / 64 - base + 1) as usize];
            for id in ids {
                words[(id / 64 - base) as usize] |= 1 << (id % 64);
            }
            out.vint(base as i32);
            out.vint(words.len() as i32);
            words.iter().for_each(|w| out.long(*w as i64));
            return;
        }
    }
    if span <= 0xffff {
        out.byte(16);
        out.vint(min as i32);
        let half = n / 2;
        for i in 0..half {
            out.int(((ids[i] - min) << 16 | (ids[half + i] - min)) as i32);
        }
        if n % 2 == 1 {
            short(out, ids[n - 1] - min);
        }
    } else if max <= 0x1f_ffff {
        out.byte(21);
        let third = (n / 3) & !15;
        let mut ints: Vec<u32> = ids[..third * 2].iter().map(|id| id << 11).collect();
        for i in 0..third {
            let id = ids[third * 2 + i];
            ints[i] |= id & 0x7ff;
            ints[i + third] |= (id >> 11) & 0x7ff;
        }
        ints.iter().for_each(|v| out.int(*v as i32));
        let mut i = third * 3;
        while i + 2 < n {
            let l = (0..3).fold(0u64, |l, k| l | u64::from(ids[i + k]) << (21 * k));
            out.long(l as i64);
            i += 3;
        }
        for id in &ids[i..] {
            short(out, *id);
            out.byte((id >> 16) as u8);
        }
    } else if max <= 0xff_ffff {
        out.byte(24);
        let quarter = n / 4;
        let mut ints: Vec<u32> = ids[..quarter * 3].iter().map(|id| id << 8).collect();
        for i in 0..quarter {
            let id = ids[quarter * 3 + i];
            ints[i] |= id & 0xff;
            ints[i + quarter] |= (id >> 8) & 0xff;
            ints[i + quarter * 2] |= id >> 16;
        }
        ints.iter().for_each(|v| out.int(*v as i32));
        for id in &ids[quarter * 4..] {
            short(out, *id);
            out.byte((id >> 16) as u8);
        }
    } else {
        out.byte(32);
        ids.iter().for_each(|id| out.int(*id as i32));
    }
}

/// How many of `n > 1` leaves go left (`BKDWriter.getNumLeftLeafNodes`): fill the last full
/// level, putting the rest on the left.
fn left_leaves(n: usize) -> usize {
    let full = 1 << n.ilog2();
    full / 2 + (n - full).min(full / 2)
}

/// `BKDWriter.recursePackIndex` for one dimension: the packed index of `n` leaves from `off`,
/// whose leaf file pointers are `fps` and split values `splits` (the first value of each leaf
/// but the first). `last` is the nearest ancestor's split value, `min_fp` the leftmost leaf of
/// the parent; a left child (`negative`) stores neither its pointer nor a positive delta.
fn pack_index(
    fps: &[u64],
    splits: &[&[u8]],
    min_fp: u64,
    last: &mut [u8],
    negative: bool,
    off: usize,
    n: usize,
) -> Vec<u8> {
    let mut out = Output::default();
    if !negative {
        out.vlong((fps[off] - min_fp) as i64);
    }
    if n == 1 {
        return out.0;
    }
    let left = left_leaves(n);
    let split = splits[off + left - 1];
    let w = split.len();
    let prefix = split
        .iter()
        .zip(last.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let delta = if prefix < w {
        (i32::from(split[prefix]) - i32::from(last[prefix])).abs()
    } else {
        0
    };
    out.vint(delta * (w as i32 + 1) + prefix as i32);
    if prefix + 1 < w {
        out.bytes(&split[prefix + 1..]);
    }
    let saved = last.to_vec();
    last[prefix..].copy_from_slice(&split[prefix..]);
    let left_bytes = pack_index(fps, splits, fps[off], last, true, off, left);
    if left > 1 {
        out.vint(left_bytes.len() as i32);
    }
    out.bytes(&left_bytes);
    let right = pack_index(fps, splits, fps[off], last, false, off + left, n - left);
    out.bytes(&right);
    last.copy_from_slice(&saved);
    out.0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every doc id encoding, including the 24- and 32-bit ones the Lucene-written test data
    /// does not reach, reads back what was written.
    #[test]
    fn doc_ids_round_trip() {
        let sets: [Vec<u32>; 7] = [
            (5..517).collect(),                               // contiguous
            (0..300).map(|i| i * 3).collect(),                // bitset
            (0..301).rev().map(|i| i * 200).collect(),        // 16-bit deltas
            (0..511).map(|i| i * 4001 % 2_000_000).collect(), // 21 bits
            (0..203).map(|i| i * 80_021).collect(),           // 24 bits
            vec![1 << 30, 7, 1 << 25],                        // 32 bits
            vec![9],
        ];
        for (ids, kind) in sets.iter().zip([0xfe, 0xff, 16, 21, 24, 32, 0xfe]) {
            let mut out = Output::default();
            write_doc_ids(&mut out, ids);
            assert_eq!(out.0[0], kind);
            let r = &mut DataInput::new(&out.0);
            assert_eq!(&read_doc_ids(r, ids.len()).unwrap(), ids);
            assert_eq!(r.remaining(), 0);
        }
    }

    #[test]
    fn sortable_bytes_order_like_numbers() {
        assert_eq!(sortable_bytes(4, 8), [0x80, 0, 0, 0, 0, 0, 0, 4]);
        assert_eq!(sortable_bytes(-1, 4), [0x7f, 0xff, 0xff, 0xff]);
        assert_eq!(sortable_bytes(-1, 2), [0x7f, 0xff]);
        assert!(sortable_bytes(-5, 8) < sortable_bytes(3, 8));
        for (v, w) in [
            (i64::MIN, 8),
            (-3, 8),
            (i64::from(i32::MIN), 4),
            (70_000, 4),
            (-2, 2),
        ] {
            assert_eq!(decode(&sortable_bytes(v, w)), v);
        }
    }

    /// Elasticsearch's own `_seq_no` points, rebuilt from its doc values, must match byte for byte.
    #[test]
    fn seq_no_points_match_the_es_fixture() {
        use crate::{SegmentInfos, SegmentReader, doc_values::DocValues};
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../fixtures/es-9.5.5/data/indices");
        let mut checked = 0;
        for e in std::fs::read_dir(dir).unwrap() {
            let idx = e.unwrap().path().join("0/index");
            let si = SegmentInfos::read_latest(&idx).unwrap();
            for seg in &si.segments {
                let r = SegmentReader::open(&idx, seg.clone()).unwrap();
                let Ok(f) = r.field("_seq_no") else { continue };
                if f.point_dimensions == 0 || seg.del_count > 0 {
                    continue;
                }
                let Ok(DocValues::Numbers(nums)) = r.doc_values("_seq_no") else {
                    continue;
                };
                let pairs: PointValues = nums
                    .iter()
                    .enumerate()
                    .flat_map(|(d, v)| v.iter().map(move |n| (d as u32, sortable_bytes(*n, 8))))
                    .collect();
                let (m, i, d) = write_points(&[(f.number, pairs)], &seg.info.id).unwrap();
                let name = |ext: &str| {
                    r.cfs
                        .names()
                        .find(|n| n.ends_with(ext))
                        .unwrap()
                        .to_string()
                };
                // Real segments also hold other point fields after `_seq_no`; compare its part.
                let (m, i, d) = (&m[..m.len() - 36], &i[..i.len() - 16], &d[..d.len() - 16]);
                for (ext, ours) in [(".kdm", m), (".kdi", i), (".kdd", d)] {
                    let real = r.cfs.file(&name(ext)).unwrap().unwrap();
                    assert_eq!(&real[..ours.len()], ours, "{ext}");
                }
                checked += 1;
            }
        }
        assert!(checked > 0);
    }
}
