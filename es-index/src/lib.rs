//! Index/document model, mappings, analysis, storage glue.

pub mod aliases;
pub mod analysis;
pub mod disk;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    time::{Duration, Instant},
};

/// MVP field types; flat mappings only (no object/nested).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FieldMapping {
    Text,
    /// Dynamically mapped string: `text` with a `keyword` subfield.
    #[serde(rename = "text", skip_deserializing)]
    TextKeyword {
        fields: BTreeMap<String, FieldMapping>,
    },
    Keyword,
    Long,
    Integer,
    Short,
    Byte,
    Double,
    Float,
    Boolean,
    Date {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        format: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        locale: Option<String>,
    },
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
        self.merge_with(json, false);
    }

    /// Like [`merge`](Self::merge); with `preserve_existing`, already-set keys are kept.
    pub fn merge_with(&mut self, json: &Value, preserve_existing: bool) {
        for (key, value) in flatten_settings(json) {
            let key = if key.starts_with("index.") {
                key
            } else {
                format!("index.{key}")
            };
            if preserve_existing && self.0.contains_key(&key) {
                continue;
            }
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

    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }

    /// The flat `index.*` keys and string values.
    pub fn flat(&self) -> &BTreeMap<String, String> {
        &self.0
    }

    /// Renders as nested JSON, e.g. `{"index": {"number_of_shards": "1"}}`.
    pub fn to_json(&self) -> Value {
        self.to_json_where(|_| true)
    }

    /// Like [`Settings::to_json`], keeping only keys for which `keep` is true.
    pub fn to_json_where(&self, keep: impl Fn(&str) -> bool) -> Value {
        let mut out = Value::Object(Default::default());
        for (key, value) in self.0.iter().filter(|(k, _)| keep(k)) {
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

/// Leaf values of nested settings JSON under dotted keys (`{"a": {"b": 1}}` → `a.b`).
pub fn flatten_settings(json: &Value) -> Vec<(String, &Value)> {
    let mut out = Vec::new();
    flatten("", json, &mut out);
    out
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
    pub seq_no: u64,
}

/// The `result` field of ES write responses.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DocResult {
    Created,
    Updated,
    Deleted,
    NotFound,
    Noop,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WriteResult {
    pub result: DocResult,
    pub version: u64,
    pub seq_no: u64,
}

/// Lucene term statistics read from disk. They keep counting documents that were updated or
/// deleted but not yet merged away, which BM25 scores depend on.
#[derive(Debug, Default)]
pub struct TermStats {
    /// Per indexed field: (docs with the field, sum of term frequencies).
    pub fields: HashMap<String, (f64, f64)>,
    pub doc_freq: HashMap<(String, String), f64>,
}

#[derive(Debug, Default)]
pub struct Index {
    /// Set for indices opened from disk; scoring uses it instead of counting live documents.
    pub stats: Option<TermStats>,
    pub settings: Settings,
    pub mappings: Mappings,
    pub aliases: BTreeMap<String, aliases::Alias>,
    /// Closed indices keep their metadata but reject document reads, writes, and searches.
    pub closed: bool,
    docs: HashMap<String, Document>,
    /// Sources of updated/deleted docs, kept for BM25 statistics like Lucene's unmerged segments.
    // ponytail: a "merge" drops them all once they pass a third of all physical docs, roughly
    // ES's default merge policy; real merges are per segment and less predictable
    superseded: Vec<Value>,
    /// Version and deletion time of deleted docs, kept for `index.gc_deletes` (default 60s) so
    /// a re-created doc continues the version count, as in ES.
    deleted_versions: HashMap<String, (u64, Instant)>,
    /// Tombstones in deletion order, so expired ones are pruned from the front.
    tombstone_order: VecDeque<(String, Instant)>,
    next_seq_no: u64,
    refreshed_seq_no: u64,
}

impl Index {
    fn last_version(&self, id: &str) -> u64 {
        self.docs
            .get(id)
            .map(|d| d.version)
            .or_else(|| self.deleted_versions.get(id).map(|t| t.0))
            .unwrap_or(0)
    }

    fn next_seq_no(&mut self) -> u64 {
        self.next_seq_no += 1;
        self.next_seq_no - 1
    }

    /// Adds dynamic mappings for fields of `source` not yet mapped.
    pub fn map_dynamic(&mut self, source: &Value) {
        // ponytail: top-level string fields only; no other dynamic types or nested objects
        let auto_text =
            self.settings.get("index.mapping.dynamic_strings.auto_text") != Some("false");
        if let Some(obj) = source.as_object() {
            for k in obj.iter().filter(|(_, v)| v.is_string()).map(|(k, _)| k) {
                self.mappings
                    .properties
                    .entry(k.clone())
                    .or_insert_with(|| {
                        if auto_text {
                            FieldMapping::TextKeyword {
                                fields: [("keyword".to_owned(), FieldMapping::Keyword)].into(),
                            }
                        } else {
                            FieldMapping::Keyword
                        }
                    });
            }
        }
    }

    /// Drops tombstones older than `index.gc_deletes`.
    fn prune_tombstones(&mut self) {
        let ttl = self
            .settings
            .get("index.gc_deletes")
            .and_then(parse_time)
            .unwrap_or(Duration::from_secs(60));
        let now = Instant::now();
        while let Some((id, at)) = self.tombstone_order.front() {
            if now.duration_since(*at) < ttl {
                break;
            }
            // A newer delete of the same id re-queued it; only its latest entry removes it.
            if self.deleted_versions.get(id).is_some_and(|t| t.1 == *at) {
                self.deleted_versions.remove(id);
            }
            self.tombstone_order.pop_front();
        }
    }

    /// Number of deleted docs whose versions are still remembered.
    pub fn tombstones(&self) -> usize {
        self.deleted_versions.len()
    }

    /// Creates or overwrites a document.
    pub fn put(&mut self, id: &str, source: Value) -> WriteResult {
        self.prune_tombstones();
        let version = self.last_version(id) + 1;
        let seq_no = self.next_seq_no();
        self.deleted_versions.remove(id);
        let old = self.docs.insert(
            id.to_owned(),
            Document {
                source,
                version,
                seq_no,
            },
        );
        let result = if let Some(old) = old {
            self.supersede(old.source);
            DocResult::Updated
        } else {
            DocResult::Created
        };
        WriteResult {
            result,
            version,
            seq_no,
        }
    }

    /// Creates a document; `Err(current_version)` if it already exists.
    pub fn create(&mut self, id: &str, source: Value) -> Result<WriteResult, u64> {
        match self.docs.get(id) {
            Some(doc) => Err(doc.version),
            None => Ok(self.put(id, source)),
        }
    }

    pub(crate) fn insert_loaded(&mut self, id: String, doc: Document) {
        self.next_seq_no = self.next_seq_no.max(doc.seq_no + 1);
        self.docs.insert(id, doc);
    }

    /// Makes all writes so far visible to search.
    pub fn refresh(&mut self) {
        self.refreshed_seq_no = self.next_seq_no;
    }

    // ponytail: only refresh_interval -1 delays visibility; otherwise writes are searchable at once
    pub fn searchable(&self, d: &Document) -> bool {
        self.settings.get("index.refresh_interval") != Some("-1")
            || d.seq_no < self.refreshed_seq_no
    }

    pub fn get(&self, id: &str) -> Option<&Document> {
        self.docs.get(id)
    }

    /// All live documents, in no particular order.
    pub fn docs(&self) -> impl Iterator<Item = (&str, &Document)> {
        self.docs.iter().map(|(id, d)| (id.as_str(), d))
    }

    fn supersede(&mut self, old: Value) {
        self.superseded.push(old);
        // superseded / (live + superseded) > 1/3
        if self.superseded.len() * 2 > self.docs.len() {
            self.superseded.clear();
        }
    }

    /// Sources of documents that were updated or deleted (see `superseded`).
    pub fn superseded(&self) -> &[Value] {
        &self.superseded
    }

    /// Deletes a document. Like ES, a missing document still consumes a version and seq_no.
    pub fn delete(&mut self, id: &str) -> WriteResult {
        self.prune_tombstones();
        let version = self.last_version(id) + 1;
        let seq_no = self.next_seq_no();
        let now = Instant::now();
        self.deleted_versions.insert(id.to_owned(), (version, now));
        self.tombstone_order.push_back((id.to_owned(), now));
        let result = match self.docs.remove(id) {
            Some(old) => {
                self.supersede(old.source);
                DocResult::Deleted
            }
            None => DocResult::NotFound,
        };
        WriteResult {
            result,
            version,
            seq_no,
        }
    }

    /// Merges `partial` into an existing document, or indexes `upsert` if it is missing.
    /// `None` means the document is missing and there is no upsert.
    pub fn update(
        &mut self,
        id: &str,
        partial: &Value,
        upsert: Option<Value>,
        detect_noop: bool,
    ) -> Option<WriteResult> {
        let Some(doc) = self.docs.get(id) else {
            return upsert.map(|source| self.put(id, source));
        };
        let mut source = doc.source.clone();
        merge(&mut source, partial);
        if detect_noop && source == doc.source {
            let (version, seq_no) = (doc.version, doc.seq_no);
            return Some(WriteResult {
                result: DocResult::Noop,
                version,
                seq_no,
            });
        }
        Some(self.put(id, source))
    }
}

fn merge(target: &mut Value, patch: &Value) {
    match (target, patch) {
        (Value::Object(t), Value::Object(p)) => {
            for (k, v) in p {
                merge(t.entry(k).or_insert(Value::Null), v);
            }
        }
        (t, p) => *t = p.clone(),
    }
}

/// Parses an ES time value (`250ms`, `60s`, `2m`, `1h`, `1d`); `None` for `-1` or junk.
pub fn parse_time(v: &str) -> Option<Duration> {
    let units = [
        ("ms", 1u64),
        ("s", 1_000),
        ("m", 60_000),
        ("h", 3_600_000),
        ("d", 86_400_000),
    ];
    units.iter().find_map(|(unit, millis)| {
        let n: u64 = v.strip_suffix(unit)?.parse().ok()?;
        Some(Duration::from_millis(n * millis))
    })
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

    fn w(result: DocResult, version: u64, seq_no: u64) -> WriteResult {
        WriteResult {
            result,
            version,
            seq_no,
        }
    }

    #[test]
    fn document_put_get_delete() {
        let mut idx = Index::default();
        assert_eq!(idx.put("1", json!({"a": 1})), w(DocResult::Created, 1, 0));
        assert_eq!(idx.put("1", json!({"a": 2})), w(DocResult::Updated, 2, 1));
        let doc = idx.get("1").unwrap();
        assert_eq!(
            (doc.version, doc.seq_no, &doc.source),
            (2, 1, &json!({"a": 2}))
        );
        assert_eq!(idx.delete("1"), w(DocResult::Deleted, 3, 2));
        assert!(idx.get("1").is_none());
        assert_eq!(idx.delete("1"), w(DocResult::NotFound, 4, 3));
    }

    #[test]
    fn superseded_docs_are_dropped_past_a_third_of_all_docs() {
        let mut idx = Index::default();
        for id in ["a", "b", "c"] {
            idx.put(id, json!({}));
        }
        idx.put("a", json!({"v": 2}));
        // 1 superseded of 4 physical docs: kept, like an unmerged segment.
        assert_eq!(idx.superseded().len(), 1);
        idx.delete("b");
        // 2 of 4 exceeds a third: "merged" away.
        assert_eq!(idx.superseded().len(), 0);
    }

    #[test]
    fn tombstones_expire_after_gc_deletes() {
        let mut idx = Index::default();
        idx.settings.merge(&json!({"index.gc_deletes": "0s"}));
        idx.put("1", json!({}));
        idx.delete("1");
        // The expired tombstone is forgotten, so the doc starts over like a new one.
        assert_eq!(idx.put("1", json!({})).version, 1);
        assert_eq!(idx.tombstones(), 0);
    }

    #[test]
    fn parses_es_time_values() {
        use std::time::Duration;
        assert_eq!(parse_time("60s"), Some(Duration::from_secs(60)));
        assert_eq!(parse_time("250ms"), Some(Duration::from_millis(250)));
        assert_eq!(parse_time("2m"), Some(Duration::from_secs(120)));
        assert_eq!(parse_time("1h"), Some(Duration::from_secs(3600)));
        assert_eq!(parse_time("1d"), Some(Duration::from_secs(86400)));
        assert_eq!(parse_time("-1"), None);
        assert_eq!(parse_time("soon"), None);
    }

    #[test]
    fn version_continues_after_delete() {
        let mut idx = Index::default();
        idx.put("1", json!({}));
        idx.delete("1");
        assert_eq!(idx.put("1", json!({})), w(DocResult::Created, 3, 2));
    }

    #[test]
    fn delete_of_never_seen_doc_is_version_1() {
        assert_eq!(Index::default().delete("x"), w(DocResult::NotFound, 1, 0));
    }

    #[test]
    fn create_conflicts_with_existing() {
        let mut idx = Index::default();
        assert_eq!(idx.create("1", json!({})), Ok(w(DocResult::Created, 1, 0)));
        assert_eq!(idx.create("1", json!({})), Err(1));
    }

    #[test]
    fn update_merges_recursively() {
        let mut idx = Index::default();
        idx.put("1", json!({"a": 1, "o": {"x": 1, "y": 1}}));
        let r = idx.update("1", &json!({"b": 2, "o": {"y": 2}}), None, true);
        assert_eq!(r, Some(w(DocResult::Updated, 2, 1)));
        assert_eq!(
            idx.get("1").unwrap().source,
            json!({"a": 1, "b": 2, "o": {"x": 1, "y": 2}})
        );
    }

    #[test]
    fn update_without_change_is_noop() {
        let mut idx = Index::default();
        idx.put("1", json!({"a": 1}));
        assert_eq!(
            idx.update("1", &json!({"a": 1}), None, true),
            Some(w(DocResult::Noop, 1, 0))
        );
    }

    #[test]
    fn update_without_detect_noop_bumps_version() {
        let mut idx = Index::default();
        idx.put("1", json!({"a": 1}));
        let r = idx.update("1", &json!({"a": 1}), None, false).unwrap();
        assert_eq!((r.result, r.version), (DocResult::Updated, 2));
    }

    #[test]
    fn update_missing_doc() {
        let mut idx = Index::default();
        assert_eq!(idx.update("1", &json!({"a": 1}), None, true), None);
        let r = idx.update("1", &json!({"a": 1}), Some(json!({"u": 1})), true);
        assert_eq!(r, Some(w(DocResult::Created, 1, 0)));
        assert_eq!(idx.get("1").unwrap().source, json!({"u": 1}));
    }
}
