//! Writes Lucene 10 BKD points (`.kdm`/`.kdi`/`.kdd`), one single-leaf tree per field.

use crate::{codec_util::ID_LEN, output::Output};

/// Sortable big-endian bytes for `v` in a `width`-byte field (`LongPoint` is 8, `IntPoint` is 4).
pub fn sortable_bytes(v: i64, width: usize) -> Vec<u8> {
    let be = ((v as u64) ^ (1 << 63)).to_be_bytes();
    if width == 8 {
        be.to_vec()
    } else {
        (((v as i32) as u32) ^ (1 << 31)).to_be_bytes().to_vec()
    }
}

/// `(doc, sortable value bytes)` pairs of one field, in doc order.
pub type PointValues = Vec<(u32, Vec<u8>)>;

/// `(meta, index, data)` files. `fields` is `(field number, (doc, value) pairs)`; every value in
/// a field has the same width, and a doc may have none or several, but a field has at least one.
// ponytail: one leaf per field regardless of size; a real tree if Elasticsearch must range-search
// huge shards fast.
pub fn write_points(
    fields: &[(i32, PointValues)],
    id: &[u8; ID_LEN],
) -> crate::Result<(Vec<u8>, Vec<u8>, Vec<u8>)> {
    let (mut meta, mut index, mut data) = (Output::default(), Output::default(), Output::default());
    meta.index_header("Lucene90PointsFormatMeta", 1, id, "");
    index.index_header("Lucene90PointsFormatIndex", 1, id, "");
    data.index_header("Lucene90PointsFormatData", 1, id, "");
    for (number, pairs) in fields {
        let values: Vec<&[u8]> = pairs.iter().map(|p| p.1.as_slice()).collect();
        let Some(width) = values.first().map(|v| v.len()) else {
            return crate::err(format!("points field {number} has no values"));
        };
        let mut order: Vec<usize> = (0..values.len()).collect();
        order.sort_by(|a, b| values[*a].cmp(values[*b]).then(a.cmp(b)));
        let (min, max) = (values[order[0]], values[order[order.len() - 1]]);
        let prefix = min
            .iter()
            .zip(max.iter())
            .take_while(|(a, b)| a == b)
            .count();
        let leaf = data.0.len() as i64;
        data.vint(values.len() as i32);
        let ids: Vec<u32> = order.iter().map(|i| pairs[*i].0).collect();
        if ids.windows(2).all(|w| w[1] == w[0] + 1) {
            data.byte(254); // CONTIGUOUS_ID: the run starting at one doc id
            data.vint(ids[0] as i32);
        } else {
            // ponytail: Lucene also packs ids as 16/21/24-bit or a bitset; plain ints read the same.
            data.byte(32);
            for id in &ids {
                data.bytes(&(*id as i32).to_le_bytes());
            }
        }
        data.vint(prefix as i32);
        data.bytes(&min[..prefix]);
        if prefix == width {
            data.byte(0xff);
        } else {
            data.byte(0);
            let mut i = 0;
            while i < order.len() {
                let b = values[order[i]][prefix];
                let run = order[i..]
                    .iter()
                    .take(255)
                    .take_while(|j| values[**j][prefix] == b)
                    .count();
                data.byte(b);
                data.byte(run as u8);
                for j in &order[i..i + run] {
                    data.bytes(&values[*j][prefix + 1..]);
                }
                i += run;
            }
        }
        let index_start = index.0.len() as i64;
        index.vlong(leaf);
        meta.bytes(&number.to_le_bytes());
        meta.header("BKD", 10);
        meta.vint(1);
        meta.vint(1);
        meta.vint(values.len().max(512) as i32);
        meta.vint(width as i32);
        meta.vint(1);
        meta.bytes(min);
        meta.bytes(max);
        meta.vlong(values.len() as i64);
        let mut docs: Vec<u32> = pairs.iter().map(|p| p.0).collect();
        docs.dedup(); // pairs are in doc order
        meta.vint(docs.len() as i32);
        meta.vint((index.0.len() as i64 - index_start) as i32);
        meta.long(leaf);
        meta.long(index_start);
    }
    let (index, data) = (index.finish(), data.finish());
    meta.bytes(&(-1i32).to_le_bytes());
    meta.long(index.len() as i64);
    meta.long(data.len() as i64);
    Ok((meta.finish(), index, data))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sortable_bytes_order_like_numbers() {
        assert_eq!(sortable_bytes(4, 8), [0x80, 0, 0, 0, 0, 0, 0, 4]);
        assert_eq!(sortable_bytes(-1, 4), [0x7f, 0xff, 0xff, 0xff]);
        assert!(sortable_bytes(-5, 8) < sortable_bytes(3, 8));
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
