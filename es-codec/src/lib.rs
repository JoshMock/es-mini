//! Lucene 10 on-disk codec, read side first. Ported from Lucene 10.5.1 sources.
//!
//! ponytail: files are read fully into memory; switch to mmap when indices outgrow RAM.

pub mod cluster_state;
pub mod codec_util;
pub mod compound;
pub mod data_dir;
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
