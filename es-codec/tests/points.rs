//! Multi-leaf BKD trees written by real Lucene 10.5.1: see tests/gen/GenPoints.java and
//! GenRanges.java and GenRangeTies.java, regenerate with tests/gen/gen_docker.sh Points points
//! (or Ranges ranges, RangeTies range-ties).

mod common;

use std::{cell::Cell, cmp::Ordering};

const DOCS: i64 = 70_000;

/// `(doc, value)` of every point the generator wrote for `field`.
fn expected(field: &str) -> Vec<(u32, i64)> {
    (0..DOCS)
        .flat_map(|d| {
            let values = match field {
                "seq" => vec![d],
                "mod" if d % 4 == 0 => vec![d % 1000],
                "const" if d % 3 == 0 => vec![7],
                "sparse" if d % 10 == 0 => vec![-d, d * 1_000_003],
                _ => vec![],
            };
            values.into_iter().map(move |v| (d as u32, v))
        })
        .collect()
}

fn place(lo: i64, hi: i64) -> impl Fn(i64) -> Ordering {
    move |v| {
        if v < lo {
            Ordering::Less
        } else if v > hi {
            Ordering::Greater
        } else {
            Ordering::Equal
        }
    }
}

#[test]
fn ranges_match_every_point_in_them() {
    let r = common::open("points");
    for (field, ranges) in [
        (
            "seq",
            vec![
                (0, DOCS),
                (5000, 5999),
                (511, 512),
                (-5, -1),
                (DOCS, i64::MAX),
            ],
        ),
        ("mod", vec![(0, 999), (7, 7), (250, 260), (1000, 2000)]),
        ("const", vec![(7, 7), (0, 6), (8, 9), (i64::MIN, i64::MAX)]),
        (
            "sparse",
            vec![(-100, 100), (i64::MIN, -30_000), (1, 5_000_000), (0, 0)],
        ),
    ] {
        let points = expected(field);
        for (lo, hi) in ranges {
            let mut got = r.point_docs(field, place(lo, hi)).unwrap();
            got.sort_unstable();
            let mut want: Vec<u32> = (points.iter())
                .filter(|(_, v)| (lo..=hi).contains(v))
                .map(|(d, _)| *d)
                .collect();
            want.sort_unstable();
            assert_eq!(got, want, "{field} {lo}..={hi}");
        }
    }
}

/// Ranges read only the leaves they visit, each bounded by the next leaf's start or the next
/// field's data; windows across every leaf boundary of each field match every point in them.
#[test]
fn disk_speed_ranges_across_leaf_and_field_boundaries_match_every_point() {
    let r = common::open("points");
    for (field, step) in [
        ("sparse", 70_000_170),
        ("mod", 37),
        ("const", 1),
        ("seq", 97),
    ] {
        let points = expected(field);
        let (min, max) = (
            points.iter().map(|p| p.1).min(),
            points.iter().map(|p| p.1).max(),
        );
        let (min, max) = (min.unwrap(), max.unwrap());
        let mut lo = min - step;
        while lo <= max + step {
            let hi = lo + step;
            let mut got = r.point_docs(field, place(lo, hi)).unwrap();
            got.sort_unstable();
            let mut want: Vec<u32> = (points.iter())
                .filter(|(_, v)| (lo..=hi).contains(v))
                .map(|(d, _)| *d)
                .collect();
            want.sort_unstable();
            assert_eq!(got, want, "{field} {lo}..={hi}");
            lo = hi;
        }
    }
}

/// A narrow range reads a leaf or two, not every value.
#[test]
fn a_narrow_range_skips_other_leaves() {
    let r = common::open("points");
    let calls = Cell::new(0);
    let docs = r
        .point_docs("seq", |v| {
            calls.set(calls.get() + 1);
            place(30_000, 30_009)(v)
        })
        .unwrap();
    assert_eq!(docs, (30_000..30_010).collect::<Vec<u32>>());
    assert!(calls.get() < 2_000, "{} values compared", calls.get());
}

#[test]
fn fields_without_points_err() {
    let r = common::open("stored-and-numeric");
    let field = &r.fields[0].name;
    assert!(r.point_docs(field, place(0, 1)).is_err());
}

/// es-codec writes the same tree bytes as Lucene for the same points.
#[test]
fn written_trees_match_lucene() {
    use es_codec::points::{sortable_bytes, write_points};
    let r = common::open("points");
    // in the order Lucene's flush wrote them (its field hash table's)
    let fields: Vec<_> = ["sparse", "mod", "const", "seq"]
        .iter()
        .map(|name| {
            let f = r.field(name).unwrap();
            let width = f.point_num_bytes as usize;
            let pairs = (expected(name).into_iter())
                .map(|(d, v)| (d, sortable_bytes(v, width)))
                .collect();
            (f.number, 1, pairs)
        })
        .collect();
    let (meta, index, data) = write_points(&fields, &r.seg.info.id, r.seg.info.max_doc).unwrap();
    for (ext, ours) in [(".kdm", meta), (".kdi", index), (".kdd", data)] {
        let real = r.cfs.file(ext).unwrap().unwrap();
        let at = (real.iter().zip(&ours)).position(|(a, b)| a != b);
        assert!(
            at.is_none() && real.len() == ours.len(),
            "{ext} differs at {at:?}"
        );
    }
}

