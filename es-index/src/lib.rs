//! Index/document model, mappings, analysis, storage glue.

pub mod aliases;
pub mod analysis;
pub mod disk;
pub mod filter_path;
pub mod mapping;
pub mod postings;
pub mod tz;

pub use mapping::{FieldMapping, Mappings};

use serde::Serialize;
use serde_json::Value;
use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque},
    time::{Duration, Instant},
};

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
            ("index.version.created", VERSION_CREATED.to_string()),
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
    /// Documents written since the index was opened; they replace any on disk.
    docs: HashMap<String, Document>,
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
    superseded: Vec<Value>,
    /// Version and deletion time of deleted docs, kept for `index.gc_deletes` (default 60s) so
    /// a re-created doc continues the version count, as in ES.
    deleted_versions: HashMap<String, (u64, Instant)>,
    /// Tombstones in deletion order, so expired ones are pruned from the front.
    tombstone_order: VecDeque<(String, Instant)>,
    next_seq_no: u64,
    refreshed_seq_no: u64,
    /// Writes not yet in the translog (see [`disk::persist`]).
    unlogged: Vec<es_codec::translog::Op>,
    request_cache: std::sync::Mutex<RequestCache>,
}

/// Elasticsearch's shard request cache (see [`Index::cached`]).
#[derive(Debug, Default)]
struct RequestCache {
    hits: u64,
    misses: u64,
    /// Serialized size of the keys and entries.
    bytes: usize,
    /// The visible sequence number and mappings the entries were computed against.
    reader: Option<(u64, Mappings)>,
    entries: HashMap<String, Value>,
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

    /// Removes a live document, returning its source if no segment holds it: one in a segment
    /// stays counted in its statistics, as a deleted document does in Lucene.
    fn take(&mut self, id: &str) -> Option<Option<Value>> {
        if let Some(d) = self.docs.remove(id) {
            self.postings
                .get_mut()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(id);
            return Some((d.seq_no >= self.persisted).then_some(d.source));
        }
        self.disk.as_mut()?.remove(id).map(|_| None)
    }

