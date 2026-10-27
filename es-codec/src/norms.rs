//! `Lucene90NormsFormat` (`.nvm` meta, `.nvd` data): its reader, and a writer of 1-byte norms.
use crate::{
    Result, SegmentReader,
    codec_util::{self, ID_LEN},
    doc_values::{read_docs_with_field, write_docs_with_field},
    err,
    field_infos::IndexOptions,
    input::DataInput,
    output::Output,
};

/// Each doc's norm of `field` (0 for docs without one); `None` if the field has no norms.
pub(crate) fn read_norms(r: &SegmentReader, field: &str) -> Result<Option<Vec<i64>>> {
    let info = r.field(field)?;
    if info.omit_norms || info.index_options == IndexOptions::None {
        return Ok(None);
    }
    let (meta, data) = (r.cfs_file(".nvm")?, r.cfs_file(".nvd")?);
    let (id, max_doc) = (&r.seg.info.id, r.seg.info.max_doc);
    codec_util::check_footer(&meta)?;
    codec_util::check_footer(&data)?;
    let m = &mut DataInput::new(&meta);
    codec_util::check_index_header(m, "Lucene90NormsMetadata", 0, 0, id, "")?;
    let d = &mut DataInput::new(&data);
    codec_util::check_index_header(d, "Lucene90NormsData", 0, 0, id, "")?;
    loop {
        let number = m.read_int()?;
        if number == -1 {
            return err(format!("{} has no norms for {field}", r.seg.name));
        }
        let docs = read_docs_with_field(m)?;
        m.read_int()?; // numDocsWithField
        let bytes = m.read_byte()? as usize;
        let offset = m.read_long()?;
        if number != info.number {
            continue;
        }
        let mut out = vec![0; max_doc.max(0) as usize];
        for (i, doc) in docs.doc_ids(&data, max_doc)?.into_iter().enumerate() {
            let at = |n| {
                let start = usize::try_from(offset).ok()? + i * n;
                data.get(start..start + n)
            };
            let norm = match bytes {
                // constant: the offset slot holds the value
                0 => Some(offset),
                // little-endian, sign-extended
                n @ (1 | 2 | 4 | 8) => at(n).map(|b| {
                    let v = (b.iter().rev()).fold(0i64, |v, byte| v << 8 | i64::from(*byte));
                    v << (64 - 8 * n) >> (64 - 8 * n)
                }),
                n => return err(format!("invalid bytes per norm {n}")),
            };
            match (norm, out.get_mut(doc as usize)) {
                (Some(norm), Some(slot)) => *slot = norm,
                _ => return err(format!("norms of {field} extend past their data")),
            }
        }
        return Ok(Some(out));
    }
}

/// Returns `(nvm, nvd)` for `fields` of `(field number, norm per doc, `None` for docs without one)`.
///
/// ponytail: norms are 1 byte each (Lucene's `SmallFloat` encoding yields signed bytes); add wider
/// norms if a mapping needs them.
pub fn write_norms(id: &[u8; ID_LEN], fields: &[(i32, &[Option<i8>])]) -> (Vec<u8>, Vec<u8>) {
    let mut w = NormsWriter::new(id);
    fields
        .iter()
        .for_each(|&(number, per_doc)| w.add(number, per_doc));
    w.finish()
}

/// Writes `.nvm`/`.nvd` a field at a time.
pub struct NormsWriter {
    meta: Output,
    data: Output,
}

impl NormsWriter {
    pub fn new(id: &[u8; ID_LEN]) -> Self {
        let (mut meta, mut data) = (Output::default(), Output::default());
        meta.index_header("Lucene90NormsMetadata", 0, id, "");
        data.index_header("Lucene90NormsData", 0, id, "");
        Self { meta, data }
    }

    /// A field's norm per doc, `None` for docs without one.
    pub fn add(&mut self, number: i32, per_doc: &[Option<i8>]) {
        let Self { meta, data } = self;
        let docs: Vec<u32> = (0..per_doc.len() as u32)
            .filter(|d| per_doc[*d as usize].is_some())
            .collect();
        let norms: Vec<i8> = per_doc.iter().flatten().copied().collect();
        meta.bytes(&number.to_le_bytes());
        write_docs_with_field(meta, data, &docs, per_doc.len());
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

    /// Returns `(nvm, nvd)`.
    pub fn finish(mut self) -> (Vec<u8>, Vec<u8>) {
        self.meta.bytes(&(-1i32).to_le_bytes());
        (self.meta.finish(), self.data.finish())
    }
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
        let (nvm, nvd) = write_norms(&ID, &[(3, &[4, 3, 3, 6, 5].map(Some))]);
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
        let (nvm, nvd) = write_norms(&ID, &[(1, &[7, 7, 7].map(Some))]);
        assert_eq!(nvd.len(), 43 + 16);
        assert!(nvm.windows(9).any(|w| w == [0, 7, 0, 0, 0, 0, 0, 0, 0]));
    }
}
