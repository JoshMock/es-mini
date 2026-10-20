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
    std::fs::create_dir_all(dir)
        .and_then(|()| std::fs::write(&path, out.finish()))
        .map_err(|e| crate::Error(format!("{}: {e}", path.display())))
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
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
