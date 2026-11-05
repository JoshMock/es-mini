//! Compound files (`.cfs` data + `.cfe` entries, Lucene90CompoundFormat), and the separate files
//! of a non-compound segment behind the same interface.

use crate::{
    Result,
    codec_util::{self, ID_LEN},
    err,
    input::DataInput,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    os::unix::fs::FileExt,
    path::Path,
};

/// A segment's files stay on disk; sub-files are read from them when asked for.
pub struct CompoundFile {
    /// The `.cfs`, or each file of a non-compound segment.
    files: Vec<File>,
    /// File name without the segment prefix (e.g. `.fnm`) -> (index in `files`, offset, length).
    entries: BTreeMap<String, (usize, u64, u64)>,
}

fn open_file(path: &Path) -> Result<(File, u64)> {
    let io = |e: std::io::Error| crate::Error(format!("{}: {e}", path.display()));
    let file = File::open(path).map_err(io)?;
    let len = file.metadata().map_err(io)?.len();
    Ok((file, len))
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
            entries.insert(name, (0, offset as u64, len as u64));
        }
        let (data, len) = open_file(&dir.join(format!("{segment}.cfs")))?;
        let cfs = Self {
            files: vec![data],
            entries,
        };
        // Like Lucene's `CodecUtil.retrieveChecksum`, only the footer's structure is checked on
        // open; checksumming the whole file would read every sub-file.
        let header = cfs.read_at(0, 0, len.min(64))?;
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
        codec_util::check_footer_structure(&cfs.read_at(
            0,
            footer_at,
            codec_util::FOOTER_LEN as u64,
        )?)?;
        if let Some((name, _)) = cfs
            .entries
            .iter()
            .find(|(_, (_, o, l))| o.checked_add(*l).is_none_or(|end| end > len))
        {
            return err(format!(
                "compound entry {name} extends past the end of {segment}.cfs"
            ));
        }
        Ok(cfs)
    }

    /// The `files` of a non-compound segment (its `SegmentInfo.files`), each its own sub-file.
    pub fn open_files(dir: &Path, segment: &str, files: &BTreeSet<String>) -> Result<Self> {
        let mut cfs = Self {
            files: vec![],
            entries: BTreeMap::new(),
        };
        for name in files {
            let Some(ext) = name.strip_prefix(segment) else {
                return err(format!("{name} is not a file of segment {segment}"));
            };
            let (file, len) = open_file(&dir.join(name))?;
            cfs.entries.insert(ext.into(), (cfs.files.len(), 0, len));
            cfs.files.push(file);
        }
        Ok(cfs)
    }

    /// Each file's length and the checksum its footer records, which covers all its bytes.
    fn footer(&self, at: usize) -> Result<(u64, i64)> {
        let io = |e: std::io::Error| crate::Error(format!("reading segment file: {e}"));
        let len = self.files[at].metadata().map_err(io)?.len();
        let Some(footer_at) = len.checked_sub(codec_util::FOOTER_LEN as u64) else {
            return err("segment file is too short for a footer");
        };
        let footer = self.read_at(at, footer_at, codec_util::FOOTER_LEN as u64)?;
        Ok((len, codec_util::check_footer_structure(&footer)?))
    }

    /// The checksum each file's footer records, unverified: a fingerprint of the segment's
    /// bytes that reads only the footers.
    pub fn checksums(&self) -> Result<Vec<i64>> {
        (0..self.files.len())
            .map(|at| Ok(self.footer(at)?.1))
            .collect()
    }

    /// Checks the CRC32 of each whole file against its footer (Lucene's
    /// `CodecUtil.checksumEntireFile`), reading it a chunk at a time.
    pub fn verify(&self) -> Result<()> {
        for at in 0..self.files.len() {
            let (len, footer) = self.footer(at)?;
            let (mut crc, mut from, end) = (0, 0, len - 8);
            while from < end {
                let n = (end - from).min(1 << 20);
                crc = codec_util::crc32_update(crc, &self.read_at(at, from, n)?);
                from += n;
            }
            if i64::from(crc) != footer {
                return err(format!(
                    "segment file checksum mismatch: expected {footer:#x}, got {crc:#x}"
                ));
            }
        }
        Ok(())
    }

    fn read_at(&self, file: usize, offset: u64, len: u64) -> Result<Vec<u8>> {
        let mut buf = vec![0; len as usize];
        self.files[file]
            .read_exact_at(&mut buf, offset)
            .map_err(|e| crate::Error(format!("reading segment file: {e}")))?;
        Ok(buf)
    }

    /// A sub-file by its name without the segment prefix, e.g. `.fnm` or `_Lucene90_0.dvd`.
    pub fn file(&self, ext: &str) -> Result<Option<Vec<u8>>> {
        self.entries
            .get(ext)
            .map(|&(f, o, l)| self.read_at(f, o, l))
            .transpose()
    }

    /// `len` bytes at `offset` of a sub-file, or `None` if there is no such sub-file.
    pub fn slice(&self, ext: &str, offset: u64, len: u64) -> Result<Option<Vec<u8>>> {
        let Some(&(f, o, l)) = self.entries.get(ext) else {
            return Ok(None);
        };
        if offset.checked_add(len).is_none_or(|end| end > l) {
            return err(format!("read past the end of compound entry {ext}"));
        }
        self.read_at(f, o + offset, len).map(Some)
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(String::as_str)
    }
}

