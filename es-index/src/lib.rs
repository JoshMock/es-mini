//! Index/document model, mappings, analysis, storage glue.

pub mod aliases;
pub mod analysis;
pub mod date_math;
pub mod disk;
pub mod filter_path;
pub mod index_settings;
pub mod mapping;
pub mod postings;
pub mod tz;

pub use mapping::{FieldMapping, Mappings};

use serde::Serialize;
use serde_json::Value;
use std::{
    borrow::Cow,
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque, hash_map::RandomState},
    hash::BuildHasher,
    sync::{Arc, Mutex, OnceLock, PoisonError},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// Index settings stored the way ES normalizes them: flat `index.*` keys whose values are
/// strings or lists of strings ([`setting_value`]).
#[derive(Debug, Clone, PartialEq)]
pub struct Settings(BTreeMap<String, Value>);

const DEFAULT_SETTINGS: [(&str, &str); 2] = [
    (index_settings::NUMBER_OF_SHARDS, "1"),
    (index_settings::NUMBER_OF_REPLICAS, "1"),
];

impl Default for Settings {
    fn default() -> Self {
        Self(
            DEFAULT_SETTINGS
                .iter()
                .map(|(k, v)| (k.to_string(), Value::from(*v)))
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
                    Some((_, v)) => self.0.insert(key, Value::from(*v)),
                    None => self.0.remove(&key),
                },
                v => self.0.insert(key, setting_value(v)),
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
            (index_settings::UUID, random_uuid()),
            (index_settings::CREATION_DATE, now.as_millis().to_string()),
            (index_settings::VERSION_CREATED, VERSION_CREATED.to_string()),
            ("index.provided_name", name.to_owned()),
        ] {
            self.0.entry(key.to_owned()).or_insert(value.into());
        }
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(Value::as_str)
    }

    /// The defaults of the registered settings ([`index_settings`]) this does not set.
    pub fn defaults(&self) -> Settings {
        Settings(
            index_settings::SETTINGS
                .iter()
                .filter(|s| !self.0.contains_key(s.key))
                .filter_map(|s| Some((s.key.to_owned(), s.default?.into())))
                .collect(),
        )
    }

    /// The flat `index.*` keys and their values.
    pub fn flat(&self) -> &BTreeMap<String, Value> {
        &self.0
    }

    /// Renders as nested JSON, e.g. `{"index": {"number_of_shards": "1"}}`.
    pub fn to_json(&self) -> Value {
        self.to_json_where(|_| true)
    }

    /// Like [`Settings::to_json`], keeping only keys for which `keep` is true.
    pub fn to_json_where(&self, keep: impl Fn(&str) -> bool) -> Value {
        let mut out = serde_json::Map::new();
        for (key, value) in self.0.iter().filter(|(k, _)| keep(k)) {
            insert_setting(&mut out, "", key, value.clone());
        }
        Value::Object(out)
    }
}

/// Settings.processSetting: nests a dotted key, keeping the rest of a key whose prefix is
/// already a leaf in dot notation beside it (`a` and `a.b` render as `{"a": .., "a.b": ..}`).
// ponytail: keys arrive sorted, so a leaf never follows its own branch (ES flattens that case)
fn insert_setting(map: &mut serde_json::Map<String, Value>, prefix: &str, key: &str, value: Value) {
    let Some((head, rest)) = key.split_once('.') else {
        map.insert(format!("{prefix}{key}"), value);
        return;
    };
    let head = format!("{prefix}{head}");
    match map.get_mut(&head) {
        Some(Value::Object(inner)) => insert_setting(inner, "", rest, value),
        Some(_) => insert_setting(map, &format!("{head}."), rest, value),
        None => {
            let mut inner = serde_json::Map::new();
            insert_setting(&mut inner, "", rest, value);
            map.insert(head, Value::Object(inner));
        }
    }
}

/// Whether Settings.fromXContent accepts every list in `json`: lists hold only strings, numbers
/// and booleans.
pub fn setting_lists_ok(json: &Value) -> bool {
    match json {
        Value::Object(m) => m.values().all(setting_lists_ok),
        Value::Array(items) => (items.iter())
            .all(|v| matches!(v, Value::String(_) | Value::Number(_) | Value::Bool(_))),
        _ => true,
    }
}

/// A settings value as Settings.fromXContent stores it: arrays stay lists of strings, other
/// values become strings. Lists are checked with [`setting_lists_ok`] first.
pub fn setting_value(v: &Value) -> Value {
    match v {
        Value::Array(items) => Value::Array(items.iter().map(setting_value).collect()),
        Value::String(_) => v.clone(),
        v => Value::String(v.to_string()),
    }
}

