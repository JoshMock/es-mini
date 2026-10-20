//! Index metadata from `<data>/_state`, the Lucene index written by `PersistedClusterStateService`.
//!
//! Each live document is one page of a blob: a global, per-index (`index_uuid`), or per-mapping
//! (`mapping_hash`) document. Blobs are `DFL\0` + raw deflate; index blobs hold SMILE, mapping
//! blobs hold a SMILE `{"content": <binary>}` wrapping deflated JSON.

use crate::{
    Result, SegmentInfos, SegmentReader, err, inflate,
    smile::{self, Value},
    stored_fields::StoredValue,
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

/// Reads every index in `data_dir`, ordered by name.
pub fn read_indices(data_dir: &Path) -> Result<Vec<IndexMetadata>> {
    let dir = data_dir.join("_state");
    // (page, bytes) per index uuid / mapping hash
    let mut index_pages: BTreeMap<String, Vec<(i32, Vec<u8>)>> = BTreeMap::new();
    let mut mapping_pages: BTreeMap<String, Vec<(i32, Vec<u8>)>> = BTreeMap::new();
    for seg in SegmentInfos::read_latest(&dir)?.segments {
        let reader = SegmentReader::open(&dir, seg)?;
        let live = reader.live_docs()?;
        for (doc, fields) in reader.stored_fields()?.into_iter().enumerate() {
            if !live[doc] {
                continue;
            }
            let (mut key, mut page, mut data) = (None, 0, None);
            for (number, value) in fields {
                match (reader.field_by_number(number)?.name.as_str(), value) {
                    ("index_uuid", StoredValue::String(s)) => key = Some((true, s)),
                    ("mapping_hash", StoredValue::String(s)) => key = Some((false, s)),
                    ("page", StoredValue::Int(p)) => page = p,
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
            pages.entry(key).or_default().push((page, data));
        }
    }
    let assemble = |mut pages: Vec<(i32, Vec<u8>)>| {
        pages.sort_by_key(|p| p.0);
        unframe(&pages.into_iter().flat_map(|p| p.1).collect::<Vec<_>>())
    };
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
                .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned())))
                .collect(),
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