/// Writes `<segment>.cfs` and `<segment>.cfe` from the sub-files `<segment><name>` in `dir`
/// (inverse of [`CompoundFile::open`]), copying them a buffer at a time and then deleting them,
/// as Lucene90CompoundFormat does, so no sub-file is held in memory.
pub fn write_compound(
    dir: &Path,
    segment: &str,
    id: &[u8; ID_LEN],
    names: &[String],
) -> Result<()> {
    use crate::output::{FileOutput, Output};
    use std::io::Write;
    let (mut head, mut cfe) = (Output::default(), Output::default());
    head.index_header("Lucene90CompoundData", 0, id, "");
    cfe.index_header("Lucene90CompoundEntries", 0, id, "");
    cfe.vint(names.len() as i32);
    let path = dir.join(format!("{segment}.cfs"));
    let mut write = || -> std::io::Result<()> {
        let mut cfs = FileOutput::create(&path)?;
        cfs.write_all(&head.0)?;
        for name in names {
            let offset = cfs.file_pointer();
            let len = std::io::copy(
                &mut File::open(dir.join(format!("{segment}{name}")))?,
                &mut cfs,
            )?;
            cfe.string(name);
            cfe.long(offset as i64);
            cfe.long(len as i64);
        }
        cfs.finish()?.sync_all()
    };
    write().map_err(|e| crate::Error(format!("{}: {e}", path.display())))?;
    crate::write_file_new(&dir.join(format!("{segment}.cfe")), &cfe.finish())?;
    for name in names {
        // A leftover is deleted by the next commit, as an unreferenced file.
        let _ = std::fs::remove_file(dir.join(format!("{segment}{name}")));
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
        std::fs::write(dir.join("_0.a"), b"hello").unwrap();
        std::fs::write(dir.join("_0.b"), b"world!").unwrap();
        write_compound(&dir, "_0", &[3; ID_LEN], &[".a".into(), ".b".into()]).unwrap();
        let cfs = CompoundFile::open(&dir, "_0", &[3; ID_LEN]).unwrap();
        cfs.verify().unwrap();
        assert!(!dir.join("_0.a").exists() && !dir.join("_0.b").exists());
        assert_eq!(cfs.file(".a").unwrap().unwrap(), b"hello");
        assert_eq!(cfs.file(".b").unwrap().unwrap(), b"world!");
        assert_eq!(cfs.slice(".b", 1, 3).unwrap().unwrap(), b"orl");
        assert!(cfs.slice(".a", 3, 3).is_err());
        assert!(cfs.file(".c").unwrap().is_none());
        assert!(CompoundFile::open(&dir, "_0", &[4; ID_LEN]).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
