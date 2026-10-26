//! Lucene90 doc values (`.dvm` metadata, `.dvd` data).
//!
//! Fields are decoded eagerly into per-doc vectors. The `.dvs` skip index is not used: its
//! metadata is skipped over.

mod read;
mod write;

pub(crate) use read::{read_docs_with_field, read_field_values};
pub(crate) use write::write_docs_with_field;
pub use write::{write_doc_values, write_doc_values_with};

const VERSION_MAX: i32 = 2;
/// Doc id block index of the IndexedDISI terminator block (`NO_MORE_DOCS >>> 16`).
const DISI_END_BLOCK: u16 = 0x7fff;
/// IndexedDISI `DEFAULT_DENSE_RANK_POWER`: a rank entry every 512 doc ids of a bitset block.
const DENSE_RANK_POWER: u8 = 9;
const TERMS_DICT_BLOCK_LZ4_SHIFT: u32 = 6;
const TERMS_DICT_BLOCK_SIZE: usize = 1 << TERMS_DICT_BLOCK_LZ4_SHIFT;
/// Block shift of the direct-monotonic tables the writer emits (Lucene90DocValuesFormat).
const DIRECT_MONOTONIC_BLOCK_SHIFT: i32 = 16;
const TERMS_DICT_REVERSE_INDEX_SHIFT: i32 = 10;
const TERMS_DICT_REVERSE_INDEX_MASK: usize = (1 << TERMS_DICT_REVERSE_INDEX_SHIFT) - 1;

/// A field's doc values, one entry per doc id (empty when the doc has no value).
#[derive(Debug, Clone, PartialEq)]
pub enum DocValues {
    /// Numeric and sorted numeric fields; doubles are sortable-long encoded.
    Numbers(Vec<Vec<i64>>),
    /// Sorted and sorted set fields: each doc's ordinals in ascending order, which index
    /// `terms` (the field's distinct values in byte order).
    Ordinals {
        ords: Vec<Vec<usize>>,
        terms: Vec<Vec<u8>>,
    },
    /// Binary fields: one value per doc, `None` where the doc has none.
    Bytes(Vec<Option<Vec<u8>>>),
}

#[cfg(test)]
mod tests {
    use super::read::decode;
    use super::*;
    use crate::output::Output;

    #[test]
    fn write_then_read_numeric_round_trips() {
        let id = [3u8; 16];
        let wrap = |v: Vec<i64>| v.into_iter().map(|x| vec![x]).collect::<Vec<_>>();
        let a = wrap((0..1000).map(|i| i * 37 - 5000).collect());
        let b = wrap(vec![i64::MIN, i64::MAX, 0]);
        let c = wrap(vec![42; 1000]);
        let sparse: Vec<Vec<i64>> = (0..1000).map(|i| vec![i * 3; i as usize % 2]).collect();
        let fields = [(0, a.clone()), (1, c.clone()), (2, sparse.clone())];
        let (m, d) = write_doc_values(&fields, &[], &id, "Lucene90_0");
        let read = |n, len| decode(&m, &d, (&id, len), "Lucene90_0", n, &|_| Ok(false)).unwrap();
        assert_eq!(read(0, 1000), DocValues::Numbers(a));
        assert_eq!(read(1, 1000), DocValues::Numbers(c));
        assert_eq!(read(2, 1000), DocValues::Numbers(sparse));
        let (m, d) = write_doc_values(&[(7, b.clone())], &[], &id, "Lucene90_0");
        let got = decode(&m, &d, (&id, 3), "Lucene90_0", 7, &|_| Ok(false)).unwrap();
        assert_eq!(got, DocValues::Numbers(b));
    }

    #[test]
    fn write_then_read_sorted_numeric_round_trips() {
        let id = [5u8; 16];
        let v = vec![10, -3, 25, 7];
        let per_doc = v.iter().map(|x| vec![*x]).collect();
        let (m, d) = write_doc_values_with(&[], &[(2, per_doc)], &[], &id, "Lucene90_0");
        let got = decode(&m, &d, (&id, 4), "Lucene90_0", 2, &|_| Ok(false)).unwrap();
        assert_eq!(
            got,
            DocValues::Numbers(v.into_iter().map(|x| vec![x]).collect())
        );
    }

