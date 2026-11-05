//! Term statistics a shard's segments cannot give fail the open instead of going missing.

use es_index::disk;
use std::path::{Path, PathBuf};

mod common;
use common::Scratch;

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let to = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &to);
        } else {
            std::fs::copy(e.path(), to).unwrap();
        }
    }
}

/// Renames the postings format in `fnm` and rewrites its checksum, so it still verifies.
fn rename_postings_format(fnm: &Path) {
    let mut b = std::fs::read(fnm).unwrap();
    let at = b.windows(9).position(|w| w == b"Lucene104").unwrap();
    b[at..at + 9].copy_from_slice(b"Lucene999");
    let end = b.len() - 8;
    let crc = u64::from(es_codec::codec_util::crc32(&b[..end]));
    b[end..].copy_from_slice(&crc.to_be_bytes());
    std::fs::write(fnm, b).unwrap();
}

#[test]
fn open_fails_on_postings_it_cannot_read_for_term_statistics() {
    let es =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../fixtures/es-9.5.5-non-compound/data");
    let dir = Scratch::new("term-stats-unreadable", Some("data"));
    copy_dir(&es, &dir);
    let shard = dir.join("indices/yiDRF0tGQnaa05SvKeuoUg/0/index");
    for fnm in ["_2.fnm", "_2_1.fnm"] {
        rename_postings_format(&shard.join(fnm));
    }
    let err = disk::open(&dir).expect_err("open must fail");
    assert!(err.to_string().contains("Lucene999"), "{err}");
}
