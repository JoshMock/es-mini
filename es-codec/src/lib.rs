//! Lucene 10 on-disk codec, read side first. Ported from Lucene 10.5.1 sources.
//!
//! ponytail: files are read fully into memory; switch to mmap when indices outgrow RAM.

pub mod codec_util;
pub mod compound;
pub mod field_infos;
pub mod input;
pub mod segment_infos;
pub mod stored_fields;

pub use compound::CompoundFile;
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
