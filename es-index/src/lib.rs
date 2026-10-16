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

/// Index settings stored the way ES normalizes them: flat `index.*` keys with string values.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings(BTreeMap<String, String>);

const DEFAULT_SETTINGS: [(&str, &str); 2] = [
    ("index.number_of_shards", "1"),
    ("index.number_of_replicas", "1"),
];

impl Default for Settings {
    fn default() -> Self {
        Self(
            DEFAULT_SETTINGS
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
    }
}

impl Settings {
    /// Merges user-supplied settings (nested or dotted keys, `index.` prefix optional).
    /// A `null` value resets the key to its default.
    pub fn merge(&mut self, json: &Value) {
        let mut flat = Vec::new();
        flatten("", json, &mut flat);
        for (key, value) in flat {
            let key = if key.starts_with("index.") {
                key
            } else {
                format!("index.{key}")
            };
            match value {
                Value::Null => match DEFAULT_SETTINGS.iter().find(|(k, _)| *k == key) {
                    Some((_, v)) => self.0.insert(key, v.to_string()),
                    None => self.0.remove(&key),
                },
                Value::String(v) => self.0.insert(key, v.clone()),
                v => self.0.insert(key, v.to_string()),
            };
        }
    }

    /// Renders as nested JSON, e.g. `{"index": {"number_of_shards": "1"}}`.
    pub fn to_json(&self) -> Value {
        let mut out = Value::Object(Default::default());
        for (key, value) in &self.0 {
            let mut node = &mut out;
            for part in key.split('.') {
                // ponytail: a leaf that is also a branch (`a` and `a.b`) loses the leaf; ES rejects this
                if !node.is_object() {
                    *node = Value::Object(Default::default());
                }
                let Value::Object(map) = node else {
                    unreachable!()
                };
                node = map
                    .entry(part)
                    .or_insert_with(|| Value::Object(Default::default()));
            }
            *node = Value::String(value.clone());
        }
        out
    }
}

fn flatten<'a>(prefix: &str, json: &'a Value, out: &mut Vec<(String, &'a Value)>) {
    match json {
        Value::Object(map) => {
            for (k, v) in map {
                let key = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                flatten(&key, v, out);
            }
        }
        // ponytail: arrays are stringified, not expanded like ES's list settings
        v => out.push((prefix.to_owned(), v)),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Document {
    pub source: Value,
    pub version: u64,
}

#[derive(Debug, Default)]
pub struct Index {
    pub settings: Settings,
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
    fn settings_are_flattened_prefixed_and_stringified() {
        let mut s = Settings::default();
        s.merge(
            &json!({"number_of_shards": 3, "index": {"refresh_interval": "1s"}, "index.foo": true}),
        );
        assert_eq!(
            s.to_json(),
            json!({"index": {"number_of_shards": "3", "number_of_replicas": "1", "refresh_interval": "1s", "foo": "true"}})
        );
    }

    #[test]
    fn null_setting_resets_to_default() {
        let mut s = Settings::default();
        s.merge(&json!({"index.number_of_replicas": 0, "index.foo": "x"}));
        s.merge(&json!({"index.number_of_replicas": null, "index.foo": null}));
        assert_eq!(
            s.to_json(),
            json!({"index": {"number_of_shards": "1", "number_of_replicas": "1"}})
        );
    }

    #[test]
    fn leaf_and_branch_on_same_key_does_not_panic() {
        let mut s = Settings::default();
        s.merge(&json!({"a": "x", "a.b": "y"}));
        assert_eq!(s.to_json()["index"]["a"]["b"], "y");
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