/// A settings value as `Settings.get` reads it: a list renders like Java's `List.toString`, an
/// object like `Map.toString`.
pub fn setting_string(v: &Value) -> String {
    match v {
        Value::Array(items) => {
            let items: Vec<String> = items.iter().map(setting_string).collect();
            format!("[{}]", items.join(", "))
        }
        Value::Object(m) => {
            let items: Vec<String> = m
                .iter()
                .map(|(k, v)| format!("{k}={}", setting_string(v)))
                .collect();
            format!("{{{}}}", items.join(", "))
        }
        Value::String(s) => s.clone(),
        v => v.to_string(),
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
#[derive(Debug, Default, Clone)]
pub struct TermStats {
    /// Per indexed field: (docs with the field, sum of term frequencies).
    pub fields: HashMap<String, (f64, f64)>,
    pub doc_freq: HashMap<(String, String), f64>,
}

#[derive(Debug, Default)]
pub struct Index {
    /// Statistics of the segments of an index opened from disk or last committed, deleted
    /// documents included; scoring adds those of the documents in memory not yet committed.
    pub stats: Option<TermStats>,
    pub settings: Settings,
    pub mappings: Mappings,
    pub aliases: BTreeMap<String, aliases::Alias>,
    /// Closed indices keep their metadata but reject document reads, writes, and searches.
    pub closed: bool,
    /// User data of the shard commit the index was opened from; saves keep its history markers.
    pub commit: BTreeMap<String, String>,
    /// Documents written since the index was opened; they replace any on disk. Shared with
    /// snapshots.
    docs: HashMap<String, Arc<Document>>,
    /// Postings over `docs`, built per field as queries need them (see [`postings`]).
    postings: std::sync::Mutex<postings::Postings>,
    /// Documents in memory with lower sequence numbers are in the segments, which `stats`
    /// already counts: a snapshot's copies of those on disk, or those of the last commit.
    persisted: u64,
    /// The segments of an index opened from disk, serving the documents not written since.
    disk: Option<disk::Shard>,
    /// Sources of updated/deleted docs that were in memory, kept for BM25 statistics like
    /// Elasticsearch's soft deletes, which merges keep above the last commit's checkpoint
    /// (`stats` counts those on disk). A commit hands them to its segments' statistics, from
    /// which merges drop them.
    // ponytail: never dropped in an index without a data directory, which never commits;
    // Elasticsearch drops them once a flush and a retention lease renewal let a merge do so.
    // A commit drops those superseded since the previous one, which no segment holds.
    superseded: Vec<Arc<Document>>,
    /// Version and deletion time of deleted docs, kept for `index.gc_deletes` (default 60s) so
    /// a re-created doc continues the version count, as in ES.
    deleted_versions: HashMap<String, (u64, Instant)>,
    /// Tombstones in deletion order, so expired ones are pruned from the front.
    tombstone_order: VecDeque<(String, Instant)>,
    next_seq_no: u64,
    refreshed_seq_no: u64,
    /// When `refreshed_seq_no` was last advanced; `None` before the first refresh.
    refreshed_at: Option<Instant>,
    /// Writes not yet in the translog (see [`disk::persist`]).
    unlogged: Vec<es_codec::translog::Op>,
    request_cache: std::sync::Mutex<RequestCache>,
}

/// The request cache's cap, `indices.requests.cache.size`: Elasticsearch's default of 1% of
/// the heap, for a 1 GiB heap.
// ponytail: each index is capped on its own, where ES caps all of a node's together; es-mini
// has no heap or node settings to size it from
pub const REQUEST_CACHE_BYTES: usize = (1 << 30) / 100;

/// Elasticsearch's shard request cache (see [`Index::cached`]).
#[derive(Debug, Default)]
struct RequestCache {
    hits: u64,
    misses: u64,
    evictions: u64,
    /// Serialized size of the keys and entries.
    bytes: usize,
    /// The visible sequence number and mappings the entries were computed against.
    reader: Option<(u64, Mappings)>,
    /// Entries with the tick of their last use.
    entries: HashMap<String, (Value, u64)>,
    /// Keys by the tick of their last use, least recent first.
    recency: BTreeMap<u64, String>,
    tick: u64,
}

impl Index {
    fn last_version(&self, id: &str) -> u64 {
        self.meta(id)
            .map(|m| m.0)
            .or_else(|| self.deleted_versions.get(id).map(|t| t.0))
            .unwrap_or(0)
    }

    /// The version and sequence number of a live document, without reading its source.
    pub(crate) fn meta(&self, id: &str) -> Option<(u64, u64)> {
        match self.docs.get(id) {
            Some(d) => Some((d.version, d.seq_no)),
            None => self.disk.as_ref()?.meta(id).map(|d| (d.version, d.seq_no)),
        }
    }

    /// Removes a live document, returning it if no segment holds it: one in a segment stays
    /// counted in its statistics, as a deleted document does in Lucene.
    fn take(&mut self, id: &str) -> Option<Option<Arc<Document>>> {
        if let Some(d) = self.docs.remove(id) {
            self.postings
                .get_mut()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(id);
            return Some((d.seq_no >= self.persisted).then_some(d));
        }
        self.disk.as_mut()?.remove(id).map(|_| None)
    }

    fn next_seq_no(&mut self) -> u64 {
        // The scheduled refresh that came due since the last one, run late but before this write.
        if self.refresh_due() {
            self.refresh();
        }
        self.next_seq_no += 1;
        self.next_seq_no - 1
    }

    /// Drops tombstones older than `index.gc_deletes`.
    fn prune_tombstones(&mut self) {
        let ttl = self
            .settings
            .get(index_settings::GC_DELETES)
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
        self.put_auto(id, source, -1)
    }

    /// Like [`Index::put`] for an id Elasticsearch generated at `auto_id_timestamp` (-1: none).
    pub fn put_auto(&mut self, id: &str, source: Value, auto_id_timestamp: i64) -> WriteResult {
        self.prune_tombstones();
        let version = self.last_version(id) + 1;
        let seq_no = self.next_seq_no();
        self.unlogged.push(es_codec::translog::Op::Index {
            id: id.to_owned(),
            source: serde_json::to_vec(&source).unwrap_or_default(),
            seq_no: seq_no as i64,
            version: version as i64,
            primary_term: 1,
            routing: None,
            auto_id_timestamp,
        });
        let result = self.store(id, source, version, seq_no);
        WriteResult {
            result,
            version,
            seq_no,
        }
    }

    /// Stores a document at a given version and sequence number.
    fn store(&mut self, id: &str, source: Value, version: u64, seq_no: u64) -> DocResult {
        self.deleted_versions.remove(id);
        let old = self.take(id);
        self.postings
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .add(id, &source);
        self.docs.insert(
            id.to_owned(),
            Arc::new(Document {
                source,
                version,
                seq_no,
            }),
        );
        match old {
            Some(old) => {
                self.superseded.extend(old);
                DocResult::Updated
            }
            None => DocResult::Created,
        }
    }

    /// Writes not yet in the translog, oldest first; taking them marks them logged.
    pub fn take_unlogged(&mut self) -> Vec<es_codec::translog::Op> {
        std::mem::take(&mut self.unlogged)
    }

    /// Re-applies an operation from the translog with its recorded version and sequence number.
    pub(crate) fn replay(&mut self, op: es_codec::translog::Op) {
        use es_codec::translog::Op;
        self.next_seq_no = self.next_seq_no.max(op.seq_no() as u64 + 1);
        match op {
            Op::Index {
                id,
                source,
                seq_no,
                version,
                ..
            } => {
                let source = serde_json::from_slice(&source).unwrap_or(Value::Null);
                self.store(&id, source, version as u64, seq_no as u64);
            }
            Op::Delete { id, version, .. } => {
                self.remove(&id, version as u64);
            }
            Op::NoOp { .. } => {}
        }
    }

    /// Creates a document; `Err(current_version)` if it already exists.
    pub fn create(&mut self, id: &str, source: Value) -> Result<WriteResult, u64> {
        self.create_auto(id, source, -1)
    }

    /// Like [`Index::create`] for a generated id; see [`Index::put_auto`].
    pub fn create_auto(
        &mut self,
        id: &str,
        source: Value,
        auto_id_timestamp: i64,
    ) -> Result<WriteResult, u64> {
        match self.meta(id) {
            Some((version, _)) => Err(version),
            None => Ok(self.put_auto(id, source, auto_id_timestamp)),
        }
    }

    /// A copy for point-in-time searches: later writes to `self` do not change it. It shares
    /// the documents and segments, which stay readable after a save deletes their files.
    // ponytail: copies the ids and versions of the documents (not their sources); memory grows
    // with the document count times open snapshots, where Lucene shares per-segment live docs
    pub fn snapshot(&self) -> Index {
        let mut settings = self.settings.clone();
        // it sees the writes visible now, and is never refreshed
        settings.merge(&serde_json::json!({ index_settings::REFRESH_INTERVAL: "-1" }));
        Index {
            stats: self.stats.clone(),
            settings,
            mappings: self.mappings.clone(),
            docs: self.docs.clone(),
            disk: self.disk.clone(),
            superseded: self.superseded.clone(),
            persisted: self.persisted,
            next_seq_no: self.next_seq_no,
            refreshed_seq_no: if self.refresh_due() {
                self.next_seq_no
            } else {
                self.refreshed_seq_no
            },
            refreshed_at: self.refreshed_at,
            ..Index::default()
        }
    }

    /// The result of `compute` for `key`, reused until the visible documents or the mappings
    /// change, like Elasticsearch's shard request cache. Past [`REQUEST_CACHE_BYTES`] it evicts
    /// the least recently used entries.
    pub fn cached<E>(
        &self,
        key: String,
        compute: impl FnOnce() -> Result<Value, E>,
    ) -> Result<Value, E> {
        let visible = if self.refresh_due() {
            self.next_seq_no
        } else {
            self.refreshed_seq_no
        };
        let lock = || {
            self.request_cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        };
        {
            let mut c = lock();
            if !c
                .reader
                .as_ref()
                .is_some_and(|r| r.0 == visible && r.1 == self.mappings)
            {
                c.reader = Some((visible, self.mappings.clone()));
                c.entries.clear();
                c.recency.clear();
                c.bytes = 0;
            }
            c.tick += 1;
            let tick = c.tick;
            if let Some((v, used)) = c.entries.get_mut(&key) {
                let (v, last) = (v.clone(), std::mem::replace(used, tick));
                c.recency.remove(&last);
                c.recency.insert(tick, key);
                c.hits += 1;
                return Ok(v);
            }
            c.misses += 1;
        }
        // Computed unlocked so that other searches of this index are not held up.
        let v = compute()?;
        let mut c = lock();
        if c.reader
            .as_ref()
            .is_some_and(|r| r.0 == visible && r.1 == self.mappings)
        {
            c.tick += 1;
            let tick = c.tick;
            c.bytes += key.len() + v.to_string().len();
            c.recency.insert(tick, key.clone());
            // another search may have computed it at the same time
            if let Some((old, last)) = c.entries.insert(key.clone(), (v.clone(), tick)) {
                c.recency.remove(&last);
                c.bytes -= key.len() + old.to_string().len();
            }
            while c.bytes > REQUEST_CACHE_BYTES {
                let Some((_, k)) = c.recency.pop_first() else {
                    break;
                };
                let (old, _) = c.entries.remove(&k).expect("recency lists cached keys");
                c.bytes -= k.len() + old.to_string().len();
                c.evictions += 1;
            }
        }
        Ok(v)
    }

    /// The request cache's (hit count, miss count, eviction count, size of its entries in
    /// bytes).
    pub fn request_cache_stats(&self) -> (u64, u64, u64, usize) {
        let c = self
            .request_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (c.hits, c.misses, c.evictions, c.bytes)
    }

    /// Makes all writes so far visible to search.
    pub fn refresh(&mut self) {
        self.refreshed_seq_no = self.next_seq_no;
        self.refreshed_at = Some(Instant::now());
    }

    /// Whether a scheduled refresh has come due since the last refresh, making every write
    /// visible. Writes run one that came due first, so the writes since all precede it.
    // ponytail: no refresh_interval set means searchable at once, as a search of an idle
    // Elasticsearch shard waits for its refresh; `-1` never refreshes on its own
    fn refresh_due(&self) -> bool {
        let Some(interval) = self.settings.get(index_settings::REFRESH_INTERVAL) else {
            return true;
        };
        match index_settings::time_millis(interval, index_settings::REFRESH_INTERVAL) {
            Ok(ms) if ms >= 0 => self
                .refreshed_at
                .is_none_or(|at| at.elapsed() >= Duration::from_millis(ms as u64)),
            _ => false,
        }
    }

    /// Whether the write with `seq_no` is visible to search.
    pub fn searchable(&self, seq_no: u64) -> bool {
        seq_no < self.refreshed_seq_no || self.refresh_due()
    }

    /// A live document; one on disk is read from its segment.
    pub fn get(&self, id: &str) -> Option<Cow<'_, Document>> {
        if let Some(d) = self.docs.get(id) {
            return Some(Cow::Borrowed(&**d));
        }
        let disk = self.disk.as_ref()?;
        // ponytail: open checksums the segments, so only an I/O error since (e.g. a deleted
        // file) leaves a document unreadable; it is then reported missing
        disk.load(&disk.meta(id)?).ok().map(Cow::Owned)
    }

    /// All live documents, those on disk first, then those in memory in indexing order (as ES
    /// visits older segments first); those on disk are read as iterated.
    pub fn docs(&self) -> impl Iterator<Item = (&str, Cow<'_, Document>)> {
        self.docs_at(None, None)
    }

    /// The live documents in memory with ids in `in_memory`, and those on disk at `on_disk`
    /// (all of either if `None`).
    pub fn docs_at<'a: 'k, 'k>(
        &'a self,
        in_memory: Option<&'k std::collections::HashSet<String>>,
        on_disk: Option<&'k BTreeSet<disk::DiskPos>>,
    ) -> impl Iterator<Item = (&'a str, Cow<'a, Document>)> + 'k {
        let keep = move |p| on_disk.is_none_or(|at| at.contains(&p));
        let on_disk = self.disk.iter().flat_map(move |d| d.docs(keep));
        let mut memory: Vec<(&'a String, &'a Arc<Document>)> = match in_memory {
            // Few candidates: look them up instead of walking every document.
            Some(ids) => ids
                .iter()
                .filter_map(|id| self.docs.get_key_value(id))
                .collect(),
            None => self.docs.iter().collect(),
        };
        // results that depend on the visiting order (floating-point sums) are then deterministic
        memory.sort_unstable_by_key(|(_, d)| d.seq_no);
        on_disk
            // ponytail: open checksums the segments; documents an I/O error since makes
            // unreadable are skipped
            .filter_map(Result::ok)
            .map(|(id, d)| (id, Cow::Owned(d)))
            .chain(
                memory
                    .into_iter()
                    .map(|(id, d)| (id.as_str(), Cow::Borrowed(&**d))),
            )
    }

    /// Every live document, without reading it: its position on disk or its id in memory.
    pub fn live(&self) -> (HashSet<disk::DiskPos>, HashSet<&str>) {
        let on_disk = self.disk_live().map(|(p, _)| p).collect();
        (on_disk, self.docs.keys().map(String::as_str).collect())
    }

    /// Where a live document on disk is; `None` for one in memory or missing.
    pub fn disk_pos(&self, id: &str) -> Option<disk::DiskPos> {
        self.disk.as_ref()?.pos(id)
    }

    /// Every live document on disk, without reading it: its position, id and sequence number.
    pub fn disk_live(&self) -> impl Iterator<Item = (disk::DiskPos, (&str, u64))> {
        self.disk.iter().flat_map(disk::Shard::positions)
    }

    /// The on-disk documents that may index `term` in `field`, from its postings (see
    /// [`docs_at`](Self::docs_at)); `_id` is looked up directly. With `exact`, only those that
    /// do, or `None` if some segment has no postings for the field to read.
    pub fn disk_term(
        &self,
        field: &str,
        term: &str,
        exact: bool,
    ) -> Option<BTreeSet<disk::DiskPos>> {
        self.disk.as_ref().map_or_else(
            || Some(BTreeSet::new()),
            |d| d.term_docs(field, term, exact),
        )
    }

    /// The on-disk documents with a value of numeric or date `field` that `place` puts in a
    /// range (`Equal`), from its points tree or else its doc values (dates as epoch millis);
    /// `None` for other fields. `place` must be monotone: `Less` below the range, `Greater`
    /// above. With `exact`, only those that have one, or `None` if some segment has neither to
    /// read. Floats are their f32 values, so `place` compares them with float semantics.
    pub fn disk_numbers(
        &self,
        field: &str,
        place: impl Fn(&Value) -> Ordering,
        exact: bool,
    ) -> Option<BTreeSet<disk::DiskPos>> {
        use FieldMapping as M;
        let decode = match self.mappings.properties.get(field)? {
            M::Long | M::Integer | M::Short | M::Byte | M::Date { .. } => Value::from,
            M::Double => |v| Value::from(sortable_double(v)),
            M::Float => |v| Value::from(sortable_float(v)),
            _ => return None,
        };
        self.disk.as_ref().map_or_else(
            || Some(BTreeSet::new()),
            |d| d.number_docs(field, |v| place(&decode(v)), exact),
        )
    }

    /// Reads the doc values of `field` for live on-disk documents by id: numbers (dates as
    /// epoch millis), booleans, or keyword terms and ips in order. `None` for a document in memory, or
    /// in a segment without doc values for the field. Each segment's column is decoded once.
    pub fn disk_values<'a>(
        &'a self,
        field: &'a str,
    ) -> impl FnMut(&str) -> Option<Vec<Value>> + 'a {
        use FieldMapping as M;
        use es_codec::doc_values::DocValues as D;
        let mut columns: HashMap<usize, Option<D>> = HashMap::new();
        move |id| {
            let disk = self.disk.as_ref()?;
            let (seg, doc) = disk.pos(id)?;
            let column = &*columns
                .entry(seg)
                .or_insert_with(|| disk.column(seg, field));
            let numbers = |f: fn(i64) -> Value| match column {
                Some(D::Numbers(n)) => Some(n[doc].iter().map(|v| f(*v)).collect()),
                _ => None,
            };
            match self.mappings.properties.get(field)? {
                M::Long | M::Integer | M::Short | M::Byte | M::Date { .. } => numbers(Value::from),
                M::Double => numbers(|v| Value::from(sortable_double(v))),
                M::Float => numbers(|v| Value::from(sortable_float(v))),
                M::HalfFloat => numbers(|v| {
                    let s = v as i16;
                    Value::from(f64::from(half_float(s ^ ((s >> 15) & 0x7fff))))
                }),
                M::Boolean => numbers(|v| Value::Bool(v != 0)),
                M::Keyword { .. } => match column {
                    Some(D::Ordinals { ords, terms }) => Some(
                        (ords[doc].iter())
                            .map(|o| String::from_utf8_lossy(&terms[*o]).into_owned().into())
                            .collect(),
                    ),
                    _ => None,
                },
                M::Ip => match column {
                    Some(D::Ordinals { ords, terms }) => (ords[doc].iter())
                        .map(|o| Some(ip_string(terms[*o].as_slice().try_into().ok()?).into()))
                        .collect(),
                    _ => None,
                },
                _ => None,
            }
        }
    }

    /// Takes the statistics of `segments`, just committed with every write so far (`None`
    /// without segments, so scoring keeps using the postings of new documents alone).
    pub(crate) fn committed(&mut self, segments: Option<TermStats>) {
        self.stats = segments;
        self.superseded.clear();
        self.persisted = self.next_seq_no;
        // Rebuilt as queries need them, now counting no document committed.
        *self
            .postings
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Default::default();
    }

    /// In-memory documents that were updated or deleted (see `superseded`).
    pub fn superseded(&self) -> &[Arc<Document>] {
        &self.superseded
    }

    /// Deletes a document. Like ES, a missing document still consumes a version and seq_no.
    pub fn delete(&mut self, id: &str) -> WriteResult {
        self.prune_tombstones();
        let version = self.last_version(id) + 1;
        let seq_no = self.next_seq_no();
        self.unlogged.push(es_codec::translog::Op::Delete {
            id: id.to_owned(),
            seq_no: seq_no as i64,
            version: version as i64,
            primary_term: 1,
        });
        let result = self.remove(id, version);
        WriteResult {
            result,
            version,
            seq_no,
        }
    }

    /// Deletes a document, remembering `version` for `index.gc_deletes`.
    fn remove(&mut self, id: &str, version: u64) -> DocResult {
        let now = Instant::now();
        self.deleted_versions.insert(id.to_owned(), (version, now));
        self.tombstone_order.push_back((id.to_owned(), now));
        match self.take(id) {
            Some(old) => {
                self.superseded.extend(old);
                DocResult::Deleted
            }
            None => DocResult::NotFound,
        }
    }

    /// Merges `partial` into an existing document, or indexes `upsert` if it is missing,
    /// mapping new fields as [`Index::map_dynamic`] does (whose error it returns). `None` means
    /// the document is missing and there is no upsert.
    pub fn update(
        &mut self,
        id: &str,
        partial: &Value,
        upsert: Option<Value>,
        detect_noop: bool,
    ) -> Result<Option<WriteResult>, String> {
        let Some(doc) = self.get(id) else {
            let Some(source) = upsert else {
                return Ok(None);
            };
            self.map_dynamic(id, &source)?;
            return Ok(Some(self.put(id, source)));
        };
        let mut source = doc.source.clone();
        merge(&mut source, partial);
        if detect_noop && source == doc.source {
            let (version, seq_no) = (doc.version, doc.seq_no);
            return Ok(Some(WriteResult {
                result: DocResult::Noop,
                version,
                seq_no,
            }));
        }
        drop(doc);
        self.map_dynamic(id, &source)?;
        Ok(Some(self.put(id, source)))
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
pub const VERSION_CREATED: i64 = es_codec::segment_infos::INDEX_VERSION_ID;

/// An id from Elasticsearch's `TimeBasedUUIDGenerator` (15 bytes, URL-safe base64 without
/// padding) and the millisecond timestamp it embeds.
pub fn auto_id() -> (String, i64) {
    static STATE: Mutex<(u32, i64)> = Mutex::new((0, 0));
    static MAC: OnceLock<[u8; 6]> = OnceLock::new();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    let (seq, ts) = {
        let mut st = STATE.lock().unwrap_or_else(PoisonError::into_inner);
        st.0 = st.0.wrapping_add(1) & 0xff_ffff;
        st.1 = st.1.max(now) + i64::from(st.0 == 0);
        *st
    };
    // ponytail: random per-process node bytes stand in for the munged MAC address
    let mac = MAC.get_or_init(|| {
        let r = RandomState::new().hash_one(0u8).to_be_bytes();
        [r[0] | 1, r[1], r[2], r[3], r[4], r[5]]
    });
    let (s, t) = (seq.to_be_bytes(), ts.to_be_bytes());
    let bytes = [
        s[3], s[1], t[5], t[4], t[3], t[2], mac[0], mac[1], mac[2], mac[3], mac[4], mac[5], t[6],
        s[2], t[7],
    ];
    (
        es_codec::base64::encode(&bytes, es_codec::base64::URL, false),
        ts,
    )
}

/// A 22-character URL-safe base64 id like Elasticsearch's index UUIDs.
fn random_uuid() -> String {
    use std::hash::{BuildHasher, Hasher};
    // RandomState seeds each hasher from OS randomness, so two of them make 128 random bits.
    let half = || {
        std::collections::hash_map::RandomState::new()
            .build_hasher()
            .finish()
    };
    let bits = (u128::from(half()) << 64) | u128::from(half());
    (0..22)
        .map(|i| es_codec::base64::URL[((bits >> (i * 6)) & 63) as usize] as char)
        .collect()
}

/// A double from its sortable-long doc value: flipping the non-sign bits of negatives is its
/// own inverse.
fn sortable_double(v: i64) -> f64 {
    f64::from_bits((v ^ ((v >> 63) & i64::MAX)) as u64)
}

/// A float from its sortable-int doc value, as [`sortable_double`] reads a double.
fn sortable_float(v: i64) -> f64 {
    let v = v as i32;
    f64::from(f32::from_bits((v ^ ((v >> 31) & i32::MAX)) as u32))
}

/// The IEEE half-precision bits nearest `v` (ties to even), like Lucene's
/// `HalfFloatPoint.halfFloatToShortBits`.
pub fn half_float_bits(v: f32) -> i16 {
    let round_shift = |i: i32, shift: i32| {
        let i = i + (1 << (shift - 1));
        ((i - ((i as u32 >> shift) as i32 & 1)) as u32 >> shift) as i32
    };
    let bits = v.to_bits() as i32;
    let (sign, exp, mantissa) = (bits >> 31 & 1, bits >> 23 & 0xff, bits & 0x7f_ffff);
    let (exp, mantissa) = match exp {
        0xff => (0x1f, mantissa >> 13),
        // even the largest denormal float is below the smallest half float
        0 => (0, 0),
        _ => match exp - 127 + 15 {
            e if e >= 0x1f => (0x1f, 0),
            e if 23 - 10 - e + 1 >= 32 => (0, 0),
            e => {
                let m = match e <= 0 {
                    true => round_shift(mantissa | 0x80_0000, 23 - 10 - e + 1),
                    false => round_shift(e << 23 | mantissa, 23 - 10),
                };
                (m >> 10, m & 0x3ff)
            }
        },
    };
    (sign << 15 | exp << 10 | mantissa) as i16
}

/// The float of half-precision `bits` (exact).
pub fn half_float(bits: i16) -> f32 {
    let (exp, mantissa) = (i32::from(bits) >> 10 & 0x1f, f32::from(bits as u16 & 0x3ff));
    let v = match exp {
        0x1f if mantissa == 0.0 => f32::INFINITY,
        0x1f => f32::NAN,
        0 => mantissa * 2f32.powi(-24),
        e => (1024.0 + mantissa) * 2f32.powi(e - 25),
    };
    if bits < 0 { -v } else { v }
}

/// The sortable short ES stores as a `half_float` doc value (`halfFloatToSortableShort`).
pub fn half_float_sortable(v: f32) -> i64 {
    let s = half_float_bits(v);
    i64::from(s ^ ((s >> 15) & 0x7fff))
}

/// The 16-byte encoding of an `ip` value (IPv4 as IPv4-mapped IPv6), like Lucene's
/// `InetAddressPoint.encode`; it sorts like Elasticsearch orders addresses.
pub fn ip_bytes(v: &serde_json::Value) -> Option<[u8; 16]> {
    let ip: std::net::IpAddr = v.as_str()?.parse().ok()?;
    Some(match ip {
        std::net::IpAddr::V4(v4) => v4.to_ipv6_mapped().octets(),
        std::net::IpAddr::V6(v6) => v6.octets(),
    })
}

/// The canonical string of an encoded `ip`, like Elasticsearch's `DocValueFormat.IP`.
pub fn ip_string(bytes: [u8; 16]) -> String {
    let v6 = std::net::Ipv6Addr::from(bytes);
    v6.to_ipv4_mapped()
        .map_or_else(|| v6.to_string(), |v4| v4.to_string())
}

/// Formats epoch milliseconds as ES's `date_optional_time` (`2024-01-15T10:30:00.000Z`).
pub fn iso_date(millis: i64) -> String {
    let (days, ms) = (millis.div_euclid(86_400_000), millis.rem_euclid(86_400_000));
    let (year, month, day) = civil_from_days(days);
    let (h, m, sec, milli) = (ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000);
    format!("{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{sec:02}.{milli:03}Z")
}

/// A Java DateTimeFormatter pattern as runs of a field letter (`y`, `u`, `M`, `d`, `H`, `m`,
/// `s`, `S`) and its width, or of literal text (width 0); `None` for other letters.
// ponytail: numeric fields only; no month or day names, zones, or optional sections
fn pattern_runs(pattern: &str) -> Option<Vec<(char, usize, String)>> {
    let mut runs: Vec<(char, usize, String)> = vec![];
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            'y' | 'u' | 'M' | 'd' | 'H' | 'm' | 's' | 'S' => match runs.last_mut() {
                Some((l, n, _)) if *l == c => *n += 1,
                _ => runs.push((c, 1, String::new())),
            },
            '\'' => {
                let quoted: String = chars.by_ref().take_while(|&c| c != '\'').collect();
                runs.push(('\'', 0, quoted));
            }
            c if c.is_ascii_alphabetic() => return None,
            c => runs.push(('\'', 0, c.into())),
        }
    }
    Some(runs)
}

