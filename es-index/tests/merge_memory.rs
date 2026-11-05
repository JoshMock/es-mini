//! A merge streams a field's postings term by term, as Lucene's `SegmentMerger` does: it never
//! holds the merged (or a merged segment's) decoded postings of a field.

use es_codec::{SegmentInfos, cluster_state::read_indices};
use es_index::{FieldMapping, Index, Indices, disk};
use serde_json::json;
use std::path::{Path, PathBuf};

mod common;
use common::Scratch;

const TEST: &str = "memory_bounds_2_a_merge_streams_postings_term_by_term";
const DOCS: usize = 300;
const WORDS: usize = 200;

fn body(i: usize) -> String {
    (0..WORDS)
        .map(|j| format!("w{} ", (i * 7 + j) % 50))
        .collect()
}

fn shard(dir: &Path) -> PathBuf {
    let meta = read_indices(dir).unwrap();
    dir.join("indices").join(&meta[0].uuid).join("0/index")
}

fn segments(dir: &Path) -> usize {
    SegmentInfos::read_latest(&shard(dir))
        .unwrap()
        .segments
        .len()
}

/// `VmRSS` or `VmHWM` (peak resident) of this process, in bytes.
fn status(key: &str) -> usize {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let line = status.lines().find(|l| l.starts_with(key)).unwrap();
    let kb: usize = line.split_whitespace().nth(1).unwrap().parse().unwrap();
    kb * 1024
}

/// Writes one small document, which merges the segments, and measures the peak memory it took.
fn merge_and_measure(dir: &Path) {
    let before = segments(dir);
    let mut indices = disk::open(dir).unwrap();
    let rss = status("VmRSS:");
    let ix = indices.get_mut("big").unwrap();
    ix.put("last", json!({"body": "w1"}));
    disk::save(dir, &indices).unwrap();
    let peak = status("VmHWM:").saturating_sub(rss);
    assert!(segments(dir) < before, "no merge happened");
    // Decoded, each position takes over 40 bytes; encoded, about one.
    let positions = before * DOCS * WORDS;
    assert!(
        peak < 4 * positions,
        "merging {positions} positions took {peak} bytes"
    );
}

#[test]
fn memory_bounds_2_a_merge_streams_postings_term_by_term() {
    // Measured in a fresh process, whose allocator holds none of the memory that writing the
    // segments used and freed.
    if let Ok(dir) = std::env::var("ESM_MERGE_DIR") {
        return merge_and_measure(Path::new(&dir));
    }
    let dir = Scratch::new("merge-memory", Some("data"));
    let mut ix = Index::default();
    ix.mappings.properties.insert(
        "body".into(),
        FieldMapping::Text {
            store: false,
            fields: Default::default(),
        },
    );
    let mut indices = Indices::from([("big".to_owned(), ix)]);
    // As many segments as the merge policy leaves alone (`segmentsPerTier`).
    for round in 0..8 {
        let ix = indices.get_mut("big").unwrap();
        for i in 0..DOCS {
            let n = round * DOCS + i;
            ix.put(&n.to_string(), json!({"body": body(n)}));
        }
        disk::save(&dir, &indices).unwrap();
    }
    assert_eq!(segments(&dir), 8);
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([TEST, "--exact", "--nocapture"])
        .env("ESM_MERGE_DIR", &*dir)
        .status()
        .unwrap();
    assert!(status.success());
}
