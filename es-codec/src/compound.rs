//! Compound files (`.cfs` data + `.cfe` entries, Lucene90CompoundFormat).

use crate::{
    Result,
    codec_util::{self, ID_LEN},
    err,
    input::DataInput,
};
use std::{collections::BTreeMap, path::Path};

pub struct CompoundFile {
    data: Vec<u8>,
    /// File extension (e.g. `.fnm`) -> (offset, length) in `data`.
    entries: BTreeMap<String, (usize, usize)>,
}

impl CompoundFile {
    pub fn open(dir: &Path, segment: &str, id: &[u8; ID_LEN]) -> Result<Self> {
        let read = |ext: &str| crate::read_file(&dir.join(format!("{segment}.{ext}")));
        let cfe = read("cfe")?;
        codec_util::check_footer(&cfe)?;
        let input = &mut DataInput::new(&cfe);
        codec_util::check_index_header(input, "Lucene90CompoundEntries", 0, 0, id, "")?;
        let mut entries = BTreeMap::new();
        for _ in 0..input.read_len()? {
            let name = input.read_string()?;
            let (offset, len) = (input.read_long()?, input.read_long()?);
            entries.insert(name, (offset as usize, len as usize));
        }
        let data = read("cfs")?;
        codec_util::check_footer(&data)?;
        codec_util::check_index_header(
            &mut DataInput::new(&data),
            "Lucene90CompoundData",
            0,
            0,
            id,
            "",
        )?;
        if let Some((name, _)) = entries
            .iter()
            .find(|(_, (o, l))| o.checked_add(*l).is_none_or(|end| end > data.len()))
        {
            return err(format!(
                "compound entry {name} extends past the end of {segment}.cfs"
            ));
        }
        Ok(Self { data, entries })
    }

    /// A sub-file by its name without the segment prefix, e.g. `.fnm` or `_Lucene90_0.dvd`.
    pub fn file(&self, ext: &str) -> Option<&[u8]> {
        self.entries.get(ext).map(|&(o, l)| &self.data[o..o + l])
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(String::as_str)
    }
}