/// Epoch milliseconds (UTC) of `s` in a Java date pattern of numeric fields (see `pattern_runs`).
pub fn parse_pattern(pattern: &str, s: &str) -> Option<i64> {
    parse_pattern_local(pattern, s, false)
}

/// [`parse_pattern`]'s local epoch millis; with `round_up`, missing time fields default to
/// 23:59:59.999 like ES's round-up parser.
fn parse_pattern_local(pattern: &str, mut s: &str, round_up: bool) -> Option<i64> {
    let [mut y, mut mo, mut d] = [1970, 1, 1];
    let [mut h, mut mi, mut sec, mut ms] = match round_up {
        true => [23, 59, 59, 999],
        false => [0; 4],
    };
    for (c, n, text) in pattern_runs(pattern)? {
        if n == 0 {
            s = s.strip_prefix(text.as_str())?;
            continue;
        }
        let (digits, rest) = s.split_at_checked(n)?;
        if !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        s = rest;
        let v: i64 = digits.parse().ok()?;
        match c {
            'y' | 'u' => y = v,
            'M' => mo = v,
            'd' => d = v,
            'H' => h = v,
            'm' => mi = v,
            's' => sec = v,
            _ => ms = format!("{digits:0<3}")[..3].parse().ok()?,
        }
    }
    let valid = (1..=12).contains(&mo) && (1..=31).contains(&d) && h < 24 && mi < 60 && sec < 60;
    (s.is_empty() && valid)
        .then(|| days_from_civil(y, mo, d) * 86_400_000 + ((h * 60 + mi) * 60 + sec) * 1000 + ms)
}

