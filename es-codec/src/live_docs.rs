//! Live docs: hard deletes (`.liv`) combined with soft deletes (the soft-deletes doc values field).
use crate::{
    Result, SegmentReader, codec_util, doc_values::DocValues, err, input::DataInput,
    output::Output, segment_infos::radix36,
};

/// `Lucene90LiveDocsFormat`: a bit set of live docs; absent when the segment has no hard deletes.
fn parse_liv(
    file: &[u8],
    id: &[u8; codec_util::ID_LEN],
    del_gen: i64,
    max_doc: usize,
    del_count: i32,
) -> Result<Vec<bool>> {
    codec_util::check_footer(file)?;
    let input = &mut DataInput::new(file);
    let suffix = radix36(del_gen as u64);
    codec_util::check_index_header(input, "Lucene90LiveDocs", 0, 0, id, &suffix)?;
    let mut live = Vec::with_capacity(max_doc);
    for _ in 0..max_doc.div_ceil(64) {
        let word = input.read_long()? as u64;
        live.extend((0..64).map(|b| word >> b & 1 == 1));
    }
    live.truncate(max_doc);
    if (live.iter().filter(|l| !**l).count() as i32) != del_count {
        return err("live docs do not match the committed delete count");
    }
    Ok(live)
}

/// Writes a `.liv` file for `live` (one flag per doc); inverse of `parse_liv`.
pub fn write_liv(live: &[bool], id: &[u8; codec_util::ID_LEN], del_gen: i64) -> Vec<u8> {
    let mut out = Output::default();
    out.index_header("Lucene90LiveDocs", 0, id, &radix36(del_gen as u64));
    for chunk in live.chunks(64) {
        out.long(
            chunk
                .iter()
                .enumerate()
                .fold(0, |w, (b, l)| w | (*l as i64) << b),
        );
    }
    out.finish()
}

/// Whether each doc id is live: not hard-deleted and without a value in the soft-deletes field.
pub(crate) fn read_live_docs(r: &SegmentReader) -> Result<Vec<bool>> {
    let seg = &r.seg;
    let mut live = match seg.del_gen {
        -1 => vec![true; r.max_doc()],
        g => parse_liv(
            &r.gen_file(&format!("_{}.liv", radix36(g as u64)))?,
            &seg.info.id,
            g,
            r.max_doc(),
            seg.del_count,
        )?,
    };
    let Some(soft) = r.fields.iter().find(|f| f.soft_deletes) else {
        return Ok(live);
    };
    let DocValues::Numbers(values) = r.doc_values(&soft.name)? else {
        return err("soft deletes field is not numeric");
    };
    // One entry per doc, so the indexes line up with `live`.
    for (live, value) in live.iter_mut().zip(&values) {
        *live &= value.is_empty();
    }
    Ok(live)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn liv_file(id: &[u8; 16], suffix: &str, words: &[u64]) -> Vec<u8> {
        let mut f = codec_util::CODEC_MAGIC.to_be_bytes().to_vec();
        f.push(16);
        f.extend(b"Lucene90LiveDocs");
        f.extend(0i32.to_be_bytes());
        f.extend(id);
        f.push(suffix.len() as u8);
        f.extend(suffix.as_bytes());
        words.iter().for_each(|w| f.extend(w.to_le_bytes()));
        f.extend(codec_util::FOOTER_MAGIC.to_be_bytes());
        f.extend(0i32.to_be_bytes());
        let crc = codec_util::crc32(&f) as i64;
        f.extend(crc.to_be_bytes());
        f
    }

    #[test]
    fn write_liv_round_trips() {
        let id = [9; 16];
        let live: Vec<bool> = (0..70).map(|d| d != 3 && d != 66).collect();
        let file = write_liv(&live, &id, 1);
        assert_eq!(parse_liv(&file, &id, 1, 70, 2).unwrap(), live);
        // Same bytes as the hand-built Lucene layout (padding bits are 0).
        let mut words = [!0u64; 2];
        words[0] &= !(1 << 3);
        words[1] = 0b11_1111 & !(1 << 2);
        assert_eq!(file, liv_file(&id, "1", &words));
    }

    #[test]
    fn liv_bits_are_live_docs() {
        let id = [7; 16];
        // 70 docs; docs 1 and 69 are deleted.
        let file = liv_file(&id, "2", &[!0b10, !(1 << 5) & 0x3f]);
        let live = parse_liv(&file, &id, 2, 70, 2).unwrap();
        let dead: Vec<_> = (0..70).filter(|d| !live[*d]).collect();
        assert_eq!(dead, [1, 69]);
        // A delete count that disagrees with the bits is corruption.
        assert!(parse_liv(&file, &id, 2, 70, 1).is_err());
    }
}
