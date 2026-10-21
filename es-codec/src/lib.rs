//! Lucene 10 on-disk codec, read side first. Ported from Lucene 10.5.1 sources.
//!
//! ponytail: files are read fully into memory; switch to mmap when indices outgrow RAM.

pub mod base64;
pub mod cluster_state;
pub mod codec_util;
pub mod compound;
pub mod data_dir;
mod direct_monotonic;
pub mod doc_values;
pub mod field_infos;
pub mod inflate;
pub mod input;
pub mod live_docs;
mod lz4;
pub mod norms;
pub mod output;
pub mod points;
pub mod postings;
pub mod segment;
pub mod segment_infos;
pub mod segment_writer;
pub mod sha256;
pub mod smile;
pub mod state_file;
pub mod stored_fields;
pub mod terms;
pub mod translog;
pub mod uid;

pub use compound::CompoundFile;
pub use segment::SegmentReader;
pub use segment_infos::{SegmentCommitInfo, SegmentInfo, SegmentInfos};

/// A corrupt, truncated, or unsupported index file.
#[derive(Debug, Clone, PartialEq)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

fn err<T>(msg: impl Into<String>) -> Result<T> {
    Err(Error(msg.into()))
}

fn read_file(path: &std::path::Path) -> Result<Vec<u8>> {
    std::fs::read(path).map_err(|e| Error(format!("{}: {e}", path.display())))
}

/// Writes `bytes` to `path` crash-safely: to a `.tmp` sibling, fsynced, renamed over `path`, then
/// the directory is fsynced (as Java `MetadataStateFormat.write` does).
pub fn write_file_atomic(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    let fail = |e: std::io::Error| Error(format!("{}: {e}", path.display()));
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp);
    let write = || -> std::io::Result<()> {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)?;
        match path.parent() {
            Some(dir) if !dir.as_os_str().is_empty() => std::fs::File::open(dir)?.sync_all(),
            _ => Ok(()),
        }
    };
    write().map_err(fail)
}

#[cfg(test)]
mod tests {
    #[test]
    fn write_file_atomic_replaces_and_leaves_no_tmp() {
        let dir = std::env::temp_dir().join(format!("esm-atomic-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("f");
        super::write_file_atomic(&path, b"one").unwrap();
        super::write_file_atomic(&path, b"two").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