    fn next_seq_no(&mut self) -> u64 {
        self.next_seq_no += 1;
        self.next_seq_no - 1
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
        self.unlogged.push(es_codec::translog::Op::Index {
            id: id.to_owned(),
            source: serde_json::to_vec(&source).unwrap_or_default(),
            seq_no: seq_no as i64,
            version: version as i64,
            primary_term: 1,
            routing: None,
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
            Document {
                source,
                version,
                seq_no,
            },
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
        match self.meta(id) {
            Some((version, _)) => Err(version),
            None => Ok(self.put(id, source)),
        }
    }

    /// A copy for point-in-time searches: later writes to `self` do not change it.
    // ponytail: copies every document into memory (on-disk segments may be deleted by later
    // saves); memory grows with the index size times open snapshots
    pub fn snapshot(&self) -> Index {
        Index {
            stats: self.stats.clone(),
            settings: self.settings.clone(),
            mappings: self.mappings.clone(),
            docs: self
                .docs()
                .map(|(id, d)| (id.to_owned(), d.into_owned()))
                .collect(),
            superseded: self.superseded.clone(),
            persisted: self.persisted,
            next_seq_no: self.next_seq_no,
            refreshed_seq_no: self.refreshed_seq_no,
            ..Index::default()
        }
    }

    /// The result of `compute` for `key`, reused until the visible documents or the mappings
    /// change, like Elasticsearch's shard request cache.
    // ponytail: unbounded until the next change; ES caps the cache at 1% of the heap
    pub fn cached<E>(
        &self,
        key: String,
        compute: impl FnOnce() -> Result<Value, E>,
    ) -> Result<Value, E> {
        let visible = if self.settings.get("index.refresh_interval") == Some("-1") {
            self.refreshed_seq_no
        } else {
            self.next_seq_no
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
                c.bytes = 0;
            }
            if let Some(v) = c.entries.get(&key).cloned() {
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
            c.bytes += key.len() + v.to_string().len();
            c.entries.insert(key, v.clone());
        }
        Ok(v)
    }

    /// The request cache's (hit count, miss count, size of its entries in bytes).
    pub fn request_cache_stats(&self) -> (u64, u64, usize) {
        let c = self
            .request_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (c.hits, c.misses, c.bytes)
    }

    /// Makes all writes so far visible to search.
    pub fn refresh(&mut self) {
        self.refreshed_seq_no = self.next_seq_no;
    }

    // ponytail: only refresh_interval -1 delays visibility; otherwise writes are searchable at once
    pub fn searchable(&self, seq_no: u64) -> bool {
        self.settings.get("index.refresh_interval") != Some("-1") || seq_no < self.refreshed_seq_no
    }

    /// A live document; one on disk is read from its segment.
    pub fn get(&self, id: &str) -> Option<Cow<'_, Document>> {
        if let Some(d) = self.docs.get(id) {
            return Some(Cow::Borrowed(d));
        }
        let disk = self.disk.as_ref()?;
        // ponytail: a document whose segment cannot be read is reported missing
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
        let mut memory: Vec<(&'a String, &'a Document)> = match in_memory {
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
            // ponytail: documents whose segment cannot be read are skipped
            .filter_map(Result::ok)
            .map(|(id, d)| (id, Cow::Owned(d)))
            .chain(
                memory
                    .into_iter()
                    .map(|(id, d)| (id.as_str(), Cow::Borrowed(d))),
            )
    }

    /// Every live document, without reading it: its position on disk or its id in memory.
    pub fn live(&self) -> (HashSet<disk::DiskPos>, HashSet<&str>) {
        let on_disk = self.disk_live().map(|(p, _)| p).collect();
        (on_disk, self.docs.keys().map(String::as_str).collect())
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

    /// The on-disk documents with a value of numeric or date `field` that `keep` accepts, from
    /// doc values (dates as epoch millis); `None` for other fields. With `exact`, only those
    /// that have one, or `None` if some segment has no doc values for the field to read.
    // ponytail: floats are skipped, since their f32 values compare differently from the source
    pub fn disk_numbers(
        &self,
        field: &str,
        keep: impl Fn(&Value) -> bool,
        exact: bool,
    ) -> Option<BTreeSet<disk::DiskPos>> {
        use FieldMapping as M;
        let decode = match self.mappings.properties.get(field)? {
            M::Long | M::Integer | M::Short | M::Byte | M::Date { .. } => Value::from,
            M::Double => |v| Value::from(sortable_double(v)),
            _ => return None,
        };
        self.disk.as_ref().map_or_else(
            || Some(BTreeSet::new()),
            |d| d.number_docs(field, |v| keep(&decode(v)), exact),
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
                M::Float => numbers(|v| {
                    let v = v as i32;
                    Value::from(f64::from(f32::from_bits(
                        (v ^ ((v >> 31) & i32::MAX)) as u32,
                    )))
                }),
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

    /// Sources of in-memory documents that were updated or deleted (see `superseded`).
    pub fn superseded(&self) -> &[Value] {
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

    /// Merges `partial` into an existing document, or indexes `upsert` if it is missing.
    /// `None` means the document is missing and there is no upsert.
    pub fn update(
        &mut self,
        id: &str,
        partial: &Value,
        upsert: Option<Value>,
        detect_noop: bool,
    ) -> Option<WriteResult> {
        let Some(doc) = self.get(id) else {
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
        drop(doc);
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
pub const VERSION_CREATED: i64 = es_codec::segment_infos::INDEX_VERSION_ID;

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

/// A Java DateTimeFormatter pattern as runs of a field letter (`y`, `M`, `d`, `H`, `m`, `s`,
/// `S`) and its width, or of literal text (width 0); `None` for other letters.
// ponytail: numeric fields only; no month or day names, zones, or optional sections
fn pattern_runs(pattern: &str) -> Option<Vec<(char, usize, String)>> {
    let mut runs: Vec<(char, usize, String)> = vec![];
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            'y' | 'M' | 'd' | 'H' | 'm' | 's' | 'S' => match runs.last_mut() {
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
pub fn parse_pattern(pattern: &str, mut s: &str) -> Option<i64> {
    let [mut y, mut mo, mut d, mut h, mut mi, mut sec, mut ms] = [1970, 1, 1, 0, 0, 0, 0];
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
            'y' => y = v,
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
            'y' if n == 2 => y.rem_euclid(100),
            'y' => y,
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
    let Some(mut s) = v.as_str() else {
        return v.as_i64();
    };
    let y = num(&mut s, 4)?;
    let [
        mut m,
        mut day,
        mut h,
        mut min,
        mut sec,
        mut milli,
        mut offset,
    ] = [1, 1, 0, 0, 0, 0, 0];
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
                    eat(&mut s, 'Z');
                } else {
                    let oh = num(&mut s, 2)?;
                    let om = if eat(&mut s, ':') { num(&mut s, 2)? } else { 0 };
                    offset = sign * (oh * 60 + om);
                }
            }
        }
    }
    if !s.is_empty() {
        return None;
    }
    let days = days_from_civil(y, m, day);
    Some(((days * 24 + h) * 60 + min - offset) * 60_000 + sec * 1000 + milli)
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
        let (hits, misses, bytes) = ix.request_cache_stats();
        assert_eq!((hits, misses), (1, 4));
        assert!(bytes > 0);
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
            FieldMapping::Keyword { ignore_above: None }
        );
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
            ("2024", 1_704_067_200_000),
            ("2024-01", 1_704_067_200_000),
            ("2024-01-02T03", 1_704_164_400_000),
            ("2024-01-02T03:04:05+01:00", 1_704_161_045_000),
            ("2024-01-02T03:04:05-01", 1_704_168_245_000),
            ("2024-01-02T03:04:05,5Z", 1_704_164_645_500),
        ] {
            let mut i = Index::default();
            i.map_dynamic(&json!({ "d": s }));
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

    #[test]
    fn disk_values_read_the_doc_values_of_on_disk_documents() {
        let dir = std::env::temp_dir().join(format!("esm-disk-values-{}", std::process::id()));
        let mut ix = Index::default();
        let props = &mut ix.mappings.properties;
        props.insert("n".into(), FieldMapping::Long);
        props.insert("d".into(), FieldMapping::Double);
        props.insert("f".into(), FieldMapping::Float);
        props.insert("k".into(), FieldMapping::Keyword { ignore_above: None });
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
