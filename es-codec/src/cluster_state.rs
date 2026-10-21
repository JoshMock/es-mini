//! Index metadata from `<data>/_state`, the Lucene index written by `PersistedClusterStateService`.
//!
//! Each live document is one page of a blob: a global, per-index (`index_uuid`), or per-mapping
//! (`mapping_hash`) document. Blobs are `DFL\0` + raw deflate; index blobs hold SMILE, mapping
//! blobs hold a SMILE `{"content": <binary>}` wrapping deflated JSON.

use crate::{
    Result, SegmentInfos, SegmentReader, err,
    field_infos::{DocValuesType, FieldInfo, IndexOptions},
    inflate,
    segment_writer::{NewSegment, write_segment},
    sha256::sha256_base64,
    smile::{self, Value},
    stored_fields::StoredValue,
    terms::TermPostings,
};
use std::{collections::BTreeMap, path::Path};

#[derive(Debug, Clone, PartialEq)]
pub struct IndexMetadata {
    pub name: String,
    pub uuid: String,
    /// Flat setting keys, e.g. `index.number_of_shards`.
    pub settings: BTreeMap<String, String>,
    /// Mapping JSON as stored, e.g. `{"_doc":{"properties":{...}}}`; `None` if the index has none.
    pub mappings: Option<String>,
}

fn unframe(blob: &[u8]) -> Result<Vec<u8>> {
    match blob.strip_prefix(b"DFL\0") {
        Some(body) => inflate::inflate(body),
        None => err("cluster state blob is not DEFLATE-framed"),
    }
}

/// Joins a blob's pages, checking they are numbered from 0 and only the last is flagged `last_page`.
fn assemble(mut pages: Vec<(i32, bool, Vec<u8>)>) -> Result<Vec<u8>> {
    pages.sort_by_key(|p| p.0);
    let n = pages.len();
    for (i, (page, last, _)) in pages.iter().enumerate() {
        if *page != i as i32 || *last != (i + 1 == n) {
            return err("cluster state blob has a missing, duplicate or misordered page");
        }
    }
    unframe(&pages.into_iter().flat_map(|p| p.2).collect::<Vec<_>>())
}

/// Reads every index in `data_dir`, ordered by name.
pub fn read_indices(data_dir: &Path) -> Result<Vec<IndexMetadata>> {
    let dir = data_dir.join("_state");
    // (page, bytes) per index uuid / mapping hash
    let mut index_pages: BTreeMap<String, Vec<(i32, bool, Vec<u8>)>> = BTreeMap::new();
    let mut mapping_pages: BTreeMap<String, Vec<(i32, bool, Vec<u8>)>> = BTreeMap::new();
    for seg in SegmentInfos::read_latest(&dir)?.segments {
        let reader = SegmentReader::open(&dir, seg)?;
        let live = reader.live_docs()?;
        for (doc, fields) in reader.stored_fields()?.into_iter().enumerate() {
            if !live[doc] {
                continue;
            }
            let (mut key, mut page, mut last, mut data) = (None, 0, false, None);
            for (number, value) in fields {
                match (reader.field_by_number(number)?.name.as_str(), value) {
                    ("index_uuid", StoredValue::String(s)) => key = Some((true, s)),
                    ("mapping_hash", StoredValue::String(s)) => key = Some((false, s)),
                    ("page", StoredValue::Int(p)) => page = p,
                    ("last_page", StoredValue::Int(l)) => last = l != 0,
                    ("data", StoredValue::Bytes(b)) => data = Some(b),
                    _ => {}
                }
            }
            let (Some((is_index, key)), Some(data)) = (key, data) else {
                continue; // the global document
            };
            let pages = if is_index {
                &mut index_pages
            } else {
                &mut mapping_pages
            };
            pages.entry(key).or_default().push((page, last, data));
        }
    }
    let mut mappings = BTreeMap::new();
    for (hash, pages) in mapping_pages {
        let wrapper = assemble(pages)?;
        let Value::Bytes(content) = smile::decode(&wrapper)?
            .get("content")
            .cloned()
            .unwrap_or(Value::Null)
        else {
            return err(format!("mapping {hash} has no content"));
        };
        let json =
            String::from_utf8(unframe(&content)?).or_else(|_| err("mapping is not utf-8"))?;
        mappings.insert(hash, json);
    }
    let mut indices = Vec::new();
    for (uuid, pages) in index_pages {
        let Value::Object(top) = smile::decode(&assemble(pages)?)? else {
            return err(format!("index {uuid} metadata is not an object"));
        };
        let Some((name, meta)) = top.first() else {
            return err(format!("index {uuid} metadata is empty"));
        };
        let settings = match meta.get("settings") {
            Some(Value::Object(s)) => s
                .iter()
                // list settings (arrays) are not modelled and stay dropped
                .filter(|(_, v)| !matches!(v, Value::Array(_)))
                .map(|(k, v)| match v.as_str() {
                    Some(v) => Ok((k.clone(), v.to_owned())),
                    None => err(format!("index {uuid} setting {k} is not a string")),
                })
                .collect::<Result<_>>()?,
            _ => BTreeMap::new(),
        };
        let mappings = meta
            .get("mappings_hash")
            .and_then(Value::as_str)
            .and_then(|h| mappings.get(h).cloned());
        indices.push(IndexMetadata {
            name: name.clone(),
            uuid,
            settings,
            mappings,
        });
    }
    indices.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(indices)
}

