//! Index/document model, mappings, analysis, storage glue.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};

/// MVP field types; flat mappings only (no object/nested).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FieldMapping {
    Text,
    Keyword,
    Long,
    Integer,
    Short,
    Byte,
    Double,
    Float,
    Boolean,
    Date,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Mappings {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub properties: BTreeMap<String, FieldMapping>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Document {
    pub source: Value,
    pub version: u64,
}

#[derive(Debug, Default)]
pub struct Index {
    // ponytail: settings kept as raw JSON until something needs typed access
    pub settings: Value,
    pub mappings: Mappings,
    docs: HashMap<String, Document>,
}

impl Index {
    /// Returns (new version, created).
    // ponytail: no delete tombstones, so a re-created doc restarts at version 1 (ES continues the count)
    pub fn put(&mut self, id: &str, source: Value) -> (u64, bool) {
        let version = self.docs.get(id).map_or(1, |d| d.version + 1);
        let created = version == 1;
        self.docs
            .insert(id.to_owned(), Document { source, version });
        (version, created)
    }

    pub fn get(&self, id: &str) -> Option<&Document> {
        self.docs.get(id)
    }

    /// Returns the version recorded for the delete, if the doc existed.
    pub fn delete(&mut self, id: &str) -> Option<u64> {
        self.docs.remove(id).map(|d| d.version + 1)
    }
}

/// All indices, keyed by name.
pub type Indices = BTreeMap<String, Index>;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn mappings_round_trip() {
        let raw = json!({"properties": {
            "title": {"type": "text"},
            "tag": {"type": "keyword"},
            "n": {"type": "long"},
            "ok": {"type": "boolean"},
            "at": {"type": "date"}
        }});
        let m: Mappings = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(m.properties["tag"], FieldMapping::Keyword);
        assert_eq!(serde_json::to_value(&m).unwrap(), raw);
    }

    #[test]
    fn empty_mappings_serialize_as_empty_object() {
        assert_eq!(
            serde_json::to_value(Mappings::default()).unwrap(),
            json!({})
        );
    }

    #[test]
    fn unknown_field_type_is_rejected() {
        let r: Result<Mappings, _> =
            serde_json::from_value(json!({"properties": {"x": {"type": "nope"}}}));
        assert!(r.is_err());
    }

    #[test]
    fn document_put_get_delete() {
        let mut idx = Index::default();
        assert_eq!(idx.put("1", json!({"a": 1})), (1, true));
        assert_eq!(idx.put("1", json!({"a": 2})), (2, false));
        let doc = idx.get("1").unwrap();
        assert_eq!((doc.version, &doc.source), (2, &json!({"a": 2})));
        assert_eq!(idx.delete("1"), Some(3));
        assert!(idx.get("1").is_none());
        assert_eq!(idx.delete("1"), None);
    }
}
