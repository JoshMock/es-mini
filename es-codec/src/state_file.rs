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

/// `node-N.st` document.
pub fn node_doc(node_id: &str, node_version: i64, oldest_index_version: i64) -> Value {
    Value::object(vec![
        ("node_id", Value::String(node_id.into())),
        ("node_version", Value::Int(node_version)),
        ("oldest_index_version", Value::Int(oldest_index_version)),
    ])
}

/// Shard `state-N.st` document for a primary.
pub fn shard_state_doc(index_uuid: &str, allocation_id: &str) -> Value {
    Value::object(vec![
        ("primary", Value::Bool(true)),
        ("index_uuid", Value::String(index_uuid.into())),
        (
            "allocation_id",
            Value::object(vec![("id", Value::String(allocation_id.into()))]),
        ),
    ])
}

/// Shard `retention-leases-N.st` document with no leases.
pub fn retention_leases_doc(primary_term: i64) -> Value {
    Value::object(vec![
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
            Value::object(vec![
                (
                    "index",
                    Value::object(vec![
                        ("index_name", Value::String((*name).into())),
                        ("index_uuid", Value::String((*uuid).into())),
                    ]),
                ),
                ("generation", Value::Int(*generation)),
            ])
        })
        .collect();
    Value::object(vec![
        ("current_term", Value::Int(current_term)),
        ("cluster_state_version", Value::Int(cluster_state_version)),
        ("generation", Value::Int(generation)),
        ("index_generations", Value::Array(gens)),
    ])
}

pub fn read(path: &Path) -> Result<Value> {
    let file = crate::read_file(path)?;
    check_footer(&file)?;
    let mut input = DataInput::new(&file);
    codec_util::check_header(&mut input, "state", 1, 2)?;
    let format = input.read_int()?;
    if format != 1 {
        return err(format!(
            "unsupported state file format id {format}, expected SMILE (1)"
        ));
    }
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

    #[test]
    fn rejects_bad_header() {
        let doc = node_doc("n", 1, 1);
        let dir = std::env::temp_dir().join(format!("esm-sf-bad-{}", std::process::id()));
        write(&dir, "node", 0, &doc).unwrap();
        let path = dir.join("node-0.st");
        let good = std::fs::read(&path).unwrap();
        // header: magic(4) + vint len(1) + "state"(5) + version(4) + format(4)
        for (offset, byte) in [(5, b'x'), (13, 9), (14, 2)] {
            let mut bad = good.clone();
            bad[offset] = byte;
            // re-seal the checksum so only the header is wrong
            let n = bad.len() - 8;
            let sum = (codec_util::crc32(&bad[..n]) as u64).to_be_bytes();
            bad[n..].copy_from_slice(&sum);
            std::fs::write(&path, &bad).unwrap();
            assert!(read(&path).is_err(), "offset {offset}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