/// Cluster-wide identity written into the global document and the commit user data.
pub struct GlobalMetadata {
    pub cluster_uuid: String,
    pub node_id: String,
    pub term: i32,
}

/// Writes a fresh `<data>/_state` Lucene index (inverse of [`read_indices`]): one single-page
/// document for the global metadata, one per index and one per distinct mapping. The `type`
/// field is indexed (not stored), as Elasticsearch finds each kind of document through it.
/// Allocation id of every index's shard 0 in written data directories.
pub const ALLOCATION_ID: &str = "esminiAllocationId00A";

pub fn write_state(
    data_dir: &Path,
    global: &GlobalMetadata,
    indices: &[IndexMetadata],
) -> Result<()> {
    let dir = data_dir.join("_state");
    std::fs::create_dir_all(&dir).map_err(|e| crate::Error(format!("{}: {e}", dir.display())))?;
    // Same field numbering as Elasticsearch; `type`, `mapping_hash`, `project_id` and `index_uuid`
    // are indexed (Elasticsearch finds each kind of document through them).
    let names = [
        "type",
        "page",
        "last_page",
        "data",
        "mapping_hash",
        "project_id",
        "index_uuid",
    ];
    let indexed = [0, 4, 5, 6];
    let postings = [("format", "Lucene104"), ("suffix", "0")]
        .map(|(k, v)| (format!("PerFieldPostingsFormat.{k}"), v.to_string()));
    let fields: Vec<_> = (0..7)
        .map(|n| FieldInfo {
            name: names[n as usize].into(),
            number: n,
            store_term_vector: false,
            omit_norms: true,
            store_payloads: false,
            soft_deletes: false,
            parent: false,
            index_options: if indexed.contains(&n) {
                IndexOptions::Docs
            } else {
                IndexOptions::None
            },
            doc_values: DocValuesType::None,
            doc_values_skip_index: false,
            dv_gen: -1,
            attributes: if indexed.contains(&n) {
                postings.iter().cloned().collect()
            } else {
                BTreeMap::new()
            },
            point_dimensions: 0,
            point_index_dimensions: 0,
            point_num_bytes: 0,
            vector_dimension: 0,
        })
        .collect();
    let doc = |key_field: i32, key: String, blob: Vec<u8>| {
        vec![
            (key_field, StoredValue::String(key)),
            (1, StoredValue::Int(0)),
            (3, StoredValue::Bytes(inflate::frame(&blob))),
            (5, StoredValue::String("default".into())),
            (2, StoredValue::Int(1)),
        ]
    };
    let node = || Value::Array(vec![Value::String(global.node_id.clone())]);
    let meta = Value::object(vec![
        ("version", Value::Int(1)),
        ("cluster_uuid", Value::String(global.cluster_uuid.clone())),
        ("cluster_uuid_committed", Value::Bool(true)),
        (
            "cluster_coordination",
            Value::object(vec![
                ("term", Value::Int(global.term as i64)),
                ("last_committed_config", node()),
                ("last_accepted_config", node()),
                ("voting_config_exclusions", Value::Array(vec![])),
            ]),
        ),
    ]);
    let mut stored = vec![vec![
        (1, StoredValue::Int(0)),
        (
            3,
            StoredValue::Bytes(inflate::frame(&smile::encode(&Value::object(vec![(
                "meta-data",
                meta,
            )])))),
        ),
        (2, StoredValue::Int(1)),
    ]];
    let mut kinds = vec!["global"];
    let mut mapping_docs = BTreeMap::new();
    for ix in indices {
        let mut meta = vec![(
            "settings".to_string(),
            Value::Object(
                ix.settings
                    .iter()
                    .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                    .collect(),
            ),
        )];
        if let Some(json) = &ix.mappings {
            let hash = sha256_base64(json.as_bytes());
            let content = Value::Bytes(inflate::frame(json.as_bytes()));
            let wrapper = Value::Object(vec![("content".into(), content)]);
            mapping_docs.insert(hash.clone(), smile::encode(&wrapper));
            meta.push(("mappings_hash".into(), Value::String(hash)));
        }
        // Without an in-sync id, Elasticsearch recovers the primary from an empty store.
        meta.push((
            "in_sync_allocations".into(),
            Value::Object(vec![(
                "0".into(),
                Value::Array(vec![Value::String(ALLOCATION_ID.into())]),
            )]),
        ));
        let top = Value::Object(vec![(ix.name.clone(), Value::Object(meta))]);
        stored.push(doc(6, ix.uuid.clone(), smile::encode(&top)));
        kinds.push("index");
    }
    for (hash, blob) in mapping_docs {
        stored.push(doc(4, hash, blob));
        kinds.push("mapping");
    }
    // Terms per indexed field, fields ordered by name; `keyed` maps a field to its stored values.
    let keyed = |field: i32| -> Vec<(String, u32)> {
        let mut v: Vec<_> = (0..stored.len() as u32)
            .filter_map(
                |d| match stored[d as usize].iter().find(|f| f.0 == field)? {
                    (_, StoredValue::String(k)) => Some((k.clone(), d)),
                    _ => None,
                },
            )
            .collect();
        v.sort();
        v
    };
    let postings = |entries: Vec<(String, u32)>| {
        TermPostings::from_sorted(entries.into_iter().map(|(t, d)| (t.into_bytes(), d)))
    };
    let mut type_entries: Vec<_> = (0..kinds.len())
        .map(|d| (kinds[d].to_string(), d as u32))
        .collect();
    type_entries.sort();
    let mut terms = vec![
        (6, postings(keyed(6))),
        (4, postings(keyed(4))),
        (5, postings(keyed(5))),
        (0, postings(type_entries)),
    ];
    terms.retain(|(_, t)| !t.is_empty());
    let seg = write_segment(
        &dir,
        &NewSegment {
            name: "_0",
            id: [1; 16],
            max_doc: stored.len() as i32,
            fields: &fields,
            stored: &stored,
            numeric: &[],
            sorted_numeric: &[],
            keyword: &[],
            norms: &[],
            terms: &terms,
            points: &[],
        },
    )?;
    SegmentInfos {
        id: [2; 16],
        generation: 1,
        lucene_version: crate::segment_infos::LUCENE_VERSION,
        index_created_major: 10,
        version: 1,
        counter: 1,
        segments: vec![seg],
        user_data: [
            ("current_term", global.term.to_string()),
            ("last_accepted_version", "1".into()),
            ("node_id", global.node_id.clone()),
            ("node_version", "9050599".into()),
            ("oldest_index_version", "9111000".into()),
            ("cluster_uuid", global.cluster_uuid.clone()),
            ("cluster_uuid_committed", "true".into()),
        ]
        .map(|(k, v)| (k.to_string(), v))
        .into(),
    }
    .write(&dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assemble_rejects_lost_pages() {
        assert!(assemble(vec![(0, false, vec![1])]).is_err());
        assert!(assemble(vec![(1, true, vec![1])]).is_err());
        assert!(assemble(vec![(0, true, vec![1]), (0, true, vec![1])]).is_err());
    }

    #[test]
    fn written_indices_read_back() {
        let data = std::env::temp_dir().join(format!("esm-cs-{}", std::process::id()));
        let indices = vec![
            IndexMetadata {
                name: "a".into(),
                uuid: "uuid-a".into(),
                settings: [("index.number_of_shards".to_string(), "1".to_string())].into(),
                mappings: Some(r#"{"_doc":{"properties":{"t":{"type":"text"}}}}"#.into()),
            },
            IndexMetadata {
                name: "b".into(),
                uuid: "uuid-b".into(),
                settings: BTreeMap::new(),
                mappings: None,
            },
        ];
        let global = GlobalMetadata {
            cluster_uuid: "cu".into(),
            node_id: "n".into(),
            term: 1,
        };
        write_state(&data, &global, &indices).unwrap();
        assert_eq!(read_indices(&data).unwrap(), indices);
        // the global doc is findable through the indexed `type` term
        let dir = data.join("_state");
        let seg = SegmentInfos::read_latest(&dir).unwrap().segments.remove(0);
        let terms = SegmentReader::open(&dir, seg)
            .unwrap()
            .terms("type")
            .unwrap();
        let global_term = terms.iter().find(|t| t.term == b"global").unwrap();
        assert_eq!(global_term.docs, vec![(0, 1)]);
        std::fs::remove_dir_all(&data).unwrap();
    }
}
