//! Writes a whole Elasticsearch 9.5.5 data directory (cluster state, node and shard files).

use crate::{
    Error, Result, SegmentInfos,
    cluster_state::{ALLOCATION_ID, GlobalMetadata, IndexMetadata, write_state},
    segment_infos::shard_user_data,
    state_file::{self, manifest_doc, node_doc, retention_leases_doc, shard_state_doc},
    translog::{Checkpoint, write_empty},
};
use std::path::Path;

const NODE_VERSION: i64 = 9_050_599;
const INDEX_VERSION: i64 = 9_111_000;
// Fixed 22-char ids; ES only requires uniqueness per shard.
const HISTORY_UUID: &str = "esminiHistoryUuid0000A";
const TRANSLOG_UUID: &str = "esminiTranslogUuid000A";

fn io(path: &Path) -> impl Fn(std::io::Error) -> Error + '_ {
    move |e| Error(format!("{}: {e}", path.display()))
}

/// Writes `data_dir` holding `indices`, each with one empty shard (no documents yet).
pub fn write_empty_data_dir(
    data_dir: &Path,
    global: &GlobalMetadata,
    indices: &[IndexMetadata],
) -> Result<()> {
    write_state(data_dir, global, indices)?;
    let state = data_dir.join("_state");
    state_file::write(
        &state,
        "node",
        0,
        &node_doc(&global.node_id, NODE_VERSION, INDEX_VERSION),
    )?;
    state_file::write(
        &state,
        "manifest",
        0,
        &manifest_doc(global.term as i64, 0, -1, &[]),
    )?;
    for ix in indices {
        let shard = data_dir.join("indices").join(&ix.uuid).join("0");
        let (index, translog) = (shard.join("index"), shard.join("translog"));
        for d in [&index, &translog] {
            std::fs::create_dir_all(d).map_err(io(d))?;
        }
        SegmentInfos {
            id: [1; 16],
            generation: 1,
            lucene_version: (10, 0, 0),
            index_created_major: 10,
            version: 1,
            counter: 0,
            segments: vec![],
            user_data: shard_user_data(HISTORY_UUID, TRANSLOG_UUID, -1, -1),
        }
        .write(&index)?;
        let cp = Checkpoint {
            generation: 1,
            global_checkpoint: -1,
            primary_term: 1,
        };
        let (tlog, ckp) = write_empty(TRANSLOG_UUID, &cp);
        for (name, bytes) in [("translog-1.tlog", tlog), ("translog.ckp", ckp)] {
            std::fs::write(translog.join(name), bytes).map_err(io(&translog))?;
        }
        let shard_state = shard.join("_state");
        state_file::write(
            &shard_state,
            "state",
            0,
            &shard_state_doc(&ix.uuid, ALLOCATION_ID),
        )?;
        state_file::write(
            &shard_state,
            "retention-leases",
            0,
            &retention_leases_doc(1),
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{cluster_state::read_indices, uid::encode_id};

    /// Set `ESM_WRITE_DIR` to keep the output for `scripts/cross_engine.sh`.
    /// Experiment: one doc "1" with only the meta fields; run cross_engine.sh get_1 on it.
    #[test]
    fn one_doc_segment() {
        use crate::{
            segment_writer::{NewSegment, es_meta_fields, write_segment},
            stored_fields::StoredValue,
            terms::TermPostings,
        };
        let Some(out) = std::env::var_os("ESM_WRITE_DIR1").map(std::path::PathBuf::from) else {
            return;
        };
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/es-9.5.5/data");
        let indices = read_indices(&fixture).unwrap();
        let global = GlobalMetadata {
            cluster_uuid: "esminiClusterUuid00A".into(),
            node_id: "esminiNodeId0000000000".into(),
            term: 1,
        };
        write_empty_data_dir(&out, &global, &indices).unwrap();
        let ix = indices.iter().find(|i| i.name == "fixture").unwrap();
        let dir = out.join("indices").join(&ix.uuid).join("0/index");
        let mut fields = es_meta_fields();
        fields[3].point_dimensions = 0;
        fields[3].point_index_dimensions = 0;
        fields[3].point_num_bytes = 0;
        let src = br#"{"title":"The quick brown fox","tag":"animal","views":10,"rating":4.5,"published":true,"date":"2024-01-01"}"#;
        let stored = [vec![
            (0, StoredValue::Bytes(encode_id("1"))),
            (1, StoredValue::Bytes(src.to_vec())),
        ]];
        let numeric = [(2, vec![1]), (3, vec![0]), (4, vec![1]), (5, vec![])];
        let terms = [(
            0,
            vec![TermPostings {
                term: encode_id("1"),
                total_term_freq: 1,
                docs: vec![(0, 1)],
                positions: vec![],
            }],
        )];
        let seg = write_segment(
            &dir,
            &NewSegment {
                name: "_0",
                id: [7; 16],
                max_doc: 1,
                fields: &fields,
                stored: &stored,
                numeric: &numeric,
                keyword: &[],
                norms: &[],
                terms: &terms,
            },
        )
        .unwrap();
        SegmentInfos {
            id: [1; 16],
            generation: 2,
            lucene_version: (10, 0, 0),
            index_created_major: 10,
            version: 2,
            counter: 1,
            segments: vec![seg],
            user_data: shard_user_data(HISTORY_UUID, TRANSLOG_UUID, 0, 0),
        }
        .write(&dir)
        .unwrap();
        // The safe commit is chosen by the translog's global checkpoint.
        let cp = Checkpoint {
            generation: 1,
            global_checkpoint: 0,
            primary_term: 1,
        };
        let (_, ckp) = write_empty(TRANSLOG_UUID, &cp);
        std::fs::write(dir.join("../translog/translog.ckp"), ckp).unwrap();
    }

    #[test]
    fn written_dir_reads_back() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/es-9.5.5/data");
        let indices = read_indices(&fixture).unwrap();
        let out = std::env::var_os("ESM_WRITE_DIR").map_or_else(
            || std::env::temp_dir().join(format!("esm-dd-{}", std::process::id())),
            Into::into,
        );
        let global = GlobalMetadata {
            cluster_uuid: "esminiClusterUuid00A".into(),
            node_id: "esminiNodeId0000000000".into(),
            term: 1,
        };
        write_empty_data_dir(&out, &global, &indices).unwrap();
        assert_eq!(read_indices(&out).unwrap(), indices);
        let state = state_file::read(&out.join("_state/node-0.st")).unwrap();
        assert_eq!(
            state,
            node_doc(&global.node_id, NODE_VERSION, INDEX_VERSION)
        );
        if std::env::var_os("ESM_WRITE_DIR").is_none() {
            std::fs::remove_dir_all(&out).unwrap();
        }
    }
}