    /// Sparse and multi-valued sorted numerics, incl. a bitset block and several 64K blocks.
    #[test]
    fn write_then_read_sparse_multi_valued_round_trips() {
        let id = [6u8; 16];
        let small = vec![vec![], vec![9, 2, 2], vec![], vec![5]];
        let sparse: Vec<Vec<i64>> = (0..70_000)
            .map(|i: i64| if i % 3 == 0 { vec![i] } else { vec![] })
            .collect();
        let dense_block: Vec<Vec<i64>> = (0..5000).map(|i| vec![i * 2, i]).collect();
        let tail = (0..140_000).map(|i| if i > 139_990 { vec![1] } else { vec![] });
        let far: Vec<Vec<i64>> = tail.collect();
        for per_doc in [small, sparse, dense_block, far] {
            let (m, d) =
                write_doc_values_with(&[], &[(2, per_doc.clone())], &[], &id, "Lucene90_0");
            let got = decode(
                &m,
                &d,
                (&id, per_doc.len() as i32),
                "Lucene90_0",
                2,
                &|_| Ok(false),
            )
            .unwrap();
            let sorted: Vec<Vec<i64>> = per_doc
                .into_iter()
                .map(|mut v| {
                    v.sort();
                    v
                })
                .collect();
            assert_eq!(got, DocValues::Numbers(sorted));
        }
    }

    #[test]
    fn write_then_read_sparse_multi_valued_keyword_round_trips() {
        let id = [8u8; 16];
        let t = |s: &str| s.as_bytes().to_vec();
        let vals = vec![
            vec![t("b"), t("a")],
            vec![],
            vec![t("b")],
            vec![t("b"), t("b")],
        ];
        let (m, d) = write_doc_values(&[], &[(1, vals)], &id, "Lucene90_0");
        let got = decode(&m, &d, (&id, 4), "Lucene90_0", 1, &|_| Ok(false)).unwrap();
        let DocValues::Ordinals { ords, terms } = got else {
            panic!("not ordinals")
        };
        assert_eq!(terms, vec![t("a"), t("b")]);
        assert_eq!(ords, vec![vec![0, 1], vec![], vec![1], vec![1]]);
        let sparse_single = vec![vec![], vec![t("x")]];
        let (m, d) = write_doc_values(&[], &[(1, sparse_single)], &id, "Lucene90_0");
        let got = decode(&m, &d, (&id, 2), "Lucene90_0", 1, &|_| Ok(false)).unwrap();
        let DocValues::Ordinals { ords, .. } = got else {
            panic!("not ordinals")
        };
        assert_eq!(ords, vec![vec![], vec![0]]);
    }

    #[test]
    fn write_then_read_keyword_round_trips() {
        let id = [4u8; 16];
        let vals: Vec<Vec<u8>> = (0..300)
            .map(|i| {
                format!(
                    "a-common-prefix-longer-than-fifteen-bytes-{:03}",
                    i * 7 % 150
                )
                .into_bytes()
            })
            .collect();
        let (m, d) = write_doc_values(
            &[(0, vec![vec![5]; 300])],
            &[(1, vals.iter().map(|v| vec![v.clone()]).collect())],
            &id,
            "Lucene90_0",
        );
        let got = decode(&m, &d, (&id, 300), "Lucene90_0", 1, &|_| Ok(false)).unwrap();
        let DocValues::Ordinals { ords, terms } = got else {
            panic!("not ordinals")
        };
        assert_eq!(terms.len(), 150);
        let back: Vec<_> = ords.iter().map(|o| terms[o[0]].clone()).collect();
        assert_eq!(back, vals);
    }

    /// Field "half" of tests/data/stored-and-numeric (every other doc of 70000: a bitset block,
    /// then a sorted list), whose IndexedDISI Lucene 10.5.1 wrote with rank and jump tables.
    #[test]
    fn writes_the_indexed_disi_lucene_writes() {
        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/stored-and-numeric");
        let seg = crate::SegmentInfos::read_latest(&dir)
            .unwrap()
            .segments
            .remove(0);
        let r = crate::SegmentReader::open(&dir, seg).unwrap();
        let DocValues::Numbers(half) = r.doc_values("half").unwrap() else {
            panic!("not numbers")
        };
        let docs: Vec<u32> = (0..half.len() as u32)
            .filter(|d| !half[*d as usize].is_empty())
            .collect();
        let (mut meta, mut data) = (Output::default(), Output::default());
        write_docs_with_field(&mut meta, &mut data, &docs, half.len());
        assert_eq!(meta.0[meta.0.len() - 1], DENSE_RANK_POWER);
        let lucene = r.cfs_file("_Lucene90_0.dvd").unwrap();
        assert!(lucene.windows(data.0.len()).any(|w| w == data.0));
    }

    /// The ES 9.5.5 fixture's `tag` terms (animal, misc, pet) and ordinals, byte for byte.
    #[test]
    fn small_keyword_matches_es_fixture_terms_block() {
        let vals = ["pet", "animal", "misc"]
            .map(|v| v.as_bytes().to_vec())
            .to_vec();
        let vals = vals.into_iter().map(|v| vec![v]).collect();
        let (_, d) = write_doc_values(&[], &[(2, vals)], &[0; 16], "x");
        let block = [
            6, b'a', b'n', b'i', b'm', b'a', b'l', 9, 0x90, 0x30, b'm', b'i', b's', b'c', 0x20,
            b'p', b'e', b't',
        ];
        assert!(d.windows(block.len()).any(|w| w == block));
    }
}
