//! Elasticsearch 9.5.5 data with a non-compound segment (fixtures/generate_non_compound.sh).

use es_index::{Indices, disk};
use std::path::PathBuf;

mod common;
use common::Scratch;

fn ids(indices: &Indices) -> Vec<String> {
    let mut ids: Vec<_> = indices["nocfs"]
        .docs()
        .map(|(id, _)| id.to_string())
        .collect();
    ids.sort();
    ids
}

#[test]
fn opens_and_resaves_an_index_with_a_non_compound_segment() {
    let es =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../fixtures/es-9.5.5-non-compound/data");
    let indices = disk::open(&es).unwrap();
    assert_eq!(ids(&indices), ["1", "2", "4", "5", "6"]);
    assert_eq!(indices["nocfs"].get("5").unwrap().source["views"], 42);
    let dir = Scratch::new("non-compound", Some("data"));
    disk::save(&dir, &indices).unwrap();
    assert_eq!(ids(&disk::open(&dir).unwrap()), ["1", "2", "4", "5", "6"]);
}