/// Epoch milliseconds `t` (already shifted to local time) in a Java date pattern of numeric fields (see `pattern_runs`).
pub fn format_pattern(pattern: &str, t: i64) -> Option<String> {
    let (days, ms) = (t.div_euclid(86_400_000), t.rem_euclid(86_400_000));
    let (y, mo, d) = civil_from_days(days);
    let mut out = String::new();
    for (c, n, text) in pattern_runs(pattern)? {
        let v = match c {
            'y' | 'u' if n == 2 => y.rem_euclid(100),
            'y' | 'u' => y,
            'M' => mo,
            'd' => d,
            'H' => ms / 3_600_000,
            'm' => ms / 60_000 % 60,
            's' => ms / 1000 % 60,
            'S' => {
                out += &format!("{:03}", ms % 1000)
                    .chars()
                    .chain(std::iter::repeat('0'))
                    .take(n)
                    .collect::<String>();
                continue;
            }
            _ => {
                out += &text;
                continue;
            }
        };
        out += &format!("{v:0n$}");
    }
    Some(out)
}

/// The `(year, month, day)` of days since 1970-01-01 (Howard Hinnant's civil_from_days).
pub fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(month <= 2), month, day)
}

/// Days since 1970-01-01 of a date (Howard Hinnant's days_from_civil).
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = y - i64::from(m <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
}

/// Epoch milliseconds of a date value: a number, or ES's default `strict_date_optional_time`
/// string `yyyy[-MM[-dd[THH[:mm[:ss[.S+]]][Z|+HH[:mm]|-HH[:mm]]]]]`.
// ponytail: the mapping's `format` is ignored; numeric strings are not read as epoch_millis
pub fn parse_date_millis(v: &serde_json::Value) -> Option<i64> {
    let Some(s) = v.as_str() else {
        return v.as_i64();
    };
    let (local, offset) = parse_iso(s, false)?;
    Some(local - offset.unwrap_or(0))
}

