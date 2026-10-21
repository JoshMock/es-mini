//! Elasticsearch `MetadataStateFormat` files (`manifest-N.st`, `node-N.st`, shard `state-N.st`):
//! codec header `state` v2, SMILE format id (LE int 1), a SMILE document, checksum footer.

use crate::codec_util::{self, check_footer};
use crate::input::DataInput;
use crate::output::Output;
use crate::smile::{self, Value};
use crate::{Result, err};
use std::path::Path;

pub fn write(dir: &Path, prefix: &str, generation: i64, doc: &Value) -> Result<()> {
    let mut out = Output::default();
    out.be_int(codec_util::CODEC_MAGIC);
    out.string("state");
    out.be_int(2);
    out.bytes(&1i32.to_le_bytes());
    out.bytes(&smile::encode(doc));
    let path = dir.join(format!("{prefix}-{generation}.st"));
    std::fs::create_dir_all(dir).map_err(|e| crate::Error(format!("{}: {e}", path.display())))?;
    crate::write_file_atomic(&path, &out.finish())
}

fn obj(fields: Vec<(&str, Value)>) -> Value {
    Value::Object(
        fields
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
    )
}

/// `node-N.st` document.
pub fn node_doc(node_id: &str, node_version: i64, oldest_index_version: i64) -> Value {
    obj(vec![
        ("node_id", Value::String(node_id.into())),
        ("node_version", Value::Int(node_version)),
        ("oldest_index_version", Value::Int(oldest_index_version)),
    ])
}

/// Shard `state-N.st` document for a primary.
pub fn shard_state_doc(index_uuid: &str, allocation_id: &str) -> Value {
    obj(vec![
        ("primary", Value::Bool(true)),
        ("index_uuid", Value::String(index_uuid.into())),
        (
            "allocation_id",
            obj(vec![("id", Value::String(allocation_id.into()))]),
        ),
    ])
}

/// Shard `retention-leases-N.st` document with no leases.
pub fn retention_leases_doc(primary_term: i64) -> Value {
    obj(vec![
        ("primary_term", Value::Int(primary_term)),
        ("version", Value::Int(0)),
        ("leases", Value::Array(vec![])),
    ])
}

/// `manifest-N.st` document; `indices` are `(name, uuid, metadata generation)`.
pub fn manifest_doc(
    current_term: i64,
    cluster_state_version: i64,
    generation: i64,
    indices: &[(&str, &str, i64)],
) -> Value {
    let gens = indices
        .iter()
        .map(|(name, uuid, generation)| {
            obj(vec![
                (
                    "index",
                    obj(vec![
                        ("index_name", Value::String((*name).into())),
                        ("index_uuid", Value::String((*uuid).into())),
                    ]),
                ),
                ("generation", Value::Int(*generation)),
            ])
        })
        .collect();
    obj(vec![
        ("current_term", Value::Int(current_term)),
        ("cluster_state_version", Value::Int(cluster_state_version)),
        ("generation", Value::Int(generation)),
        ("index_generations", Value::Array(gens)),
    ])
}

pub fn read(path: &Path) -> Result<Value> {
    let file = std::fs::read(path).map_err(|e| crate::Error(format!("{}: {e}", path.display())))?;
    check_footer(&file)?;
    let mut input = DataInput::new(&file);
    if input.read_be_int()? != codec_util::CODEC_MAGIC {
        return err("bad state file magic");
    }
    input.read_string()?;
    input.read_be_int()?;
    input.read_int()?;
    smile::decode(&file[input.pos..file.len() - codec_util::FOOTER_LEN])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_fixture_and_round_trips() {
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/es-9.5.5/data/_state");
        let node = read(&fixture.join("node-0.st")).unwrap();
        assert!(node.get("node_id").is_some());
        let manifest = read(&fixture.join("manifest-0.st")).unwrap();

        let dir = std::env::temp_dir().join(format!("esm-sf-{}", std::process::id()));
        write(&dir, "manifest", 3, &manifest).unwrap();
        assert_eq!(read(&dir.join("manifest-3.st")).unwrap(), manifest);

        let data = fixture.parent().unwrap();
        let shard = data.join("indices/D9r2xux8T1aNWB7zpBLShg/0/_state");
        assert_eq!(manifest_doc(0, 0, -1, &[]), manifest);
        assert_eq!(node_doc("rQYqacfoQ7uXdXc7tkJ9vQ", 9050599, 9111000), node);
        assert_eq!(
            shard_state_doc("D9r2xux8T1aNWB7zpBLShg", "LZN8helzS5G-ZGRov0Geww"),
            read(&shard.join("state-0.st")).unwrap()
        );
        assert_eq!(
            retention_leases_doc(1),
            read(&shard.join("retention-leases-0.st")).unwrap()
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
