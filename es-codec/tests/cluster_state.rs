//! Index metadata of the Elasticsearch 9.5.5 fixture data directory (fixtures/generate.sh).

use es_codec::cluster_state::read_indices;
use std::path::PathBuf;

#[test]
fn reads_fixture_indices() {
    let data = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../fixtures/es-9.5.5/data");
    let indices = read_indices(&data).unwrap();
    let names: Vec<_> = indices.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(
        names,
        [
            ".ds-.logs-elasticsearch.deprecation-default-2026.10.16-000001",
            "fixture"
        ]
    );

    let fixture = &indices[1];
    assert_eq!(fixture.uuid, "D9r2xux8T1aNWB7zpBLShg");
    assert_eq!(fixture.settings["index.number_of_shards"], "1");
    assert_eq!(fixture.settings["index.number_of_replicas"], "0");
    assert_eq!(fixture.settings["index.uuid"], fixture.uuid);
    assert_eq!(
        fixture.mappings.as_deref(),
        Some(
            r#"{"_doc":{"properties":{"date":{"type":"date"},"published":{"type":"boolean"},"rating":{"type":"double"},"tag":{"type":"keyword"},"title":{"type":"text"},"views":{"type":"long"}}}}"#
        )
    );

    let ds = &indices[0];
    assert_eq!(ds.uuid, "45EQl4KeRciNIFMAKqBewg");
    assert!(
        ds.mappings
            .as_deref()
            .unwrap()
            .contains("dynamic_templates")
    );
}
