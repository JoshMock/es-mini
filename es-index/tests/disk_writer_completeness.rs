//! Saved segments hold every mapped field the way Elasticsearch writes it, so Elasticsearch
//! reading an es-mini data directory finds them.

use es_codec::{SegmentInfos, SegmentReader, stored_fields::StoredValue};
use es_index::{FieldMapping, Index, Indices, disk};
use serde_json::json;

mod common;
use common::Scratch;

/// Saves `idx` and opens the one segment it writes.
fn saved_segment(name: &str, idx: Index) -> (Scratch, SegmentReader) {
    let root = Scratch::new(&format!("completeness-{name}"), None);
    let dir = root.join("data");
    disk::save(&dir, &Indices::from([("idx".to_owned(), idx)])).unwrap();
    let uuid = &es_codec::cluster_state::read_indices(&dir).unwrap()[0].uuid;
    let shard = dir.join("indices").join(uuid).join("0/index");
    let seg = SegmentInfos::read_latest(&shard)
        .unwrap()
        .segments
        .remove(0);
    let r = SegmentReader::open(&shard, seg).unwrap();
    (root, r)
}

#[test]
fn disk_writer_completeness_stores_text_fields_mapped_with_store() {
    let mut idx = Index::default();
    let props = &mut idx.mappings.properties;
    props.insert("st".into(), FieldMapping::Text { store: true });
    props.insert("t".into(), FieldMapping::Text { store: false });
    idx.put(
        "1",
        json!({"st": ["Quick fox", "lazy dog"], "t": "not stored"}),
    );
    let (_root, r) = saved_segment("store", idx);
    let st = r.field("st").unwrap().number;
    let got: Vec<_> = (r.document(0).unwrap().into_iter())
        .filter(|(n, _)| *n == st)
        .map(|(_, v)| v)
        .collect();
    let want = ["Quick fox", "lazy dog"].map(|s| StoredValue::String(s.into()));
    assert_eq!(got, want);
    let t = r.field("t").unwrap().number;
    assert!(r.document(0).unwrap().iter().all(|(n, _)| *n != t));
}
