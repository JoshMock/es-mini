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
            lucene_version: crate::segment_infos::LUCENE_VERSION,
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

    /// Writes the fixture's five live docs as one segment. Set `ESM_WRITE_DIR1` to keep the
    /// output, then run `scripts/cross_engine.sh` on it.
    #[test]
    fn fixture_docs_segment() {
        use crate::{
            field_infos::{DocValuesType, FieldInfo, IndexOptions},
            segment_writer::{NewSegment, es_meta_fields, write_segment},
            stored_fields::StoredValue,
            terms::TermPostings,
        };
        use std::collections::BTreeMap;
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
        // (id, title, tag, views, rating, published, date millis)
        let docs = [
            (
                "1",
                "The quick brown fox",
                "animal",
                10,
                4.5,
                true,
                1704067200000,
            ),
            (
                "2",
                "Quick quick rabbits",
                "animal",
                26,
                3.0,
                false,
                1707955200000,
            ),
            ("3", "Lazy dog sleeps", "pet", 3, 2.5, true, 1703980800000),
            (
                "4",
                "A brown dog and a fox",
                "pet",
                7,
                5.0,
                true,
                1710028800000,
            ),
            ("6", "Fresh fox news", "news", 42, 3.5, true, 1714867200000),
        ];
        let json = |d: &(&str, &str, &str, i64, f64, bool, i64)| {
            let date = [
                "2023-12-31",
                "2024-01-01",
                "2024-02-15",
                "2024-03-10",
                "2024-05-05",
            ]
            .into_iter()
            .zip([
                1703980800000,
                1704067200000,
                1707955200000,
                1710028800000,
                1714867200000,
            ])
            .find(|(_, m)| *m == d.6)
            .unwrap()
            .0;
            format!(
                r#"{{"date":"{date}","published":{},"rating":{:?},"tag":"{}","title":"{}","views":{}}}"#,
                d.5, d.4, d.2, d.1, d.3
            )
        };
        let mut fields = es_meta_fields();
        fields[3].point_dimensions = 0;
        fields[3].point_index_dimensions = 0;
        fields[3].point_num_bytes = 0;
        let meta = |i: usize| fields[i].clone();
        let user = |name: &str, number, index_options, doc_values, base: usize, omit_norms| {
            let mut attributes = BTreeMap::new();
            for (k, v) in &meta(base).attributes {
                attributes.insert(k.clone(), v.clone());
            }
            if index_options != IndexOptions::None {
                for k in ["format", "suffix"] {
                    let v = meta(0).attributes[&format!("PerFieldPostingsFormat.{k}")].clone();
                    attributes.insert(format!("PerFieldPostingsFormat.{k}"), v);
                }
            }
            FieldInfo {
                name: name.into(),
                number,
                omit_norms,
                index_options,
                doc_values,
                attributes,
                point_dimensions: 0,
                point_index_dimensions: 0,
                point_num_bytes: 0,
                ..meta(2)
            }
        };
        fields.extend([
            user(
                "title",
                6,
                IndexOptions::DocsAndFreqs,
                DocValuesType::None,
                0,
                false,
            ),
            user(
                "tag",
                7,
                IndexOptions::Docs,
                DocValuesType::SortedSet,
                2,
                true,
            ),
            user(
                "views",
                8,
                IndexOptions::None,
                DocValuesType::Numeric,
                2,
                false,
            ),
            user(
                "rating",
                9,
                IndexOptions::None,
                DocValuesType::Numeric,
                2,
                false,
            ),
            user(
                "published",
                10,
                IndexOptions::Docs,
                DocValuesType::Numeric,
                2,
                true,
            ),
            user(
                "date",
                11,
                IndexOptions::None,
                DocValuesType::Numeric,
                2,
                false,
            ),
        ]);
        let stored: Vec<_> = docs
            .iter()
            .map(|d| {
                vec![
                    (0, StoredValue::Bytes(encode_id(d.0))),
                    (1, StoredValue::Bytes(json(d).into_bytes())),
                ]
            })
            .collect();
        let n = docs.len();
        let numeric = [
            (2, vec![1; n]),
            (3, (0..n as i64).collect()),
            (4, vec![1; n]),
            (5, vec![]),
            (8, docs.iter().map(|d| d.3).collect()),
            (9, docs.iter().map(|d| d.4.to_bits() as i64).collect()),
            (10, docs.iter().map(|d| i64::from(d.5)).collect()),
            (11, docs.iter().map(|d| d.6).collect()),
        ];
        let keyword = [(
            7,
            docs.iter().map(|d| vec![d.2.as_bytes().to_vec()]).collect(),
        )];
        let postings = |mut m: BTreeMap<Vec<u8>, Vec<(u32, u32)>>| -> Vec<TermPostings> {
            std::mem::take(&mut m)
                .into_iter()
                .map(|(term, docs)| TermPostings::new(term, docs))
                .collect()
        };
        let mut ids = BTreeMap::new();
        let mut title = BTreeMap::<Vec<u8>, Vec<(u32, u32)>>::new();
        let mut tags = BTreeMap::new();
        let mut published = BTreeMap::new();
        let mut lengths = vec![];
        for (i, d) in docs.iter().enumerate() {
            let i = i as u32;
            ids.insert(encode_id(d.0), vec![(i, 1)]);
            tags.entry(d.2.as_bytes().to_vec())
                .or_insert_with(Vec::new)
                .push((i, 1));
            published
                .entry(if d.5 { b"T".to_vec() } else { b"F".to_vec() })
                .or_insert_with(Vec::new)
                .push((i, 1));
            let words: Vec<_> = d.1.split(' ').map(str::to_lowercase).collect();
            lengths.push(words.len() as i8);
            let mut freq = BTreeMap::<_, u32>::new();
            for w in words {
                *freq.entry(w.into_bytes()).or_default() += 1;
            }
            for (w, f) in freq {
                title.entry(w).or_default().push((i, f));
            }
        }
        let terms = [
            (0, postings(ids)),
            (6, postings(title)),
            (7, postings(tags)),
            (10, postings(published)),
        ];
        let seg = write_segment(
            &dir,
            &NewSegment {
                name: "_0",
                id: [7; 16],
                max_doc: n as i32,
                fields: &fields,
                stored: &stored,
                numeric: &numeric,
                sorted_numeric: &[],
                keyword: &keyword,
                norms: &[(6, lengths.into_iter().map(Some).collect())],
                terms: &terms,
                points: &[],
            },
        )
        .unwrap();
        let max_seq = n as i64 - 1;
        SegmentInfos {
            id: [1; 16],
            generation: 2,
            lucene_version: crate::segment_infos::LUCENE_VERSION,
            index_created_major: 10,
            version: 2,
            counter: 1,
            segments: vec![seg],
            user_data: shard_user_data(HISTORY_UUID, TRANSLOG_UUID, max_seq, max_seq),
        }
        .write(&dir)
        .unwrap();
        // The safe commit is chosen by the translog's global checkpoint.
        let cp = Checkpoint {
            generation: 1,
            global_checkpoint: max_seq,
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