/// The local epoch millis of a `strict_date_optional_time` string and the offset it gives, if
/// any. Missing fields default to the start of the year, or with `round_up` to 23:59:59.999 of
/// its first day, like ES's round-up parser.
fn parse_iso(mut s: &str, round_up: bool) -> Option<(i64, Option<i64>)> {
    fn num(s: &mut &str, n: usize) -> Option<i64> {
        let (d, rest) = s.split_at_checked(n)?;
        if !d.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        *s = rest;
        d.parse().ok()
    }
    fn eat(s: &mut &str, c: char) -> bool {
        s.strip_prefix(c).inspect(|r| *s = r).is_some()
    }
    let y = num(&mut s, 4)?;
    let [mut m, mut day] = [1, 1];
    let [mut h, mut min, mut sec, mut milli] = match round_up {
        true => [23, 59, 59, 999],
        false => [0; 4],
    };
    let mut offset = None;
    if eat(&mut s, '-') {
        m = num(&mut s, 2)?;
        if eat(&mut s, '-') {
            day = num(&mut s, 2)?;
            if eat(&mut s, 'T') {
                h = num(&mut s, 2)?;
                if eat(&mut s, ':') {
                    min = num(&mut s, 2)?;
                    if eat(&mut s, ':') {
                        sec = num(&mut s, 2)?;
                        if eat(&mut s, '.') || eat(&mut s, ',') {
                            let n = s.bytes().take_while(u8::is_ascii_digit).count();
                            if n == 0 {
                                return None;
                            }
                            milli = format!("{:0<3}", &s[..n.min(3)]).parse().ok()?;
                            s = &s[n..];
                        }
                    }
                }
                let sign = if eat(&mut s, '+') {
                    1
                } else if eat(&mut s, '-') {
                    -1
                } else {
                    0
                };
                if sign == 0 {
                    offset = eat(&mut s, 'Z').then_some(0);
                } else {
                    let oh = num(&mut s, 2)?;
                    let om = if eat(&mut s, ':') { num(&mut s, 2)? } else { 0 };
                    offset = Some(sign * (oh * 60 + om) * 60_000);
                }
            }
        }
    }
    if !s.is_empty() {
        return None;
    }
    let days = days_from_civil(y, m, day);
    Some((
        ((days * 24 + h) * 60 + min) * 60_000 + sec * 1000 + milli,
        offset,
    ))
}

const MONTHS: [(&str, [&str; 12]); 2] = [
    (
        "fr",
        [
            "janv.", "févr.", "mars", "avr.", "mai", "juin", "juil.", "août", "sept.", "oct.",
            "nov.", "déc.",
        ],
    ),
    (
        "en",
        [
            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
        ],
    ),
];

/// Epoch millis of `"E, d MMM yyyy HH:mm:ss Z"` with localized month names.
// ponytail: only this one custom format; generalize to a token-driven parser when another is needed.
fn parse_dated(v: &Value, locale: &str) -> Option<i64> {
    let months = MONTHS.iter().find(|(l, _)| *l == locale)?.1;
    let mut p = v.as_str()?.split_once(", ")?.1.split(' ');
    let d: i64 = p.next()?.parse().ok()?;
    let name = p.next()?;
    let m = months.iter().position(|n| *n == name)? as i64 + 1;
    let y: i64 = p.next()?.parse().ok()?;
    let mut t = p.next()?.split(':').map(|x| x.parse::<i64>());
    let secs = t.next()?.ok()? * 3600 + t.next()?.ok()? * 60 + t.next()?.ok()?;
    let z = p.next()?;
    let off = (z[1..3].parse::<i64>().ok()? * 3600 + z[3..5].parse::<i64>().ok()? * 60)
        * if z.starts_with('-') { -1 } else { 1 };
    Some((days_from_civil(y, m, d) * 86400 + secs - off) * 1000)
}

/// Epoch millis of a value of a date field with `format` and `locale`.
pub fn date_millis(format: Option<&str>, locale: Option<&str>, v: &Value) -> Option<i64> {
    date_millis_in(format, locale, v, &tz::TimeZone::UTC, false)
}

/// [`date_millis`] of a value without an offset of its own in time zone `tz`, missing fields
/// rounded up with `round_up` (see [`parse_iso`]).
pub(crate) fn date_millis_in(
    format: Option<&str>,
    locale: Option<&str>,
    v: &Value,
    tz: &tz::TimeZone,
    round_up: bool,
) -> Option<i64> {
    let local = |l: i64| tz.at_local(l, 0);
    let iso = |s: &str| parse_iso(s, round_up).map(|(l, o)| o.map_or_else(|| local(l), |o| l - o));
    let Some(format) = format else {
        // strict_date_optional_time||epoch_millis
        return v
            .as_str()
            .map_or_else(|| v.as_i64(), |s| iso(s).or_else(|| s.parse().ok()));
    };
    (format.split("||"))
        .find_map(|f| match f {
            "epoch_millis" => json_num(v).map(|x| x as i64),
            "epoch_second" => json_num(v)
                .map(|x| (x * 1000.0) as i64 + if round_up && x.fract() == 0.0 { 999 } else { 0 }),
            // ponytail: other named formats (`strict_date_time`, ...) parse as the default format
            _ if format_pattern(f, 0).is_none() => iso(v.as_str()?),
            _ => parse_pattern_local(f, v.as_str()?, round_up).map(local),
        })
        .or_else(|| parse_dated(v, locale.unwrap_or("en")))
}

/// A JSON number, or a string holding one, as `f64`.
pub fn json_num(v: &Value) -> Option<f64> {
    v.as_f64().or_else(|| v.as_str()?.parse().ok())
}

/// Parses an ES time value (`250ms`, `60s`, `2m`, `1h`, `1d`); `None` for `-1` or junk.
pub fn parse_time(v: &str) -> Option<Duration> {
    let ms = index_settings::time_millis(v, "").ok()?;
    (ms >= 0).then(|| Duration::from_millis(ms as u64))
}

/// Parses an ES byte size (`512mb`, `1kb`, `100b`; `b` to `pb`, any case); `None` for junk.
pub(crate) fn parse_bytes(v: &str) -> Option<u64> {
    let v = v.trim().to_ascii_lowercase();
    let units = ["pb", "tb", "gb", "mb", "kb", "b"];
    units.iter().zip((0..6).rev()).find_map(|(unit, pow)| {
        let n: f64 = v.strip_suffix(unit)?.trim().parse().ok()?;
        Some((n * 1024f64.powi(pow)) as u64)
    })
}

