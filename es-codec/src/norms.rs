//! `Lucene90NormsFormat` writer for fields that have a norm on every doc (`.nvm` meta, `.nvd` data).
use crate::{codec_util::ID_LEN, output::Output};

/// Returns `(nvm, nvd)` for `fields` of `(field number, one norm per doc)`.
///
/// ponytail: norms are 1 byte each (Lucene's `SmallFloat` encoding yields signed bytes) and every
/// doc has one; add wider norms and sparse (IndexedDISI) fields if a mapping needs them.
pub fn write_norms(id: &[u8; ID_LEN], fields: &[(i32, &[i8])]) -> (Vec<u8>, Vec<u8>) {
    let (mut meta, mut data) = (Output::default(), Output::default());
    meta.index_header("Lucene90NormsMetadata", 0, id, "");
    data.index_header("Lucene90NormsData", 0, id, "");
    for &(number, norms) in fields {
        meta.bytes(&number.to_le_bytes());
        meta.long(-1); // docsWithField: all docs
        meta.long(0);
        meta.bytes(&(-1i16).to_le_bytes()); // jump table entries
        meta.byte(0xff); // dense rank power
        meta.bytes(&(norms.len() as i32).to_le_bytes());
        match norms.split_first() {
            Some((&first, rest)) if rest.iter().all(|&n| n == first) => {
                meta.byte(0); // constant: the offset slot holds the value
                meta.long(first as i64);
            }
            _ => {
                meta.byte(1);
                meta.long(data.0.len() as i64);
                norms.iter().for_each(|&n| data.byte(n as u8));
            }
        }
    }
    meta.bytes(&(-1i32).to_le_bytes());
    (meta.finish(), data.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: [u8; 16] = [
        0xdb, 0xd1, 0x78, 0xa7, 0x54, 0x3c, 0xa0, 0x2b, 0xd7, 0x31, 0x7b, 0xde, 0xb2, 0x68, 0x14,
        0x47,
    ];

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len() / 2)
            .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
            .collect()
    }

    /// Bytes of `.nvm`/`.nvd` from the ES 9.5.5 fixture shard `_0` (field 3 = `title`, 5 docs).
    #[test]
    fn matches_es_9_5_5_fixture() {
        let (nvm, nvd) = write_norms(&ID, &[(3, &[4, 3, 3, 6, 5])]);
        assert_eq!(
            nvd,
            unhex(
                "3fd76c17114c7563656e6539304e6f726d734461746100000000dbd178a7543ca02bd7317bdeb2681447000403030605c02893e8000000000000000096ee8861"
            )
        );
        assert_eq!(
            nvm,
            unhex(
                "3fd76c17154c7563656e6539304e6f726d734d6574616461746100000000dbd178a7543ca02bd7317bdeb26814470003000000ffffffffffffffff0000000000000000ffffff05000000012b00000000000000ffffffffc02893e8000000000000000019b682c5"
            )
        );
    }

    #[test]
    fn constant_norms_store_no_data() {
        let (nvm, nvd) = write_norms(&ID, &[(1, &[7, 7, 7])]);
        assert_eq!(nvd.len(), 43 + 16);
        assert!(nvm.windows(9).any(|w| w == [0, 7, 0, 0, 0, 0, 0, 0, 0]));
    }
}
