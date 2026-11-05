//! The in-place selector and sorter `BKDWriter` orders a flushed field's points with
//! (`MutablePointTreeReaderUtils.partition` and `sortByDim`). Points that compare equal (same
//! value in the dimension, same doc) end up wherever their swaps leave them, which decides the
//! side of a split and the order in a leaf they take, so this follows Lucene swap for swap.

use std::cmp::Ordering;

/// A point's packed value and doc.
pub(super) type Point<'a> = (&'a [u8], u32);

type Cmp<'c> = &'c dyn Fn(&Point, &Point) -> Ordering;

/// Orders points by the bytes `r` of their value, then doc.
fn by(r: std::ops::Range<usize>) -> impl Fn(&Point, &Point) -> Ordering {
    move |a, b| (&a.0[r.clone()], a.1).cmp(&(&b.0[r.clone()], b.1))
}

/// `MutablePointTreeReaderUtils.partition`: puts the point at `mid` that `pts` sorted by the
/// bytes `dim` of their values, then doc, would have there, smaller ones before and larger ones
/// after it. All values share the first `prefix` bytes of `dim`; `max_doc` is the segment's.
pub(super) fn partition(
    pts: &mut [Point],
    dim: std::ops::Range<usize>,
    prefix: usize,
    max_doc: i32,
    mid: usize,
) {
    // `PackedInts.bitsRequired(maxDoc - 1)`
    let bits = (32 - ((max_doc.max(1) - 1) as u32).leading_zeros()).max(1) as usize;
    let (start, dim_bytes) = (dim.start + prefix, dim.len() - prefix);
    let byte_at = move |p: &Point, k: usize| match k.checked_sub(dim_bytes) {
        None => usize::from(p.0[start + k]),
        Some(k) => (p.1 >> bits.saturating_sub((k + 1) * 8) & 0xff) as usize,
    };
    let radix = Radix {
        byte_at: &byte_at,
        max_len: dim_bytes + bits.div_ceil(8),
        cmp: &by(dim),
    };
    radix.select(pts, 0, pts.len(), mid, 0, 0);
}

/// `MutablePointTreeReaderUtils.sortByDim`: sorts a leaf by the bytes `dim`, then doc.
pub(super) fn sort_by_dim(pts: &mut [Point], dim: std::ops::Range<usize>) {
    intro_sort(pts, 0, pts.len() as isize, 2 * log2(pts.len()), &by(dim));
}

/// `MathUtil.log(x, 2)`
fn log2(x: usize) -> i32 {
    x.checked_ilog2().unwrap_or(0) as i32
}

/// `RadixSelector` over the bytes `byte_at` gives (none is the -1 of a shorter key), falling
/// back to an `IntroSelector` by `cmp`.
struct Radix<'c> {
    byte_at: &'c dyn Fn(&Point, usize) -> usize,
    max_len: usize,
    cmp: Cmp<'c>,
}

impl Radix<'_> {
    fn select(&self, a: &mut [Point], from: usize, to: usize, k: usize, d: usize, l: usize) {
        if to - from <= 100 || l >= 8 {
            intro_select(a, from as isize, to as isize, k as isize, self.cmp);
        } else {
            self.radix_select(a, from, to, k, d, l);
        }
    }

    fn radix_select(&self, a: &mut [Point], from: usize, to: usize, k: usize, d: usize, l: usize) {
        // bucket 0 is for keys shorter than `d`, which fixed-length ones never are
        let mut histogram = [0usize; 257];
        let common = self.histogram(a, from, to, d, &mut histogram);
        if common > 0 {
            if d + common < self.max_len && histogram[0] < to - from {
                self.radix_select(a, from, to, k, d + common, l);
            }
            return;
        }
        let mut bucket_from = from;
        for (bucket, &n) in histogram.iter().enumerate() {
            let bucket_to = bucket_from + n;
            if bucket_to > k {
                self.partition(a, from..to, bucket, bucket_from..bucket_to, d);
                if bucket != 0 && d + 1 < self.max_len {
                    self.select(a, bucket_from, bucket_to, k, d + 1, l + 1);
                }
                return;
            }
            bucket_from = bucket_to;
        }
    }

    fn bucket(&self, p: &Point, d: usize) -> usize {
        (self.byte_at)(p, d) + 1
    }

    /// `computeCommonPrefixLengthAndBuildHistogram`: the length of the bytes from `k` all of
    /// `a[from..to]` share (at most 24), or 0 after counting each byte at `k` in `histogram`.
    fn histogram(
        &self,
        a: &[Point],
        from: usize,
        to: usize,
        k: usize,
        hist: &mut [usize],
    ) -> usize {
        let byte_at = self.byte_at;
        let mut len = 24.min(self.max_len).min(self.max_len - k);
        let common: Vec<usize> = (0..len).map(|j| byte_at(&a[from], k + j)).collect();
        let mut i = from + 1;
        'outer: while i < to {
            for (j, &c) in common.iter().enumerate().take(len) {
                let b = byte_at(&a[i], k + j);
                if b != c {
                    len = j;
                    if len == 0 {
                        hist[common[0] + 1] = i - from;
                        hist[b + 1] = 1;
                        break 'outer;
                    }
                    break;
                }
            }
            i += 1;
        }
        if i < to {
            for p in &a[i + 1..to] {
                hist[self.bucket(p, k)] += 1;
            }
        } else {
            hist[common[0] + 1] = to - from;
        }
        len
    }

    /// `RadixSelector.partition`: moves the points of `range` in `bucket` to `bucket_range`,
    /// smaller buckets before it and larger after.
    fn partition(
        &self,
        a: &mut [Point],
        range: std::ops::Range<usize>,
        bucket: usize,
        bucket_range: std::ops::Range<usize>,
        d: usize,
    ) {
        let (bucket_from, bucket_to) = (bucket_range.start, bucket_range.end);
        let (mut left, mut right, mut slot) = (range.start, range.end - 1, bucket_from);
        loop {
            let mut left_bucket = self.bucket(&a[left], d);
            let mut right_bucket = self.bucket(&a[right], d);
            while left_bucket <= bucket && left < bucket_from {
                if left_bucket == bucket {
                    a.swap(left, slot);
                    slot += 1;
                } else {
                    left += 1;
                }
                left_bucket = self.bucket(&a[left], d);
            }
            while right_bucket >= bucket && right >= bucket_to {
                if right_bucket == bucket {
                    a.swap(right, slot);
                    slot += 1;
                } else {
                    right -= 1;
                }
                right_bucket = self.bucket(&a[right], d);
            }
            if left < bucket_from && right >= bucket_to {
                a.swap(left, right);
                left += 1;
                right -= 1;
            } else {
                break;
            }
        }
    }
}