/// All indices, keyed by name.
pub type Indices = BTreeMap<String, Index>;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn docs_are_visited_in_indexing_order() {
        let mut ix = Index::default();
        let ids: Vec<String> = (0..40).map(|i| format!("d{}", (i * 7) % 40)).collect();
        for id in &ids {
            ix.put(id, json!({}));
        }
        ix.put("d7", json!({"updated": true}));
        let mut want: Vec<&str> = ids
            .iter()
            .map(String::as_str)
            .filter(|&id| id != "d7")
            .collect();
        want.push("d7");
        let got: Vec<&str> = ix.docs().map(|(id, _)| id).collect();
        assert_eq!(got, want);
        // older documents on disk come first, like older segments
        let dir = std::env::temp_dir().join(format!("esm-order-{}", std::process::id()));
        disk::save(&dir, &Indices::from([("i".to_owned(), ix)])).unwrap();
        let mut ix = disk::open(&dir).unwrap().remove("i").unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        ix.put("new2", json!({}));
        ix.put("new1", json!({}));
        let got: Vec<String> = ix.docs().map(|(id, _)| id.to_owned()).collect();
        assert_eq!(got[40..], ["new2", "new1"]);
    }

    #[test]
    fn request_cache_reuses_results_until_the_index_changes() {
        let mut ix = Index::default();
        let mut runs = 0;
        let mut search = |ix: &Index, key: &str| {
            ix.cached(key.into(), || {
                runs += 1;
                Ok::<_, ()>(json!(runs))
            })
            .unwrap()
        };
        assert_eq!(search(&ix, "a"), json!(1));
        assert_eq!(search(&ix, "a"), json!(1));
        assert_eq!(search(&ix, "b"), json!(2));
        ix.put("1", json!({}));
        assert_eq!(search(&ix, "a"), json!(3));
        ix.mappings.source = Some(json!({"enabled": false}));
        assert_eq!(search(&ix, "a"), json!(4));
        let (hits, misses, evictions, bytes) = ix.request_cache_stats();
        assert_eq!((hits, misses, evictions), (1, 4, 0));
        assert!(bytes > 0);
    }

    #[test]
    fn memory_bounds_request_cache_evicts_least_recently_used_entries_past_its_cap() {
        let ix = Index::default();
        let mut runs = 0;
        let mut search = |key: &str| {
            ix.cached(key.into(), || {
                runs += 1;
                // three entries fit
                Ok::<_, ()>(json!("x".repeat(REQUEST_CACHE_BYTES / 3 - 16)))
            })
            .unwrap();
            runs
        };
        for key in ["a", "b", "c"] {
            search(key);
        }
        assert_eq!(search("a"), 3, "a is cached and now the most recently used");
        assert_eq!(search("d"), 4, "evicts b, the least recently used");
        let (hits, misses, evictions, bytes) = ix.request_cache_stats();
        assert_eq!((hits, misses, evictions), (1, 4, 1));
        assert!(bytes <= REQUEST_CACHE_BYTES);
        assert_eq!(search("a"), 4);
        assert_eq!(search("b"), 5);
    }

    #[test]
    fn positive_refresh_interval_delays_visibility_until_it_elapses() {
        let mut ix = Index::default();
        ix.settings.merge(&json!({"index.refresh_interval": "1s"}));
        let first = ix.put("1", json!({})).seq_no;
        assert!(!ix.searchable(first));
        ix.refresh();
        assert!(ix.searchable(first));
        let second = ix.put("2", json!({})).seq_no;
        assert!(!ix.searchable(second));
        std::thread::sleep(Duration::from_millis(1100));
        assert!(ix.searchable(second));
        // the default interval keeps writes searchable at once (as search-idle shards do)
        let mut ix = Index::default();
        let seq_no = ix.put("1", json!({})).seq_no;
        assert!(ix.searchable(seq_no));
    }

    #[test]
    fn a_snapshot_keeps_the_writes_visible_when_taken() {
        let mut ix = Index::default();
        ix.settings
            .merge(&json!({"index.refresh_interval": "100ms"}));
        let first = ix.put("1", json!({})).seq_no;
        std::thread::sleep(Duration::from_millis(200));
        let second = ix.put("2", json!({})).seq_no;
        let snap = ix.snapshot();
        assert!(snap.searchable(first) && !snap.searchable(second));
        std::thread::sleep(Duration::from_millis(200));
        assert!(ix.searchable(second));
        assert!(!snap.searchable(second), "a snapshot is not refreshed");
    }

    #[test]
    fn date_patterns_parse_and_format() {
        let p = "yyyy-MM-dd HH:mm:ss";
        let t = 1_619_899_200_000;
        assert_eq!(parse_pattern(p, "2021-05-01 20:00:00"), Some(t));
        assert_eq!(format_pattern(p, t).as_deref(), Some("2021-05-01 20:00:00"));
        let q = "yyyy-MM-dd'T'HH:mm:ss.SSS";
        assert_eq!(parse_pattern(q, "1969-12-31T23:59:59.999"), Some(-1));
        assert_eq!(
            format_pattern(q, -1).as_deref(),
            Some("1969-12-31T23:59:59.999")
        );
        assert_eq!(parse_pattern(p, "2021-05-01T20:00:00"), None);
        assert_eq!(parse_pattern(p, "2021-05-01 20:00:00 extra"), None);
        // patterns with other fields (names, zones) are not supported
        assert_eq!(parse_pattern("dd MMM yyyy", "01 May 2021"), None);
        assert_eq!(format_pattern("yyyy Z", t), None);
    }

    #[test]
    fn field_mapping_type_name_and_numeric() {
        assert_eq!(FieldMapping::Short.type_name(), "integer");
        assert_eq!(FieldMapping::Float.type_name(), "double");
        assert!(FieldMapping::Byte.is_numeric());
        assert!(!FieldMapping::Boolean.is_numeric());
        assert!(!FieldMapping::Text { store: false }.is_numeric());
    }

    #[test]
    fn mappings_round_trip() {
        let raw = json!({"properties": {
            "title": {"type": "text"},
            "body": {"type": "text", "store": true},
            "tag": {"type": "keyword"},
            "n": {"type": "long"},
            "ok": {"type": "boolean"},
            "at": {"type": "date"}
        }});
        let m: Mappings = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(
            m.properties["tag"],
            FieldMapping::Keyword {
                ignore_above: None,
                split_queries_on_whitespace: false
            }
        );
        assert_eq!(serde_json::to_value(&m).unwrap(), raw);
    }

    #[test]
    fn dynamic_mapping_detects_types() {
        let mut i = Index::default();
        i.map_dynamic(
            "1",
            &json!({
                "s": "hi", "n": 3, "f": 1.5, "b": true, "d": "2024-01-02T03:04:05Z",
                "d2": "2024-01", "notd": "2024-1-2", "arr": [null, 7], "nul": null
            }),
        )
        .unwrap();
        let p = &i.mappings.properties;
        assert!(matches!(p["s"], FieldMapping::TextKeyword { .. }));
        assert_eq!(
            serde_json::to_value(&p["s"]).unwrap(),
            json!({"type": "text", "fields": {"keyword": {"type": "keyword", "ignore_above": 256}}})
        );
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
    fn dynamic_mapping_maps_object_fields() {
        let mut i = Index::default();
        i.map_dynamic(
            "1",
            &json!({
                "o": {"a": 1, "b": {"c": true}}, "p.q": 1.5,
                "arr": [{"x": 1}, {"y": false}], "e": {}
            }),
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(&i.mappings).unwrap(),
            json!({"properties": {
                "arr": {"properties": {"x": {"type": "long"}, "y": {"type": "boolean"}}},
                "e": {"type": "object"},
                "o": {"properties": {
                    "a": {"type": "long"},
                    "b": {"properties": {"c": {"type": "boolean"}}}
                }},
                "p": {"properties": {"q": {"type": "float"}}}
            }})
        );
        assert_eq!(i.mappings.field("o.b.c"), Some(&FieldMapping::Boolean));
    }

    #[test]
    fn mapping_validation_leaf_object_conflicts_reject_the_document() {
        let mut i = Index::default();
        i.map_dynamic("0", &json!({"a": 1, "o": {"b": 1}, "n": [{"k": 1}]}))
            .unwrap();
        let before = i.mappings.clone();
        let leaf = |f: &str, preview: &str| {
            format!(
                "failed to parse field [{f}] of type [long] in document with id '1'. \
                 Preview of field's value: '{preview}'"
            )
        };
        let object = |o: &str, f: &str| {
            format!(
                "object mapping for [{o}] tried to parse field [{f}] as object, but found a concrete value"
            )
        };
        // Elasticsearch 9.5.5's reasons, without the `[line:column]` prefix.
        for (doc, err) in [
            (
                json!({"new": 1, "a": {"b": 1, "c": ["x", null]}}),
                leaf("a", "{b=1, c=[x, null]}"),
            ),
            (json!({"a.b": 1}), leaf("a", "{b=1}")),
            (json!({"a": {}}), leaf("a", "{}")),
            (json!({"a": [1, {"b": 1}]}), leaf("a", "{b=1}")),
            (json!({"x": 1, "x.y": 2}), leaf("x", "{y=2}")),
            (json!({"o": 2}), object("o", "o")),
            (json!({"o": [1]}), object("o", "null")),
            (json!({"n": "s"}), object("n", "n")),
            (json!({"o": {"b": {"c": 1}}}), leaf("o.b", "{c=1}")),
        ] {
            assert_eq!(i.map_dynamic("1", &doc), Err(err), "{doc}");
            assert_eq!(i.mappings, before, "{doc} maps nothing");
        }
        i.map_dynamic("2", &json!({"o": null, "o2": {}, "a": null}))
            .unwrap();
    }

    #[test]
    fn numeric_and_date_detection() {
        let doc = json!({"l": "-12", "f": "1.5", "y": "2024", "d": "2024-01-02", "s": "x"});
        let detect = |mappings: Value| {
            let mut i = Index {
                mappings: serde_json::from_value(mappings.clone()).unwrap(),
                ..Default::default()
            };
            assert_eq!(serde_json::to_value(&i.mappings).unwrap(), mappings);
            i.map_dynamic("1", &doc).unwrap();
            let p = i.mappings.properties;
            ["l", "f", "y", "d", "s"].map(|f| p[f].type_name())
        };
        // Strings that parse as numbers are never dates.
        let (text, date) = ("text", "date");
        assert_eq!(detect(json!({})), [text, text, text, date, text]);
        assert_eq!(
            detect(json!({"numeric_detection": true})),
            ["long", "double", "long", date, text]
        );
        assert_eq!(
            detect(json!({"date_detection": false})),
            [text, text, text, text, text]
        );
    }

    #[test]
    fn mapping_validation_numeric_detection_follows_java_parse_double() {
        let mut i: Index = Index {
            mappings: serde_json::from_value(json!({"numeric_detection": true})).unwrap(),
            ..Default::default()
        };
        let floats = [
            "1f",
            " 2.5D ",
            "0x1.8p3",
            "-0X1P-2d",
            "NaN",
            "-Infinity",
            ".5",
            "5.",
            "1e+5",
        ];
        let strings = [
            "inf", "nan", "infinity", "0x1.8", "NaNd", "+-1", "1_0", "1e", ".",
        ];
        let all: Vec<_> = floats.iter().chain(&strings).collect();
        let doc: serde_json::Map<String, Value> = (all.iter().enumerate())
            .map(|(n, s)| (format!("f{n}"), json!(s)))
            .collect();
        i.map_dynamic("1", &doc.into()).unwrap();
        for (n, s) in all.iter().enumerate() {
            let float = n < floats.len();
            let t = i.mappings.properties[&format!("f{n}")].mapping_type();
            assert_eq!(t, if float { "float" } else { "text" }, "{s}");
        }
    }

    #[test]
    fn mapping_validation_empty_objects_map_as_objects() {
        let mut i = Index::default();
        i.map_dynamic("1", &json!({"e": {}, "a": [{}], "o": {"p": {}}}))
            .unwrap();
        let empty = json!({"type": "object"});
        let expected = json!({"properties": {
            "a": empty, "e": empty, "o": {"properties": {"p": empty}}
        }});
        assert_eq!(serde_json::to_value(&i.mappings).unwrap(), expected);
        let err =
            "object mapping for [e] tried to parse field [e] as object, but found a concrete value";
        assert_eq!(i.map_dynamic("2", &json!({"e": 1})), Err(err.into()));
        // A subfield makes the object's type implicit again.
        i.map_dynamic("3", &json!({"e": {"x": 1}, "o.p": {}}))
            .unwrap();
        let expected = json!({"properties": {
            "a": empty, "e": {"properties": {"x": {"type": "long"}}},
            "o": {"properties": {"p": empty}}
        }});
        assert_eq!(serde_json::to_value(&i.mappings).unwrap(), expected);
        // Mappings without properties read back as empty objects.
        let raw = json!({"properties": {"t": empty, "u": {}, "v": {"properties": {}}}});
        let m: Mappings = serde_json::from_value(raw).unwrap();
        let expected = json!({"properties": {"t": empty, "u": empty, "v": empty}});
        assert_eq!(serde_json::to_value(&m).unwrap(), expected);
    }

    #[test]
    fn mapping_validation_merges_keep_multi_fields_and_refuse_fixed_parameters() {
        let m = |v: Value| serde_json::from_value::<Mappings>(v).unwrap();
        let mut old = m(json!({"properties": {
            "n": {"type": "nested"}, "o": {"properties": {"a": {"type": "long"}}},
            "d": {"type": "date", "format": "yyyy"}, "t": {"type": "text"},
            "dn": {"type": "date_nanos"}, "dr": {"type": "date_range"},
            "e": {"type": "object"}, "k": {"type": "keyword", "ignore_above": 5}
        }}));
        let mut i = Index::default();
        i.map_dynamic("1", &json!({"s": "x"})).unwrap();
        old.extend(i.mappings.properties);
        let merge = |new: Value| old.merged(&m(json!({ "properties": new })));
        let fixed = |f: &str, conflicts: &[&str]| {
            let c: Vec<String> = conflicts
                .iter()
                .map(|c| format!("\n\tCannot update parameter {c}"))
                .collect();
            Err(format!(
                "Mapper for [{f}] conflicts with existing mapper:{}",
                c.concat()
            ))
        };
        let non_object = |p: &str| {
            Err(format!(
                "can't merge a non object mapping [{p}] with an object mapping"
            ))
        };
        let non_nested = |p: &str| {
            Err(format!(
                "can't merge a non-nested mapping [{p}] with a nested mapping"
            ))
        };
        // Elasticsearch 9.5.5's reasons.
        for (new, err) in [
            (
                json!({"n": {"properties": {"x": {"type": "long"}}}}),
                non_nested("n"),
            ),
            (json!({"o": {"type": "nested"}}), non_nested("o")),
            (json!({"e": {"type": "nested"}}), non_nested("e")),
            (json!({"o": {"type": "long"}}), non_object("o")),
            (json!({"e": {"type": "long"}}), non_object("e")),
            (
                json!({"k": {"properties": {"x": {"type": "long"}}}}),
                non_object("k"),
            ),
            (json!({"k.x": {"type": "long"}}), non_object("k")),
            (
                json!({"o.a": {"type": "integer"}}),
                Err("mapper [o.a] cannot be changed from type [long] to [integer]".into()),
            ),
            (
                json!({"d": {"type": "date"}}),
                fixed(
                    "d",
                    &["[format] from [yyyy] to [strict_date_optional_time||epoch_millis]"],
                ),
            ),
            (
                json!({"d": {"type": "date", "format": "MM", "locale": "fr"}}),
                fixed(
                    "d",
                    &["[format] from [yyyy] to [MM]", "[locale] from [en] to [fr]"],
                ),
            ),
            (
                json!({"dn": {"type": "date_nanos", "format": "yyyy"}}),
                fixed(
                    "dn",
                    &["[format] from [strict_date_optional_time_nanos||epoch_millis] to [yyyy]"],
                ),
            ),
            (
                json!({"dr": {"type": "date_range", "format": "yyyy"}}),
                fixed(
                    "dr",
                    &["[format] from [strict_date_optional_time||epoch_millis] to [yyyy]"],
                ),
            ),
            (
                json!({"t": {"type": "text", "store": true}}),
                fixed("t", &["[store] from [false] to [true]"]),
            ),
            (
                json!({"s": {"type": "text", "store": true}}),
                fixed("s", &["[store] from [false] to [true]"]),
            ),
        ] {
            assert_eq!(merge(new.clone()).map(|_| ()), err, "{new}");
        }
        let new = m(json!({"date_detection": false, "properties": {
            "s": {"type": "text"}, "k": {"type": "keyword", "ignore_above": 10},
            "d": {"type": "date", "format": "yyyy", "locale": "en"},
            "e": {"properties": {"x": {"type": "long"}}}, "n": {"type": "nested"}, "z": {"type": "long"}
        }}));
        let merged = serde_json::to_value(old.merged(&new).unwrap()).unwrap();
        // The omitted `keyword` multi-field of `s` is kept.
        assert_eq!(
            merged["properties"]["s"],
            json!({"type": "text", "fields": {"keyword": {"type": "keyword", "ignore_above": 256}}})
        );
        assert_eq!(
            merged["properties"]["k"],
            json!({"type": "keyword", "ignore_above": 10})
        );
        assert_eq!(
            merged["properties"]["e"],
            json!({"properties": {"x": {"type": "long"}}})
        );
        assert_eq!(merged["properties"]["z"], json!({"type": "long"}));
        assert_eq!(merged["date_detection"], false);
    }

    #[test]
    fn object_mappings_round_trip_through_dotted_paths() {
        let raw = json!({"properties": {
            "o": {"properties": {"a": {"type": "keyword"}, "b": {"properties": {"c": {"type": "long"}}}}}
        }});
        let m: Mappings = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(m.properties.keys().collect::<Vec<_>>(), ["o.a", "o.b.c"]);
        assert_eq!(serde_json::to_value(&m).unwrap(), raw);
        let typed: Mappings = serde_json::from_value(json!({"properties": {
            "o": {"type": "object", "properties": {"a": {"type": "long"}}}, "d.e": {"type": "long"}
        }}))
        .unwrap();
        assert_eq!(typed.properties.keys().collect::<Vec<_>>(), ["d.e", "o.a"]);
    }

    #[test]
    fn nested_mappings_round_trip_and_keep_dynamic_subfields() {
        let raw = json!({"properties": {
            "n": {"type": "nested", "properties": {"j": {"type": "integer"}}}
        }});
        let mut m: Mappings = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(m.properties.keys().collect::<Vec<_>>(), ["n", "n.j"]);
        assert_eq!(serde_json::to_value(&m).unwrap(), raw);
        let mut i = Index::default();
        std::mem::swap(&mut i.mappings, &mut m);
        i.map_dynamic("1", &json!({"n": [{"k": true}]})).unwrap();
        assert_eq!(i.mappings.properties["n.k"], FieldMapping::Boolean);
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
    fn generated_ids_log_their_timestamp() {
        let mut ix = Index::default();
        ix.put("a", json!({}));
        let (id, ts) = auto_id();
        assert_eq!(id.len(), 20);
        ix.put_auto(&id, json!({}), ts);
        let stamps: Vec<i64> = ix
            .take_unlogged()
            .into_iter()
            .map(|op| match op {
                es_codec::translog::Op::Index {
                    auto_id_timestamp, ..
                } => auto_id_timestamp,
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(stamps, [-1, ts]);
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
        assert_eq!(s.to_json()["index"]["a"], "x");
        assert_eq!(s.to_json()["index"]["a.b"], "y");
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
        for ms in [0, 1_705_314_600_123, 1_709_251_200_000, -86_400_001] {
            assert_eq!(parse_date_millis(&json!(iso_date(ms))), Some(ms));
        }
        assert_eq!(
            parse_date_millis(&json!("2024-01-01")),
            Some(1_704_067_200_000)
        );
        assert_eq!(parse_date_millis(&json!(5)), Some(5));
        assert_eq!(parse_date_millis(&json!("junk")), None);
    }

    #[test]
    fn dynamically_detected_dates_parse_to_epoch_millis() {
        // The disk writer skips a date field whose values do not parse, so every string
        // map_dynamic detects as a date must parse.
        for (s, ms) in [
            ("2024-01", 1_704_067_200_000),
            ("2024-01-02T03", 1_704_164_400_000),
            ("2024-01-02T03:04:05+01:00", 1_704_161_045_000),
            ("2024-01-02T03:04:05-01", 1_704_168_245_000),
            ("2024-01-02T03:04:05,5Z", 1_704_164_645_500),
        ] {
            let mut i = Index::default();
            i.map_dynamic("1", &json!({ "d": s })).unwrap();
            assert!(matches!(
                i.mappings.properties["d"],
                FieldMapping::Date { .. }
            ));
            assert_eq!(parse_date_millis(&json!(s)), Some(ms), "{s}");
        }
    }

    #[test]
    fn formats_epoch_millis_as_iso_dates() {
        assert_eq!(iso_date(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso_date(1_792_182_985_033), "2026-10-16T20:36:25.033Z");
        assert_eq!(iso_date(951_782_400_000), "2000-02-29T00:00:00.000Z");
        assert_eq!(iso_date(-1), "1969-12-31T23:59:59.999Z");
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
        // TimeValue.parseTimeValue's case-insensitive units and whitespace
        assert_eq!(parse_time(" 1H"), Some(Duration::from_secs(3600)));
        assert_eq!(parse_time("2000000micros"), Some(Duration::from_secs(2)));
    }

    #[test]
    fn parses_es_byte_sizes() {
        assert_eq!(parse_bytes("100b"), Some(100));
        assert_eq!(parse_bytes("1kb"), Some(1024));
        assert_eq!(parse_bytes("512MB"), Some(512 << 20));
        assert_eq!(parse_bytes("1.5gb"), Some(3 << 29));
        assert_eq!(parse_bytes("big"), None);
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
        assert_eq!(r, Ok(Some(w(DocResult::Updated, 2, 1))));
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
            Ok(Some(w(DocResult::Noop, 1, 0)))
        );
    }

    #[test]
    fn update_without_detect_noop_bumps_version() {
        let mut idx = Index::default();
        idx.put("1", json!({"a": 1}));
        let r = idx
            .update("1", &json!({"a": 1}), None, false)
            .unwrap()
            .unwrap();
        assert_eq!((r.result, r.version), (DocResult::Updated, 2));
    }

    #[test]
    fn update_missing_doc() {
        let mut idx = Index::default();
        assert_eq!(idx.update("1", &json!({"a": 1}), None, true), Ok(None));
        let r = idx.update("1", &json!({"a": 1}), Some(json!({"u": 1})), true);
        assert_eq!(r, Ok(Some(w(DocResult::Created, 1, 0))));
        assert_eq!(idx.get("1").unwrap().source, json!({"u": 1}));
    }

    #[test]
    fn disk_values_read_the_doc_values_of_on_disk_documents() {
        let dir = std::env::temp_dir().join(format!("esm-disk-values-{}", std::process::id()));
        let mut ix = Index::default();
        let props = &mut ix.mappings.properties;
        props.insert("n".into(), FieldMapping::Long);
        props.insert("d".into(), FieldMapping::Double);
        props.insert("f".into(), FieldMapping::Float);
        props.insert(
            "k".into(),
            FieldMapping::Keyword {
                ignore_above: None,
                split_queries_on_whitespace: false,
            },
        );
        props.insert("ok".into(), FieldMapping::Boolean);
        let date = FieldMapping::Date {
            format: None,
            locale: None,
            fields: Default::default(),
        };
        props.insert("at".into(), date);
        props.insert("t".into(), FieldMapping::Text { store: false });
        let doc = json!({"n": [3, -2], "d": -1.5, "f": 1.1, "k": ["b", "a", "b"], "ok": true,
            "at": "2021-05-01T00:00:00Z", "t": "x"});
        ix.put("1", doc);
        ix.put("2", json!({"n": 1}));
        disk::save(&dir, &Indices::from([("i".to_owned(), ix)])).unwrap();
        let mut opened = disk::open(&dir).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        let ix = opened.get_mut("i").unwrap();
        ix.put("3", json!({"n": 9}));
        let read = |field: &str, id: &str| ix.disk_values(field)(id);
        assert_eq!(read("n", "1"), Some(vec![json!(-2), json!(3)]));
        assert_eq!(read("d", "1"), Some(vec![json!(-1.5)]));
        assert_eq!(read("f", "1"), Some(vec![json!(f64::from(1.1f32))]));
        assert_eq!(read("k", "1"), Some(vec![json!("a"), json!("b")]));
        assert_eq!(read("ok", "1"), Some(vec![json!(true)]));
        assert_eq!(read("at", "1"), Some(vec![json!(1_619_827_200_000i64)]));
        assert_eq!(read("d", "2"), Some(vec![]));
        // text has no doc values; a document written since opening is in memory
        assert_eq!(read("t", "1"), None);
        assert_eq!(read("n", "3"), None);
    }

    #[test]
    fn float_ranges_narrow_disk_docs() {
        let dir = std::env::temp_dir().join(format!("esm-disk-float-{}", std::process::id()));
        let mut ix = Index::default();
        (ix.mappings.properties).insert("f".into(), FieldMapping::Float);
        ix.put("1", json!({"f": 1.1}));
        ix.put("2", json!({"f": [-3.5, 2.5]}));
        ix.put("3", json!({}));
        disk::save(&dir, &Indices::from([("i".to_owned(), ix)])).unwrap();
        let opened = disk::open(&dir).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        let ix = &opened["i"];
        let ids = |lo: f64, hi: f64| {
            let place = |v: &Value| {
                let v = v.as_f64().unwrap();
                if v < lo {
                    Ordering::Less
                } else if v > hi {
                    Ordering::Greater
                } else {
                    Ordering::Equal
                }
            };
            let docs = ix.disk_numbers("f", place, true);
            let has = |id: &&str| docs.as_ref().unwrap().contains(&ix.disk_pos(id).unwrap());
            ["1", "2", "3"].into_iter().filter(has).collect::<Vec<_>>()
        };
        // values are their f32 values
        let f = f64::from(1.1f32);
        assert_eq!(ids(f, f), ["1"]);
        assert_eq!(ids(f64::MIN, -0.1), ["2"]);
        assert_eq!(ids(1.01, f64::MAX), ["1", "2"]);
    }

    /// A narrow range reads the points tree's leaves around it, not every value.
    #[test]
    fn number_ranges_walk_the_points_tree() {
        let dir = std::env::temp_dir().join(format!("esm-disk-points-{}", std::process::id()));
        let mut ix = Index::default();
        (ix.mappings.properties).insert("n".into(), FieldMapping::Long);
        for i in 0..5000 {
            ix.put(&i.to_string(), json!({"n": i}));
        }
        disk::save(&dir, &Indices::from([("i".to_owned(), ix)])).unwrap();
        let opened = disk::open(&dir).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        let ix = &opened["i"];
        let calls = std::cell::Cell::new(0);
        let place = |v: &Value| {
            calls.set(calls.get() + 1);
            let v = v.as_i64().unwrap();
            if v < 2000 {
                Ordering::Less
            } else if v > 2009 {
                Ordering::Greater
            } else {
                Ordering::Equal
            }
        };
        let docs = ix.disk_numbers("n", place, true).unwrap();
        let want: BTreeSet<_> = (2000..2010)
            .map(|i| ix.disk_pos(&i.to_string()).unwrap())
            .collect();
        assert_eq!(docs, want);
        assert!(calls.get() < 1000, "{} values compared", calls.get());
    }

    #[test]
    fn term_freqs_come_from_postings_and_norms() {
        let dir = std::env::temp_dir().join(format!("esm-term-freqs-{}", std::process::id()));
        let mut ix = Index::default();
        let props = &mut ix.mappings.properties;
        props.insert("t".into(), FieldMapping::Text { store: false });
        props.insert(
            "k".into(),
            FieldMapping::Keyword {
                ignore_above: None,
                split_queries_on_whitespace: false,
            },
        );
        let long = vec!["fox"; 100].join(" ");
        ix.put("1", json!({"t": "fox fox dog", "k": ["a", "a", "b"]}));
        ix.put("2", json!({"t": ["fox", "a b c"]}));
        ix.put("3", json!({"t": long}));
        ix.put("4", json!({"t": "dog"}));
        // per document: (freq, field length as its norm stands for it)
        let fox = |ix: &Index, id: &str| {
            let f = ix.term_freqs("t", "t", true, "fox").unwrap();
            f.memory
                .get(id)
                .or_else(|| f.disk.get(&ix.disk_pos(id)?))
                .copied()
        };
        let a = |ix: &Index, id: &str| {
            let f = ix.term_freqs("k", "k", false, "a").unwrap();
            f.memory
                .get(id)
                .or_else(|| f.disk.get(&ix.disk_pos(id)?))
                .copied()
        };
        let all = |ix: &Index| {
            let ids = ["1", "2", "3", "4"];
            (ids.map(|id| fox(ix, id)), a(ix, "1"))
        };
        let expected = (
            [Some((2, 3)), Some((1, 4)), Some((100, 96)), None],
            // keywords index docs only and omit norms, like Elasticsearch's
            Some((1, 1)),
        );
        assert_eq!(all(&ix), expected);
        disk::save(&dir, &Indices::from([("i".to_owned(), ix)])).unwrap();
        let mut opened = disk::open(&dir).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        let ix = opened.get_mut("i").unwrap();
        assert_eq!(all(ix), expected);
        assert!(ix.disk_pos("1").is_some());
        // a write replaces the document on disk
        ix.put("1", json!({"t": "fox"}));
        assert_eq!(ix.disk_pos("1"), None);
        assert_eq!(fox(ix, "1"), Some((1, 1)));
    }

    #[test]
    fn half_floats_round_like_lucene() {
        let half = |f: f32| f64::from(half_float(half_float_bits(f)));
        assert_eq!(half(0.0152), 0.01519775390625);
        assert_eq!(half(0.04), 0.040008544921875);
        assert_eq!(half(-2.5), -2.5);
        assert_eq!(half(1e-6), 1.0132789611816406e-6);
        assert_eq!(half(65520.0), f64::INFINITY);
        assert_eq!(half_float_bits(0.0152), 0x23c8);
        // sortable: negative values sort before positive ones
        assert!(half_float_sortable(-1.0) < half_float_sortable(-0.5));
        assert!(half_float_sortable(-0.5) < half_float_sortable(2.0));
    }

    #[test]
    fn half_float_fields_keep_half_precision_on_disk() {
        let dir = std::env::temp_dir().join(format!("esm-half-float-{}", std::process::id()));
        let m: FieldMapping = serde_json::from_value(json!({"type": "half_float"})).unwrap();
        assert_eq!(
            serde_json::to_value(&m).unwrap(),
            json!({"type": "half_float"})
        );
        let mut ix = Index::default();
        ix.mappings.properties.insert("h".into(), m);
        ix.put("1", json!({"h": [0.0152, -2.5]}));
        disk::save(&dir, &Indices::from([("i".to_owned(), ix)])).unwrap();
        let opened = disk::open(&dir).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        let read = opened["i"].disk_values("h")("1");
        assert_eq!(read, Some(vec![json!(-2.5), json!(0.01519775390625)]));
    }

    #[test]
    fn ip_fields_read_back_as_canonical_addresses() {
        let m: FieldMapping = serde_json::from_value(json!({"type": "ip"})).unwrap();
        assert_eq!(m.type_name(), "ip");
        assert_eq!(
            ip_bytes(&json!("127.0.0.1")).unwrap()[10..],
            [255, 255, 127, 0, 0, 1]
        );
        assert_eq!(ip_bytes(&json!("nope")), None);
        let dir = std::env::temp_dir().join(format!("esm-disk-ip-{}", std::process::id()));
        let mut ix = Index::default();
        ix.mappings.properties.insert("ip".into(), m);
        ix.put("1", json!({"ip": ["127.0.0.1", "0:0::1", "::1"]}));
        disk::save(&dir, &Indices::from([("i".to_owned(), ix)])).unwrap();
        let opened = disk::open(&dir).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(
            opened["i"].disk_values("ip")("1"),
            Some(vec![json!("::1"), json!("127.0.0.1")])
        );
    }
}
