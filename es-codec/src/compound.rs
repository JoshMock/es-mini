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

/// Writes `<segment>.cfs` and `<segment>.cfe` from the sub-files `<segment><name>` in `dir`
/// (inverse of [`CompoundFile::open`]), copying them a buffer at a time and then deleting them,
/// as Lucene90CompoundFormat does, so no sub-file is held in memory.
pub fn write_compound(
    dir: &Path,
    segment: &str,
    id: &[u8; ID_LEN],
    names: &[String],
) -> Result<()> {
    use crate::output::Output;
    use std::io::Write;
    /// Counts and checksums what passes through to the `.cfs`.
    struct Crc<W>(W, u32, u64);
    impl<W: Write> Write for Crc<W> {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            let n = self.0.write(b)?;
            self.1 = codec_util::crc32_update(self.1, &b[..n]);
            self.2 += n as u64;
            Ok(n)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.0.flush()
        }
    }
    let (mut head, mut cfe) = (Output::default(), Output::default());
    head.index_header("Lucene90CompoundData", 0, id, "");
    cfe.index_header("Lucene90CompoundEntries", 0, id, "");
    cfe.vint(names.len() as i32);
    let path = dir.join(format!("{segment}.cfs"));
    let mut write = || -> std::io::Result<()> {
        let mut cfs = Crc(std::io::BufWriter::new(File::create(&path)?), 0, 0);
        cfs.write_all(&head.0)?;
        for name in names {
            let offset = cfs.2;
            let len = std::io::copy(
                &mut File::open(dir.join(format!("{segment}{name}")))?,
                &mut cfs,
            )?;
            cfe.string(name);
            cfe.long(offset as i64);
            cfe.long(len as i64);
        }
        let mut footer = Output::default();
        footer.be_int(codec_util::FOOTER_MAGIC);
        footer.be_int(0);
        cfs.write_all(&footer.0)?;
        let mut w = cfs.0;
        w.write_all(&i64::from(cfs.1).to_be_bytes())?;
        w.into_inner().map_err(|e| e.into_error())?.sync_all()
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
