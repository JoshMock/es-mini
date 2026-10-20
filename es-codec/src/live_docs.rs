//! Live docs: hard deletes (`.liv`) combined with soft deletes (the soft-deletes doc values field).
use crate::{
    CompoundFile, Result, SegmentCommitInfo, codec_util,
    doc_values::{self, DocValues},
    err,
    field_infos::FieldInfo,
    input::DataInput,
    segment_infos::radix36,
};
use std::path::Path;

fn read_file(dir: &Path, name: &str) -> Result<Vec<u8>> {
    let p = dir.join(name);
    std::fs::read(&p).map_err(|e| crate::Error(format!("{}: {e}", p.display())))
}

/// `Lucene90LiveDocsFormat`: a bit set of live docs; absent when the segment has no hard deletes.
fn read_liv(file: &[u8], seg: &SegmentCommitInfo) -> Result<Vec<bool>> {
    parse_liv(
        file,
        &seg.info.id,
        seg.del_gen,
        seg.info.max_doc as usize,
        seg.del_count,
    )
}

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

/// Whether each doc id is live: not hard-deleted and without a value in the soft-deletes field.
pub fn read_live_docs(
    dir: &Path,
    seg: &SegmentCommitInfo,
    cfs: &CompoundFile,
    fields: &[FieldInfo],
) -> Result<Vec<bool>> {
    let mut live = match seg.del_gen {
        -1 => vec![true; seg.info.max_doc as usize],
        g => read_liv(
            &read_file(dir, &format!("{}_{}.liv", seg.name, radix36(g as u64)))?,
            seg,
        )?,
    };
    let Some(soft) = fields.iter().find(|f| f.soft_deletes) else {
        return Ok(live);
    };
    let DocValues::Numbers(values) =
        doc_values::read_field_values(dir, seg, cfs, fields, &soft.name)?
    else {
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
