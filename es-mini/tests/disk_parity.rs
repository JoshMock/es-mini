//! Opens the recorded Elasticsearch 9.5.5 data directory (fixtures/es-9.5.5/data) and compares
//! es-mini's responses with the ones Elasticsearch recorded for the same requests. Every case
//! must match.

mod common;

use common::parity::{compare, fixtures};

#[tokio::test]
async fn disk_matches_recorded_elasticsearch_responses() {
    let indices = es_index::disk::open(&fixtures().join("es-9.5.5/data")).unwrap();
    compare(&es_mini::app_with(indices), &Default::default()).await;
}
