//! Replays fixtures/generate.sh's ingest into an in-memory es-mini and compares its responses
//! with the ones real Elasticsearch 9.5.5 recorded. Known differences live in
//! fixtures/parity-xfail.txt.

mod common;

use common::parity::{compare, fixtures, parse_script, send};
use std::{collections::BTreeSet, fs};

#[tokio::test]
async fn in_memory_matches_recorded_elasticsearch_responses() {
    let app = es_mini::app();
    for r in &parse_script().0 {
        send(&app, r).await;
    }
    let xfail: BTreeSet<String> = fs::read_to_string(fixtures().join("parity-xfail.txt"))
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_owned)
        .collect();
    compare(&app, &xfail).await;
}
