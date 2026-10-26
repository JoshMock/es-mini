//! es_index::disk::open reads an index Elasticsearch wrote with index.codec=best_compression
//! (Zstd stored fields). Data from tests/gen/best_compression.sh.

use es_index::disk;
use serde_json::json;
use std::path::PathBuf;

const WORDS: &str = "alpha bravo charlie delta echo foxtrot golf hotel india juliet kilo lima mike \
    november oscar papa quebec romeo sierra tango uniform victor whiskey xray yankee zulu red \
    orange yellow green blue indigo violet black white grey brown pink cyan magenta";

fn source(i: usize) -> serde_json::Value {
    let words: Vec<&str> = WORDS.split_whitespace().collect();
    let body: Vec<&str> = (0..30).map(|k| words[(i * k) % 40]).collect();
    json!({"n": i, "tag": format!("t{}", i % 7), "body": body.join(" ")})
}

#[test]
fn an_unreadable_index_fails_open_with_its_name() {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data/best-compression");
    let dir = std::env::temp_dir().join(format!("esm-unreadable-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let copied = std::process::Command::new("cp")
        .args(["-r".as_ref(), src.as_os_str(), dir.as_os_str()])
        .status()
        .unwrap();
    assert!(copied.success());
    let commit = "indices/gp91DUCbQ46iX8DHkjmDTg/0/index/segments_3";
    std::fs::remove_file(dir.join(commit)).unwrap();
    let opened = disk::open(&dir);
    std::fs::remove_dir_all(&dir).unwrap();
    let e = opened.unwrap_err().0;
    assert!(e.starts_with("index [bc]: "), "{e}");
}

#[test]
fn opens_a_best_compression_index() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data/best-compression");
    let indices = disk::open(&dir).unwrap();
    let bc = indices.get("bc").expect("index bc is opened");
    assert_eq!(bc.docs().count(), 200);
    for (id, doc) in bc.docs() {
        assert_eq!(doc.source, source(id.parse().unwrap()), "doc {id}");
    }
}
