//! Document routing against shards recorded from Elasticsearch 9.5.5
//! (fixtures/generate_shard_routing.sh). The fixture's indices include explicit and default
//! `routing_num_shards`, which 9.5.5-created indices ignore when routing.

use es_index::routing::shard_id;
use serde_json::Value;

#[test]
fn matches_elasticsearch_shard_assignments() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../fixtures/shard_routing.json"
    );
    let fixture: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let mut checked = 0;
    for index in fixture.as_array().unwrap() {
        let shards = index["settings"]["number_of_shards"].as_u64().unwrap() as u32;
        for case in index["cases"].as_array().unwrap() {
            let (id, routing) = (case["id"].as_str().unwrap(), case["routing"].as_str());
            let expected = case["shard"].as_u64().unwrap() as u32;
            assert_eq!(
                shard_id(id, routing, shards),
                expected,
                "{case} in {}",
                index["index"]
            );
            checked += 1;
        }
    }
    assert!(checked > 100);
}
