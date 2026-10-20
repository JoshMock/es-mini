//! Opens the single segment of an index under tests/data.

use es_codec::{SegmentInfos, SegmentReader};
use std::path::PathBuf;

pub fn open(name: &str) -> SegmentReader {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/data")
        .join(name);
    let seg = SegmentInfos::read_latest(&dir).unwrap().segments.remove(0);
    SegmentReader::open(&dir, seg).unwrap()
}
