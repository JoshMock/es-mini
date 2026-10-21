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

    /// Stamps what Elasticsearch records when it creates an index (`index.uuid`,
    /// `index.creation_date`, `index.version.created`, `index.provided_name`), keeping any
    /// values already present, e.g. from an Elasticsearch data directory.
    pub fn record_creation(&mut self, name: &str) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        for (key, value) in [
            ("index.uuid", random_uuid()),
            ("index.creation_date", now.as_millis().to_string()),
            ("index.version.created", VERSION_CREATED.to_owned()),
            ("index.provided_name", name.to_owned()),
        ] {
            self.0.entry(key.to_owned()).or_insert(value);
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
        // ponytail: top-level fields only; no nested objects, numeric_detection or ignore_above
        let auto_text =
            self.settings.get("index.mapping.dynamic_strings.auto_text") != Some("false");
        let Some(obj) = source.as_object() else {
            return;
        };
        for (k, v) in obj {
            // An array maps like its first non-null element.
            let v = match v {
                Value::Array(a) => a.iter().find(|e| !e.is_null()),
                v => Some(v),
            };
            let mapping = match v {
                Some(Value::String(s)) if is_strict_date(s) => FieldMapping::Date {
                    format: None,
                    locale: None,
                },
                Some(Value::String(_)) if auto_text => FieldMapping::TextKeyword {
                    fields: [("keyword".to_owned(), FieldMapping::Keyword)].into(),
                },
                Some(Value::String(_)) => FieldMapping::Keyword,
                Some(Value::Bool(_)) => FieldMapping::Boolean,
                Some(Value::Number(n)) if n.is_f64() => FieldMapping::Float,
                Some(Value::Number(_)) => FieldMapping::Long,
                _ => continue,
            };
            self.mappings.properties.entry(k.clone()).or_insert(mapping);
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

/// `index.version.created` of indices created by Elasticsearch 9.5.5 (read from the fixture).
pub const VERSION_CREATED: &str = "9111000";

/// A 22-character URL-safe base64 id like Elasticsearch's index UUIDs.
fn random_uuid() -> String {
    use std::hash::{BuildHasher, Hasher};
    const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    // RandomState seeds each hasher from OS randomness, so two of them make 128 random bits.
    let half = || {
        std::collections::hash_map::RandomState::new()
            .build_hasher()
            .finish()
    };
    let bits = (u128::from(half()) << 64) | u128::from(half());
    (0..22)
        .map(|i| B64[((bits >> (i * 6)) & 63) as usize] as char)
        .collect()
}

/// Formats epoch milliseconds as ES's `date_optional_time` (`2024-01-15T10:30:00.000Z`).
pub fn iso_date(millis: u64) -> String {
    let (days, ms) = (millis / 86_400_000, millis % 86_400_000);
    // Howard Hinnant's civil_from_days, for days since 1970-01-01.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let (h, m, sec, milli) = (ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000);
    format!("{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{sec:02}.{milli:03}Z")
}

/// Epoch milliseconds of a date value: a number, or `yyyy-MM-dd[THH:mm[:ss[.SSS]]][Z]` (UTC).
// ponytail: ES's default `strict_date_optional_time||epoch_millis` only, no zone offsets.
pub fn parse_date_millis(v: &serde_json::Value) -> Option<i64> {
    let Some(s) = v.as_str() else {
        return v.as_i64();
    };
    let (date, time) = s.split_once('T').unwrap_or((s, ""));
    let mut d = date.splitn(3, '-').map(|p| p.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    let time = time.strip_suffix('Z').unwrap_or(time);
    let (hms, frac) = time.split_once('.').unwrap_or((time, ""));
    let mut t = hms
        .split(':')
        .filter(|p| !p.is_empty())
        .map(|p| p.parse::<i64>().ok());
    let (h, min, sec) = (
        t.next().unwrap_or(Some(0))?,
        t.next().unwrap_or(Some(0))?,
        t.next().unwrap_or(Some(0))?,
    );
    let milli = format!("{frac:0<3}")[..3].parse::<i64>().ok()?;
    // Howard Hinnant's days_from_civil.
    let y = y - i64::from(m <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let days = era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468;
    Some(((days * 24 + h) * 60 + min) * 60_000 + sec * 1000 + milli)
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

/// Whether `s` matches ES's default dynamic date format `strict_date_optional_time`:
/// `yyyy[-MM[-dd[THH[:mm[:ss[.S+]]][Z|+HH:mm]]]]`.
fn is_strict_date(s: &str) -> bool {
    fn digits(s: &mut &str, n: usize) -> Option<()> {
        let (d, rest) = s.split_at_checked(n)?;
        d.bytes().all(|b| b.is_ascii_digit()).then(|| *s = rest)
    }
    fn eat(s: &mut &str, c: char) -> bool {
        s.strip_prefix(c).inspect(|r| *s = r).is_some()
    }
    fn parse(s: &mut &str) -> Option<()> {
        digits(s, 4)?;
        for sep in ['-', '-'] {
            if !eat(s, sep) {
                return Some(());
            }
            digits(s, 2)?;
        }
        if !eat(s, 'T') {
            return Some(());
        }
        digits(s, 2)?;
        if eat(s, ':') {
            digits(s, 2)?;
            if eat(s, ':') {
                digits(s, 2)?;
                if eat(s, '.') || eat(s, ',') {
                    let n = s.bytes().take_while(u8::is_ascii_digit).count();
                    (n > 0).then(|| *s = &s[n..])?;
                }
            }
        }
        if eat(s, 'Z') {
            return Some(());
        }
        if eat(s, '+') || eat(s, '-') {
            digits(s, 2)?;
            if eat(s, ':') {
                digits(s, 2)?;
            }
        }
        Some(())
    }
    let mut rest = s;
    parse(&mut rest).is_some() && rest.is_empty()
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
    fn dynamic_mapping_detects_types() {
        let mut i = Index::default();
        i.map_dynamic(&json!({
            "s": "hi", "n": 3, "f": 1.5, "b": true, "d": "2024-01-02T03:04:05Z",
            "d2": "2024-01", "notd": "2024-1-2", "arr": [null, 7], "nul": null
        }));
        let p = &i.mappings.properties;
        assert!(matches!(p["s"], FieldMapping::TextKeyword { .. }));
        assert_eq!(p["n"], FieldMapping::Long);
        assert_eq!(p["f"], FieldMapping::Float);
        assert_eq!(p["b"], FieldMapping::Boolean);
        assert!(matches!(p["d"], FieldMapping::Date { .. }));
        assert!(matches!(p["d2"], FieldMapping::Date { .. }));
        assert!(matches!(p["notd"], FieldMapping::TextKeyword { .. }));
        assert_eq!(p["arr"], FieldMapping::Long);
        assert!(!p.contains_key("nul"));
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
    fn parses_dates_back_to_epoch_millis() {
        use serde_json::json;
        for ms in [0u64, 1_705_314_600_123, 1_709_251_200_000] {
            assert_eq!(parse_date_millis(&json!(iso_date(ms))), Some(ms as i64));
        }
        assert_eq!(
            parse_date_millis(&json!("2024-01-01")),
            Some(1_704_067_200_000)
        );
        assert_eq!(parse_date_millis(&json!(5)), Some(5));
        assert_eq!(parse_date_millis(&json!("junk")), None);
    }

    #[test]
    fn formats_epoch_millis_as_iso_dates() {
        assert_eq!(iso_date(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso_date(1_792_182_985_033), "2026-10-16T20:36:25.033Z");
        assert_eq!(iso_date(951_782_400_000), "2000-02-29T00:00:00.000Z");
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