/// `compare(i, j)` of Lucene's sorters and selectors.
fn c(a: &[Point], cmp: Cmp, i: isize, j: isize) -> Ordering {
    cmp(&a[i as usize], &a[j as usize])
}

fn swap(a: &mut [Point], i: isize, j: isize) {
    a.swap(i as usize, j as usize);
}

fn median(a: &[Point], cmp: Cmp, i: isize, j: isize, k: isize) -> isize {
    if c(a, cmp, i, j).is_lt() {
        if c(a, cmp, j, k).is_le() {
            return j;
        }
        return if c(a, cmp, i, k).is_lt() { k } else { i };
    }
    if c(a, cmp, j, k).is_ge() {
        return j;
    }
    if c(a, cmp, i, k).is_lt() { i } else { k }
}

/// The pivot the quicksort loop of `IntroSorter` (no `k`) or `IntroSelector` picks in
/// `from..to` of more than 3 points.
fn pivot(a: &[Point], cmp: Cmp, from: isize, to: isize, k: Option<isize>) -> isize {
    let (size, last) = (to - from, to - 1);
    let mid = (from + last) >> 1;
    if size <= 40 {
        let range = size >> 2;
        return median(a, cmp, mid - range, mid, mid + range);
    }
    let range = size >> 3;
    let double = range << 1;
    let first = median(a, cmp, from, from + range, from + double);
    let middle = median(a, cmp, mid - range, mid, mid + range);
    let end = median(a, cmp, last - double, last - range, last);
    let (i, j, l) = (first, middle, end);
    match k {
        // `IntroSelector.min`
        Some(k) if k - from < range => {
            if c(a, cmp, i, j).is_le() {
                return if c(a, cmp, i, l).is_le() { i } else { l };
            }
            if c(a, cmp, j, l).is_le() { j } else { l }
        }
        // `IntroSelector.max`
        Some(k) if to - k <= range => {
            if c(a, cmp, i, j).is_le() {
                return if c(a, cmp, j, l).is_lt() { l } else { j };
            }
            if c(a, cmp, i, l).is_lt() { l } else { i }
        }
        _ => median(a, cmp, i, j, l),
    }
}

/// The three-way partition of `from..to` around the point at `pivot` that `IntroSorter` and
/// `IntroSelector` share: returns `(j, i)`, where `from..=j` hold smaller points, `i..to`
/// larger ones and the points between equal ones.
fn partition3(a: &mut [Point], cmp: Cmp, from: isize, to: isize, pivot: isize) -> (isize, isize) {
    let value = a[pivot as usize];
    let vs = |a: &[Point], j: isize| cmp(&value, &a[j as usize]);
    swap(a, from, pivot);
    let last = to - 1;
    let (mut i, mut j, mut p, mut q) = (from, to, from + 1, last);
    loop {
        i += 1;
        let mut left = vs(a, i);
        while left.is_gt() {
            i += 1;
            left = vs(a, i);
        }
        j -= 1;
        let mut right = vs(a, j);
        while right.is_lt() {
            j -= 1;
            right = vs(a, j);
        }
        if i >= j {
            if i == j && right.is_eq() {
                swap(a, i, p);
            }
            break;
        }
        swap(a, i, j);
        if right.is_eq() {
            swap(a, i, p);
            p += 1;
        }
        if left.is_eq() {
            swap(a, j, q);
            q -= 1;
        }
    }
    i = j + 1;
    for l in from..p {
        swap(a, l, j);
        j -= 1;
    }
    for l in (q + 1..=last).rev() {
        swap(a, l, i);
        i += 1;
    }
    (j, i)
}

