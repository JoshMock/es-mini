//! Lucene 10 BKD points (`.kdm`/`.kdi`/`.kdd`): a reader of one-dimensional trees that narrows a
//! range by walking them, and a writer of one- and two-dimensional trees (`BKDWriter`).

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
    let (m, index, data) = field_meta(r, field)?;
    let width = m.width;
    if !(1..=8).contains(&width) {
        return err(format!("{field} has {width}-byte points"));
    }
    let Some(index) = index.get(m.index.clone()) else {
        return err(format!("points index of {field} extends past its file"));
    };
    let tree = Tree {
        index,
        data: &data,
        leaves: m.leaves,
        max_leaf: m.max_leaf,
        max_doc: r.seg.info.max_doc.max(0) as u32,
        width,
        place,
    };
    let mut out = Vec::new();
    tree.node(1, 0, 0, &vec![0; width], (&m.lo, &m.hi), &mut out)?;
    Ok(out)
}

/// One field's entry in `.kdm`.
struct FieldMeta {
    dims: usize,
    max_leaf: usize,
    width: usize,
    leaves: usize,
    lo: Vec<u8>,
    hi: Vec<u8>,
    /// `.kdi` range of the packed index
    index: std::ops::Range<usize>,
    /// `.kdd` offset of the first leaf; the leaves follow it in order
    first_leaf: usize,
}

/// `field`'s meta entry, and the `.kdi` and `.kdd` files, checked.
fn field_meta(r: &SegmentReader, field: &str) -> Result<(FieldMeta, Vec<u8>, Vec<u8>)> {
    let info = r.field(field)?;
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
        let lo = m.read_bytes(dims * width)?.to_vec();
        let hi = m.read_bytes(dims * width)?.to_vec();
        m.read_vlong()?; // point count
        m.read_vint()?; // doc count
        let len = m.read_len()?;
        let first_leaf = usize::try_from(m.read_long()?).or_else(|_| err("negative first leaf"))?;
        let start = usize::try_from(m.read_long()?).or_else(|_| err("negative index start"))?;
        if number == info.number {
            let meta = FieldMeta {
                dims,
                max_leaf,
                width,
                leaves,
                lo,
                hi,
                index: start..start.saturating_add(len),
                first_leaf,
            };
            return Ok((meta, index, data));
        }
    }
}

/// [`SegmentReader::points`]: every leaf of `field`'s tree decoded in turn.
pub(crate) fn read_points(r: &SegmentReader, field: &str) -> Result<PointValues> {
    let (m, _, data) = field_meta(r, field)?;
    let max_doc = r.seg.info.max_doc.max(0) as u32;
    let d = &mut DataInput::new(&data);
    d.pos = m.first_leaf;
    let mut out = vec![];
    for _ in 0..m.leaves {
        let docs = leaf_docs(d, m.max_leaf, max_doc)?;
        leaf_values(d, docs.len(), m.dims, m.width, |i, v| {
            out.push((docs[i], v.to_vec()))
        })?;
    }
    out.sort();
    Ok(out)
}

/// The doc ids that start the leaf at `d`, checked against the tree's `max_leaf` and the
/// segment's `max_doc`.
fn leaf_docs(d: &mut DataInput, max_leaf: usize, max_doc: u32) -> Result<Vec<u32>> {
    let count = d.read_len()?;
    if count > max_leaf {
        return err(format!("points leaf of {count} exceeds {max_leaf}"));
    }
    let docs = read_doc_ids(d, count)?;
    if docs.iter().any(|&doc| doc >= max_doc) {
        return err("points doc id past the segment");
    }
    Ok(docs)
}

