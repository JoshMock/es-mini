//! Compound files (`.cfs` data + `.cfe` entries, Lucene90CompoundFormat).

use crate::{
    Result,
    codec_util::{self, ID_LEN},
    err,
    input::DataInput,
};
use std::{collections::BTreeMap, fs::File, os::unix::fs::FileExt, path::Path};

/// The `.cfs` stays on disk; sub-files are read from it when asked for.
pub struct CompoundFile {
    data: File,
    /// File extension (e.g. `.fnm`) -> (offset, length) in `data`.
    entries: BTreeMap<String, (u64, u64)>,
}

impl CompoundFile {
    pub fn open(dir: &Path, segment: &str, id: &[u8; ID_LEN]) -> Result<Self> {
        let cfe = crate::read_file(&dir.join(format!("{segment}.cfe")))?;
        codec_util::check_footer(&cfe)?;
        let input = &mut DataInput::new(&cfe);
        codec_util::check_index_header(input, "Lucene90CompoundEntries", 0, 0, id, "")?;
        let mut entries = BTreeMap::new();
        for _ in 0..input.read_len()? {
            let name = input.read_string()?;
            let (offset, len) = (input.read_long()?, input.read_long()?);
            entries.insert(name, (offset as u64, len as u64));
        }
        let path = dir.join(format!("{segment}.cfs"));
        let io = |e: std::io::Error| crate::Error(format!("{}: {e}", path.display()));
        let data = File::open(&path).map_err(io)?;
        let len = data.metadata().map_err(io)?.len();
        let cfs = Self { data, entries };
        // Like Lucene's `CodecUtil.retrieveChecksum`, only the footer's structure is checked on
        // open; checksumming the whole file would read every sub-file.
        let header = cfs.read_at(0, len.min(64))?;
        codec_util::check_index_header(
            &mut DataInput::new(&header),
            "Lucene90CompoundData",
            0,
            0,
            id,
            "",
        )?;
        let Some(footer_at) = len.checked_sub(codec_util::FOOTER_LEN as u64) else {
            return err(format!("{segment}.cfs is too short for a footer"));
        };
        codec_util::check_footer_structure(
            &cfs.read_at(footer_at, codec_util::FOOTER_LEN as u64)?,
        )?;
        if let Some((name, _)) = cfs
            .entries
            .iter()
            .find(|(_, (o, l))| o.checked_add(*l).is_none_or(|end| end > len))
        {
            return err(format!(
                "compound entry {name} extends past the end of {segment}.cfs"
            ));
        }
        Ok(cfs)
    }

    /// Checks the CRC32 of the whole `.cfs` against its footer (Lucene's
    /// `CodecUtil.checksumEntireFile`), reading it a chunk at a time.
    pub fn verify(&self) -> Result<()> {
        let io = |e: std::io::Error| crate::Error(format!("reading compound file: {e}"));
        let len = self.data.metadata().map_err(io)?.len();
        let footer = codec_util::check_footer_structure(&self.read_at(
            len - codec_util::FOOTER_LEN as u64,
            codec_util::FOOTER_LEN as u64,
        )?)?;
        let (mut crc, mut at, end) = (0, 0, len - 8);
        while at < end {
            let n = (end - at).min(1 << 20);
            crc = codec_util::crc32_update(crc, &self.read_at(at, n)?);
            at += n;
        }
        if i64::from(crc) != footer {
            return err(format!(
                "compound file checksum mismatch: expected {footer:#x}, got {crc:#x}"
            ));
        }
        Ok(())
    }

    fn read_at(&self, offset: u64, len: u64) -> Result<Vec<u8>> {
        let mut buf = vec![0; len as usize];
        self.data
            .read_exact_at(&mut buf, offset)
            .map_err(|e| crate::Error(format!("reading compound file: {e}")))?;
        Ok(buf)
    }

    /// A sub-file by its name without the segment prefix, e.g. `.fnm` or `_Lucene90_0.dvd`.
    pub fn file(&self, ext: &str) -> Result<Option<Vec<u8>>> {
        self.entries
            .get(ext)
            .map(|&(o, l)| self.read_at(o, l))
            .transpose()
    }

    /// `len` bytes at `offset` of a sub-file, or `None` if there is no such sub-file.
    pub fn slice(&self, ext: &str, offset: u64, len: u64) -> Result<Option<Vec<u8>>> {
        let Some(&(o, l)) = self.entries.get(ext) else {
            return Ok(None);
        };
        if offset.checked_add(len).is_none_or(|end| end > l) {
            return err(format!("read past the end of compound entry {ext}"));
        }
        self.read_at(o + offset, len).map(Some)
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(String::as_str)
    }
}

/// Writes `<segment>.cfs` and `<segment>.cfe` from `(name without segment prefix, file bytes)`
/// pairs (inverse of [`CompoundFile::open`]).
pub fn write_compound(
    dir: &Path,
    segment: &str,
    id: &[u8; ID_LEN],
    files: &[(String, Vec<u8>)],
) -> Result<()> {
    use crate::output::Output;
    let (mut cfs, mut cfe) = (Output::default(), Output::default());
    cfs.index_header("Lucene90CompoundData", 0, id, "");
    cfe.index_header("Lucene90CompoundEntries", 0, id, "");
    cfe.vint(files.len() as i32);
    for (name, bytes) in files {
        cfe.string(name);
        cfe.long(cfs.0.len() as i64);
        cfe.long(bytes.len() as i64);
        cfs.bytes(bytes);
    }
    for (ext, out) in [("cfs", cfs), ("cfe", cfe)] {
        let path = dir.join(format!("{segment}.{ext}"));
        crate::write_file_new(&path, &out.finish())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_sub_files_and_slices_from_disk() {
        let dir = std::env::temp_dir().join(format!("esm-cfs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let files = [
            (".a".to_string(), b"hello".to_vec()),
            (".b".into(), b"world!".to_vec()),
        ];
        write_compound(&dir, "_0", &[3; ID_LEN], &files).unwrap();
        let cfs = CompoundFile::open(&dir, "_0", &[3; ID_LEN]).unwrap();
        assert_eq!(cfs.file(".b").unwrap().unwrap(), b"world!");
        assert_eq!(cfs.slice(".b", 1, 3).unwrap().unwrap(), b"orl");
        assert!(cfs.slice(".a", 3, 3).is_err());
        assert!(cfs.file(".c").unwrap().is_none());
        assert!(CompoundFile::open(&dir, "_0", &[4; ID_LEN]).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