/// `IntroSelector.select`, without the shuffle Lucene does after `2 log2 n` rounds: it is
/// random (`SplittableRandom`), so no port can match it.
fn intro_select(a: &mut [Point], mut from: isize, mut to: isize, k: isize, cmp: Cmp) {
    while to - from > 3 {
        let pivot = pivot(a, cmp, from, to, Some(k));
        let (j, i) = partition3(a, cmp, from, to, pivot);
        if k <= j {
            to = j + 1;
        } else if k >= i {
            from = i;
        } else {
            return;
        }
    }
    match to - from {
        2 if c(a, cmp, from, from + 1).is_gt() => swap(a, from, from + 1),
        3 => {
            let (mid, last) = (from + 1, from + 2);
            if c(a, cmp, from, mid).is_le() {
                if c(a, cmp, mid, last).is_gt() {
                    swap(a, mid, last);
                    if c(a, cmp, from, mid).is_gt() {
                        swap(a, from, mid);
                    }
                }
            } else if c(a, cmp, mid, last).is_ge() {
                swap(a, from, last);
            } else {
                swap(a, from, mid);
                if c(a, cmp, mid, last).is_gt() {
                    swap(a, mid, last);
                }
            }
        }
        _ => {}
    }
}

/// `IntroSorter.sort`.
fn intro_sort(a: &mut [Point], mut from: isize, mut to: isize, mut max_depth: i32, cmp: Cmp) {
    while to - from > 16 {
        max_depth -= 1;
        if max_depth < 0 {
            heap_sort(a, from, to, cmp);
            return;
        }
        let pivot = pivot(a, cmp, from, to, None);
        let (j, i) = partition3(a, cmp, from, to, pivot);
        if j - from < to - 1 - i {
            intro_sort(a, from, j + 1, max_depth, cmp);
            from = i;
        } else {
            intro_sort(a, i, to, max_depth, cmp);
            to = j + 1;
        }
    }
    // `Sorter.insertionSort`
    for i in from + 1..to {
        let mut current = i;
        while c(a, cmp, current - 1, current).is_gt() {
            swap(a, current - 1, current);
            if current - 1 == from {
                break;
            }
            current -= 1;
        }
    }
}

/// `Sorter.heapSort`.
fn heap_sort(a: &mut [Point], from: isize, to: isize, cmp: Cmp) {
    if to - from <= 1 {
        return;
    }
    for i in (from..=((to - 2 - from) >> 1) + from).rev() {
        sift_down(a, i, from, to, cmp);
    }
    for end in (from + 1..to).rev() {
        swap(a, from, end);
        sift_down(a, from, from, end, cmp);
    }
}

fn sift_down(a: &mut [Point], mut i: isize, from: isize, to: isize, cmp: Cmp) {
    loop {
        let left = ((i - from) << 1) + 1 + from;
        if left >= to {
            return;
        }
        let right = left + 1;
        let next = if c(a, cmp, i, left).is_lt() {
            if right < to && c(a, cmp, left, right).is_lt() {
                right
            } else {
                left
            }
        } else if right < to && c(a, cmp, i, right).is_lt() {
            right
        } else {
            return;
        };
        swap(a, i, next);
        i = next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whatever path the selector and sorter take (radix levels, fallback selector, insertion
    /// sort, quicksort, heapsort), they order by value then doc.
    #[test]
    fn select_and_sort_order_by_value_then_doc() {
        let values: Vec<[u8; 2]> = (0..3000u32)
            .map(|i| [(i * 7919 % 13) as u8, (i * 31 % 251) as u8])
            .collect();
        let pts: Vec<Point> = (values.iter().zip(0..))
            .map(|(v, d)| (&v[..], d * 37 % 3001))
            .collect();
        let mut sorted = pts.clone();
        sorted.sort_by(by(0..2));
        for mid in [0, 1, 50, 1500, 2999] {
            let mut p = pts.clone();
            partition(&mut p, 0..2, 0, 3001, mid);
            assert_eq!(by(0..2)(&p[mid], &sorted[mid]), Ordering::Equal);
            assert!(p[..mid].iter().all(|x| by(0..2)(x, &p[mid]).is_le()));
            assert!(p[mid..].iter().all(|x| by(0..2)(x, &p[mid]).is_ge()));
        }
        for n in [1, 2, 3, 16, 17, 40, 41, 3000] {
            let mut p = pts[..n].to_vec();
            sort_by_dim(&mut p, 0..2);
            assert!(p.windows(2).all(|w| by(0..2)(&w[0], &w[1]).is_le()));
        }
        let mut p = pts.clone();
        heap_sort(&mut p, 0, 3000, &by(0..2));
        assert!(p.windows(2).all(|w| by(0..2)(&w[0], &w[1]).is_le()));
    }
}
