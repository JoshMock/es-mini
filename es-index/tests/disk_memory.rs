//! es_index::disk::open serves documents from the segments: opening a directory does not copy
//! every `_source` into memory.

use es_index::{Index, Indices, disk};
use serde_json::json;
use std::path::Path;

const TEST: &str = "opening_20000_docs_leaves_their_sources_on_disk";
const DOCS: usize = 20_000;

fn body() -> String {
    "lorem ipsum dolor sit amet ".repeat(100)
}

/// Resident memory of this process, in bytes.
fn rss() -> usize {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let line = status.lines().find(|l| l.starts_with("VmRSS:")).unwrap();
    let kb: usize = line.split_whitespace().nth(1).unwrap().parse().unwrap();
    kb * 1024
}

fn open_and_measure(dir: &Path) {
    let sources = DOCS * body().len();
    let before = rss();
    let opened = disk::open(dir).unwrap();
    let held = rss().saturating_sub(before);
    assert!(
        held < sources / 4,
        "open grew memory by {held} bytes; the sources are {sources}"
    );
    let big = &opened["big"];
    let doc = big.get("12345").unwrap();
    assert_eq!(doc.source, json!({"n": 12345, "body": body()}));
    assert_eq!(doc.version, 1);
    assert_eq!(big.docs().count(), DOCS);
}

#[test]
fn opening_20000_docs_leaves_their_sources_on_disk() {
    // Measured in a fresh process, whose allocator holds none of the memory that building the
    // directory used and freed.
    if let Ok(dir) = std::env::var("ESM_MEMORY_DIR") {
        return open_and_measure(Path::new(&dir));
    }
    let root = std::env::temp_dir().join(format!("esm-memory-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let dir = root.join("data");
    let mut ix = Index::default();
    for i in 0..DOCS {
        ix.put(&i.to_string(), json!({"n": i, "body": body()}));
    }
    disk::save(&dir, &Indices::from([("big".to_owned(), ix)])).unwrap();
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([TEST, "--exact", "--nocapture"])
        .env("ESM_MEMORY_DIR", &dir)
        .status()
        .unwrap();
    std::fs::remove_dir_all(&root).unwrap();
    assert!(status.success());
}