/// `(doc, from, to)` of every LongRange tests/gen/GenRanges.java wrote for `field`.
fn expected_ranges(field: &str) -> Vec<(u32, i64, i64)> {
    (0..3000)
        .flat_map(|d: i64| {
            let ranges = match field {
                "wide" => vec![((d - 1500) * 1_000_003, (d - 1500) * 1_000_003 + d * 7)],
                "few" if d % 3 == 0 => vec![(d % 5, d % 5 + 10), (-1, d % 7)],
                "same" if d % 100 == 0 => vec![(3, 9)],
                _ => vec![],
            };
            ranges.into_iter().map(move |(f, t)| (d as u32, f, t))
        })
        .collect()
}

/// es-codec writes the same two-dimensional trees as Lucene's flush for the same ranges.
#[test]
fn written_range_trees_match_lucene() {
    use es_codec::points::{sortable_bytes, write_points};
    let r = common::open("ranges");
    // in the order Lucene's flush wrote them (its field hash table's)
    let fields: Vec<_> = ["few", "wide", "same"]
        .iter()
        .map(|name| {
            let f = r.field(name).unwrap();
            assert_eq!((f.point_dimensions, f.point_num_bytes), (2, 8));
            let pairs = (expected_ranges(name).into_iter())
                .map(|(d, from, to)| (d, [sortable_bytes(from, 8), sortable_bytes(to, 8)].concat()))
                .collect();
            (f.number, 2, pairs)
        })
        .collect();
    let (meta, index, data) = write_points(&fields, &r.seg.info.id, r.seg.info.max_doc).unwrap();
    for (ext, ours) in [(".kdm", meta), (".kdi", index), (".kdd", data)] {
        let real = r.cfs.file(ext).unwrap().unwrap();
        let at = (real.iter().zip(&ours)).position(|(a, b)| a != b);
        assert!(
            at.is_none() && real.len() == ours.len(),
            "{ext} differs at {at:?}"
        );
    }
}

/// `(doc, from, to)` of every LongRange tests/gen/GenRangeTies.java wrote for `field`, in the
/// order it added them; `from` and `to` hold one value per range dimension.
fn expected_ties(field: &str) -> Vec<(u32, Vec<i64>, Vec<i64>)> {
    (0..4000)
        .flat_map(|d: i64| {
            let m = d % 50;
            let ranges = match field {
                "ties" => [3, 1, 2]
                    .map(|k| (vec![m], vec![m + k]))
                    .into_iter()
                    .chain([2, 0, 1].map(|k| (vec![m - k], vec![m + 5])))
                    .collect(),
                "box" => [2, 3, 1]
                    .map(|k| (vec![d % 37, d % 11], vec![d % 37 + k, d % 11 + 2 * k]))
                    .to_vec(),
                "leaf" if d % 1000 == 0 => [3, 1, 2]
                    .map(|k| (vec![k], vec![5]))
                    .into_iter()
                    .chain([(vec![4], vec![6])])
                    .collect(),
                _ => vec![],
            };
            ranges.into_iter().map(move |(f, t)| (d as u32, f, t))
        })
        .collect()
}

/// es-codec orders points that tie in a split or leaf sort dimension and doc as Lucene's flush
/// does, in trees of two and four dimensions.
#[test]
fn points_fidelity_equal_points_order_like_lucene_flush() {
    use es_codec::points::{sortable_bytes, write_points};
    let r = common::open("range-ties");
    // in the order Lucene's flush wrote them (its field hash table's)
    let fields: Vec<_> = ["ties", "box", "leaf"]
        .iter()
        .map(|name| {
            let f = r.field(name).unwrap();
            let pairs = (expected_ties(name).into_iter())
                .map(|(d, from, to)| {
                    let bytes = from.iter().chain(&to).map(|v| sortable_bytes(*v, 8));
                    (d, bytes.collect::<Vec<_>>().concat())
                })
                .collect();
            (f.number, f.point_dimensions as usize, pairs)
        })
        .collect();
    let (meta, index, data) = write_points(&fields, &r.seg.info.id, r.seg.info.max_doc).unwrap();
    for (ext, ours) in [(".kdm", meta), (".kdi", index), (".kdd", data)] {
        let real = r.cfs.file(ext).unwrap().unwrap();
        let at = (real.iter().zip(&ours)).position(|(a, b)| a != b);
        assert!(
            at.is_none() && real.len() == ours.len(),
            "{ext} differs at {at:?}"
        );
    }
}