/// Decodes the values of the leaf whose `count` doc ids were just read from `d`, as `BKDReader`'s
/// `visitDocValues` does, calling `visit` with each one's index in the leaf and its bytes.
fn leaf_values(
    d: &mut DataInput,
    count: usize,
    dims: usize,
    w: usize,
    mut visit: impl FnMut(usize, &[u8]),
) -> Result<()> {
    let mut value = vec![0; dims * w];
    let mut prefix = vec![0; dims];
    for (k, p) in prefix.iter_mut().enumerate() {
        *p = d.read_len()?;
        if *p > w {
            return err(format!("points prefix of {p} bytes in {w}-byte values"));
        }
        value[k * w..k * w + *p].copy_from_slice(d.read_bytes(*p)?);
    }
    // each dimension's bytes after its prefix (and one more in dimension `sorted`)
    let suffixes = |d: &mut DataInput, value: &mut Vec<u8>, sorted: Option<usize>| {
        for k in 0..dims {
            let skip = prefix[k] + usize::from(sorted == Some(k));
            value[k * w + skip..(k + 1) * w].copy_from_slice(d.read_bytes(w - skip)?);
        }
        Result::Ok(())
    };
    let mut n = 0;
    match d.read_byte()? {
        // all equal
        0xff if prefix.iter().all(|&p| p == w) => {
            (0..count).for_each(|i| visit(i, &value));
            n = count;
        }
        0xff => return err("points leaf of equal values without a full prefix"),
        b => {
            // the leaf's own bounds, which decoding its values does not need
            if dims > 1 {
                for &p in prefix.iter().filter(|&&p| p < w) {
                    d.read_bytes(2 * (w - p))?;
                }
            }
            if b == 0xfe {
                // low cardinality: (run length, suffixes) per distinct value
                while n < count {
                    let run = d.read_len()?;
                    suffixes(d, &mut value, None)?;
                    (n..n.saturating_add(run).min(count)).for_each(|i| visit(i, &value));
                    n = n.saturating_add(run);
                }
            } else {
                // runs sharing the byte after the sorted dimension's prefix
                let s = usize::from(b);
                if s >= dims || prefix[s] == w {
                    return err(format!("bad points leaf encoding {b}"));
                }
                while n < count {
                    value[s * w + prefix[s]] = d.read_byte()?;
                    for _ in 0..d.read_byte()? {
                        suffixes(d, &mut value, Some(s))?;
                        if n < count {
                            visit(n, &value);
                        }
                        n += 1;
                    }
                }
            }
        }
    }
    if n != count {
        return err("points leaf runs do not add up to its count");
    }
    Ok(())
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
        let docs = leaf_docs(r, self.max_leaf, self.max_doc)?;
        if all {
            out.extend(docs);
            return Ok(());
        }
        leaf_values(r, docs.len(), 1, self.width, |i, v| {
            if (self.place)(decode(v)) == Ordering::Equal {
                out.push(docs[i]);
            }
        })
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

/// `(meta, index, data)` files, as Lucene's `BKDWriter` flushes them. `fields` is
/// `(field number, dimensions, (doc, value) pairs)`: a value is its dimensions' sortable bytes
/// side by side, all values of a field have the same width, and a doc may have none or several,
/// but a field has at least one. One or two dimensions, all indexed.
pub fn write_points(
    fields: &[(i32, usize, PointValues)],
    id: &[u8; ID_LEN],
) -> crate::Result<(Vec<u8>, Vec<u8>, Vec<u8>)> {
    let (mut meta, mut index, mut data) = (Output::default(), Output::default(), Output::default());
    meta.index_header("Lucene90PointsFormatMeta", 1, id, "");
    index.index_header("Lucene90PointsFormatIndex", 1, id, "");
    data.index_header("Lucene90PointsFormatData", 1, id, "");
    for (number, dims, pairs) in fields {
        let Some(packed) = pairs.first().map(|p| p.1.len()) else {
            return crate::err(format!("points field {number} has no values"));
        };
        // ponytail: more dimensions recompute their bounds every few splits; port that with them
        if !(1..=2).contains(dims) || packed % dims != 0 {
            return crate::err(format!("points field {number} has {dims} dimensions"));
        }
        if pairs.iter().any(|p| p.1.len() != packed) {
            return crate::err(format!("points field {number} has values of mixed widths"));
        }
        let (dims, w) = (*dims, packed / dims);
        let mut points: Vec<(&[u8], u32)> = pairs.iter().map(|p| (p.1.as_slice(), p.0)).collect();
        let (mut lo, mut hi) = (points[0].0.to_vec(), points[0].0.to_vec());
        for (v, _) in &points {
            for r in (0..dims).map(|d| d * w..(d + 1) * w) {
                if v[r.clone()] < lo[r.clone()] {
                    lo[r.clone()].copy_from_slice(&v[r.clone()]);
                }
                if v[r.clone()] > hi[r.clone()] {
                    hi[r.clone()].copy_from_slice(&v[r]);
                }
            }
        }
        let leaves = points.len().div_ceil(LEAF);
        let mut tree = Bkd {
            dims,
            w,
            fps: vec![0; leaves],
            splits: vec![(0, &[][..]); leaves - 1],
            parent_splits: vec![0; dims],
        };
        let data_start = data.0.len() as u64;
        tree.build(&mut data, &mut points, 0, leaves, &lo, &hi);
        let packed_index = pack_index(&tree, 0, &mut vec![0; packed], false, 0, leaves);
        meta.bytes(&number.to_le_bytes());
        meta.header("BKD", 10);
        meta.vint(dims as i32);
        meta.vint(dims as i32);
        meta.vint(LEAF as i32);
        meta.vint(w as i32);
        meta.vint(leaves as i32);
        meta.bytes(&lo);
        meta.bytes(&hi);
        meta.vlong(pairs.len() as i64);
        let mut docs: Vec<u32> = pairs.iter().map(|p| p.0).collect();
        docs.dedup(); // pairs are in doc order
        meta.vint(docs.len() as i32);
        meta.vint(packed_index.len() as i32);
        meta.long(data_start as i64);
        meta.long(index.0.len() as i64);
        index.bytes(&packed_index);
    }
    let (index, data) = (index.finish(), data.finish());
    meta.bytes(&(-1i32).to_le_bytes());
    meta.long(index.len() as i64);
    meta.long(data.len() as i64);
    Ok((meta.finish(), index, data))
}

/// One field's tree as `BKDWriter.writeFieldNDims` builds it: leaf file pointers, and the split
/// dimension and value of each inner node, by the leaf it ends after.
struct Bkd<'a> {
    dims: usize,
    w: usize,
    fps: Vec<u64>,
    splits: Vec<(usize, &'a [u8])>,
    /// How often each dimension split the ancestors of the node being built
    parent_splits: Vec<usize>,
}

impl<'a> Bkd<'a> {
    /// `BKDWriter.build`: writes `n` leaves from leaf `off` of `pts`, whose values lie in
    /// `lo..=hi`. An inner node splits the dimension `split` picks, leaving `left_leaves(n)` full
    /// leaves of the smallest values (then lowest docs) on the left, so one dimension makes
    /// consecutive leaves of the sorted values.
    // ponytail: Lucene's selector and sorter order points equal in the split (or leaf sort)
    // dimension and doc arbitrarily; here the other dimension breaks the tie, so such points can
    // sit on another side of a split or in another order than in Lucene (the tree reads the same)
    fn build(
        &mut self,
        data: &mut Output,
        pts: &mut [(&'a [u8], u32)],
        off: usize,
        n: usize,
        lo: &[u8],
        hi: &[u8],
    ) {
        if n == 1 {
            self.fps[off] = data.0.len() as u64;
            write_leaf(data, pts, self.dims, self.w);
            return;
        }
        let dim = self.split(lo, hi);
        let r = dim * self.w..(dim + 1) * self.w;
        let left = left_leaves(n);
        let mid = left * LEAF;
        pts.select_nth_unstable_by(mid, |a, b| by_dim(a, b, &r));
        let value: &'a [u8] = pts[mid].0;
        let split = &value[r.clone()];
        self.splits[off + left - 1] = (dim, split);
        let (mut left_hi, mut right_lo) = (hi.to_vec(), lo.to_vec());
        left_hi[r.clone()].copy_from_slice(split);
        right_lo[r].copy_from_slice(split);
        self.parent_splits[dim] += 1;
        let (l, rt) = pts.split_at_mut(mid);
        self.build(data, l, off, left, lo, &left_hi);
        self.build(data, rt, off + left, n - left, &right_lo, hi);
        self.parent_splits[dim] -= 1;
    }

    /// `BKDWriter.split`: a dimension split less than half as often as another (unless all its
    /// values are equal), else the one whose values span the most.
    fn split(&self, lo: &[u8], hi: &[u8]) -> usize {
        let w = self.w;
        let most = self.parent_splits.iter().max().copied().unwrap_or(0);
        let dim = |d: usize| d * w..(d + 1) * w;
        if let Some(d) =
            (0..self.dims).find(|&d| self.parent_splits[d] < most / 2 && lo[dim(d)] != hi[dim(d)])
        {
            return d;
        }
        // big-endian `hi - lo`
        let span = |d: usize| {
            let (mut out, mut borrow) = (vec![0u8; w], 0i16);
            for i in (0..w).rev() {
                let x = i16::from(hi[d * w + i]) - i16::from(lo[d * w + i]) - borrow;
                borrow = i16::from(x < 0);
                out[i] = x.rem_euclid(256) as u8;
            }
            out
        };
        (1..self.dims).fold(0, |best, d| if span(d) > span(best) { d } else { best })
    }
}

/// Orders points by the bytes of dimension `r`, then doc, then the whole value.
fn by_dim(a: &(&[u8], u32), b: &(&[u8], u32), r: &std::ops::Range<usize>) -> Ordering {
    (&a.0[r.clone()], a.1, a.0).cmp(&(&b.0[r.clone()], b.1, b.0))
}

/// A leaf as `BKDWriter.build` (and `OneDimensionBKDWriter.writeLeafBlock`) writes it: doc ids,
/// each dimension's common prefix, then the values all equal, by distinct value (low
/// cardinality), or by runs of their first differing byte in the sorted dimension, whichever is
/// smaller. Points sort by the dimension whose first byte after its prefix takes the fewest
/// values.
fn write_leaf(out: &mut Output, leaf: &mut [(&[u8], u32)], dims: usize, w: usize) {
    let dim = |d: usize| d * w..(d + 1) * w;
    let first = leaf[0].0;
    let prefix: Vec<usize> = (0..dims)
        .map(|d| {
            (leaf[1..].iter())
                .map(|p| (first[dim(d)].iter().zip(&p.0[dim(d)])).take_while(|(a, b)| a == b))
                .map(Iterator::count)
                .fold(w, usize::min)
        })
        .collect();
    let mut sorted = (0, usize::MAX);
    for d in (0..dims).filter(|&d| prefix[d] < w) {
        // like Lucene, the first value's byte is not counted
        let mut used = [false; 256];
        leaf[1..]
            .iter()
            .for_each(|p| used[p.0[d * w + prefix[d]] as usize] = true);
        let n = used.iter().filter(|u| **u).count();
        if n < sorted.1 {
            sorted = (d, n);
        }
    }
    let sorted = sorted.0;
    leaf.sort_by(|a, b| by_dim(a, b, &dim(sorted)));
    out.vint(leaf.len() as i32);
    write_doc_ids(out, &leaf.iter().map(|p| p.1).collect::<Vec<_>>());
    for (d, &p) in prefix.iter().enumerate() {
        out.vint(p as i32);
        out.bytes(&leaf[0].0[d * w..d * w + p]);
    }
    let packed = dims * w;
    let prefix_sum: usize = prefix.iter().sum();
    if prefix_sum == packed {
        out.byte(0xff);
        return;
    }
    // each dimension's bytes after `prefix` (plus `extra` in the sorted dimension)
    let suffixes = |out: &mut Output, v: &[u8], extra: usize| {
        for d in 0..dims {
            let skip = prefix[d] + if d == sorted { extra } else { 0 };
            out.bytes(&v[d * w + skip..(d + 1) * w]);
        }
    };
    // the leaf's own bounds of each dimension, after the prefix
    let bounds = |out: &mut Output| {
        if dims == 1 {
            return;
        }
        for d in (0..dims).filter(|&d| prefix[d] < w) {
            let r = d * w + prefix[d]..(d + 1) * w;
            let values = leaf.iter().map(|p| &p.0[r.clone()]);
            out.bytes(values.clone().min().unwrap_or_default());
            out.bytes(values.max().unwrap_or_default());
        }
    };
    let distinct = 1 + leaf.windows(2).filter(|p| p[0].0 != p[1].0).count();
    // runs of at most 255 values sharing the byte after the sorted dimension's prefix
    let at = sorted * w + prefix[sorted];
    let runs = |out: Option<&mut Output>| {
        let (mut i, mut n, mut out) = (0, 0, out);
        while i < leaf.len() {
            let b = leaf[i].0[at];
            let run = (leaf[i..].iter().take(255))
                .take_while(|p| p.0[at] == b)
                .count();
            if let Some(out) = out.as_deref_mut() {
                out.byte(b);
                out.byte(run as u8);
                leaf[i..i + run].iter().for_each(|p| suffixes(out, p.0, 1));
            }
            (i, n) = (i + run, n + 1);
        }
        n
    };
    let (high, low) = if distinct == leaf.len() {
        (0, 1)
    } else {
        (
            leaf.len() * (packed - prefix_sum - 1) + 2 * runs(None),
            distinct * (packed - prefix_sum + 1),
        )
    };
    if low <= high {
        out.byte(0xfe);
        bounds(out);
        for group in leaf.chunk_by(|a, b| a.0 == b.0) {
            out.vint(group.len() as i32);
            suffixes(out, group[0].0, 0);
        }
    } else {
        out.byte(sorted as u8);
        bounds(out);
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

/// `BKDWriter.recursePackIndex`: the packed index of `n` leaves of `tree` from `off`. `last`
/// holds each dimension's nearest ancestor split value, `min_fp` the leftmost leaf of the parent;
/// a left child (`negative`) stores neither its pointer nor a positive delta.
fn pack_index(
    tree: &Bkd,
    min_fp: u64,
    last: &mut [u8],
    negative: bool,
    off: usize,
    n: usize,
) -> Vec<u8> {
    let fps = &tree.fps;
    let mut out = Output::default();
    if !negative {
        out.vlong((fps[off] - min_fp) as i64);
    }
    if n == 1 {
        return out.0;
    }
    let left = left_leaves(n);
    let (dim, split) = tree.splits[off + left - 1];
    let w = split.len();
    let last_dim = &mut last[dim * w..(dim + 1) * w];
    let prefix = (split.iter().zip(last_dim.iter()))
        .take_while(|(a, b)| a == b)
        .count();
    let delta = if prefix < w {
        (i32::from(split[prefix]) - i32::from(last_dim[prefix])).abs()
    } else {
        0
    };
    out.vint((delta * (w as i32 + 1) + prefix as i32) * tree.dims as i32 + dim as i32);
    if prefix + 1 < w {
        out.bytes(&split[prefix + 1..]);
    }
    let saved = last.to_vec();
    last[dim * w + prefix..(dim + 1) * w].copy_from_slice(&split[prefix..]);
    let left_bytes = pack_index(tree, fps[off], last, true, off, left);
    if left > 1 {
        out.vint(left_bytes.len() as i32);
    }
    out.bytes(&left_bytes);
    let right = pack_index(tree, fps[off], last, false, off + left, n - left);
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
                let (m, i, d) = write_points(&[(f.number, 1, pairs)], &seg.info.id).unwrap();
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
