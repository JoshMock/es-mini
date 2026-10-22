//! Loads indices from, and saves them to, an Elasticsearch data directory.
//!
//! An opened index serves its documents from the segments: only ids, versions and term
//! statistics are read into memory, and a `_source` is read from disk when a document is.

use crate::{Document, Index, Indices, Mappings, TermStats};
use es_codec::{
    SegmentInfos, SegmentReader,
    cluster_state::read_indices,
    doc_values::DocValues,
    field_infos::IndexOptions,
    stored_fields::{Doc, StoredValue},
    uid::decode_id,
};
use serde::Deserialize;
use serde_json::Value;
use std::{
    borrow::Cow,
    collections::{BTreeSet, HashMap, HashSet},
    path::Path,
};

/// Opens every index of `data_dir` (single shard 0).
pub fn open(data_dir: &Path) -> es_codec::Result<Indices> {
    let mut out = Indices::new();
    for meta in read_indices(data_dir)? {
        let mut index = Index::default();
        let mut settings = serde_json::Map::new();
        settings.extend(
            meta.settings
                .into_iter()
                .map(|(k, v)| (k, Value::String(v))),
        );
        index.settings.merge(&Value::Object(settings));
        if let Some(m) = meta.mappings {
            let m: Value = serde_json::from_str(&m).map_err(|e| es_codec::Error(e.to_string()))?;
            index.mappings = Mappings::deserialize(&m["_doc"]).unwrap_or_default();
        }
        let dir = data_dir.join("indices").join(&meta.uuid).join("0/index");
        // ponytail: indices in codecs we lack (e.g. Zstd stored fields) are left out, not fatal
        if load_shard(&mut index, &dir).is_ok() {
            replay_translog(&mut index, &dir.join("../translog"))?;
            out.insert(meta.name, index);
        }
    }
    Ok(out)
}

/// Re-applies the operations in a shard's translog that its commit does not hold: those above
/// the commit's `local_checkpoint`, up to the checkpoint's offset in the current generation.
fn replay_translog(index: &mut Index, dir: &Path) -> es_codec::Result<()> {
    use es_codec::translog::{read, read_checkpoint};
    let Ok(bytes) = std::fs::read(dir.join("translog.ckp")) else {
        return Ok(());
    };
    let cp = read_checkpoint(&bytes)?;
    let committed = index
        .commit
        .get("local_checkpoint")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(-1);
    for generation in cp.min_generation..=cp.generation {
        let path = dir.join(format!("translog-{generation}.tlog"));
        let file = std::fs::read(&path)
            .map_err(|e| es_codec::Error(format!("{}: {e}", path.display())))?;
        let durable = if generation == cp.generation {
            (cp.offset as usize).min(file.len())
        } else {
            file.len()
        };
        for op in read(&file[..durable])?.1 {
            if op.seq_no() > committed {
                index.replay(op);
            }
        }
    }
    Ok(())
}

/// A document's place in an opened index's segments: `(segment, doc id)`.
pub type DiskPos = (usize, usize);

/// Where a live document of a [`Shard`] is, with its version and sequence number.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DiskDoc {
    segment: usize,
    doc: usize,
    pub(crate) version: u64,
    pub(crate) seq_no: u64,
}

/// The segments of a shard opened from disk. Documents stay in them until read.
pub(crate) struct Shard {
    segments: Vec<SegmentReader>,
    /// Live documents by id. Writes to the index remove the ones they replace or delete.
    ids: HashMap<String, DiskDoc>,
}

impl std::fmt::Debug for Shard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shard")
            .field("segments", &self.segments.len())
            .field("docs", &self.ids.len())
            .finish()
    }
}

/// The `_id` and `_source` bytes of a document's stored fields.
fn id_and_source(
    seg: &SegmentReader,
    fields: Doc,
) -> es_codec::Result<(Option<String>, Option<Vec<u8>>)> {
    let (mut id, mut source) = (None, None);
    for (n, v) in fields {
        match (seg.field_by_number(n)?.name.as_str(), v) {
            ("_id", StoredValue::Bytes(b)) => id = Some(decode_id(&b)),
            ("_source", StoredValue::Bytes(b)) => source = Some(b),
            _ => {}
        }
    }
    Ok((id, source))
}

fn document(d: &DiskDoc, source: Option<Vec<u8>>) -> es_codec::Result<Document> {
    let source = source.ok_or_else(|| es_codec::Error("document has no _source".into()))?;
    Ok(Document {
        source: serde_json::from_slice(&source).map_err(|e| es_codec::Error(e.to_string()))?,
        version: d.version,
        seq_no: d.seq_no,
    })
}

impl Shard {
    pub(crate) fn len(&self) -> usize {
        self.ids.len()
    }

    pub(crate) fn meta(&self, id: &str) -> Option<DiskDoc> {
        self.ids.get(id).copied()
    }

    /// Forgets a document the index replaced or deleted.
    pub(crate) fn remove(&mut self, id: &str) -> Option<DiskDoc> {
        self.ids.remove(id)
    }

    /// Reads a live document from its segment.
    pub(crate) fn load(&self, d: &DiskDoc) -> es_codec::Result<Document> {
        let seg = &self.segments[d.segment];
        document(d, id_and_source(seg, seg.document(d.doc)?)?.1)
    }

    /// The docs that may hold `term` in `field`: those whose postings do, and every doc of a
    /// segment without readable postings for the field. Some may no longer be live.
    pub(crate) fn term_docs(&self, field: &str, term: &str) -> BTreeSet<DiskPos> {
        if field == "_id" {
            return self
                .meta(term)
                .map(|d| (d.segment, d.doc))
                .into_iter()
                .collect();
        }
        let mut out = BTreeSet::new();
        for (s, seg) in self.segments.iter().enumerate() {
            match seg.term(field, term.as_bytes()) {
                Ok(t) => out.extend(t.iter().flat_map(|t| &t.docs).map(|d| (s, d.0 as usize))),
                // e.g. a field this segment does not index, or a postings format we lack
                Err(_) => out.extend((0..seg.max_doc()).map(|d| (s, d))),
            }
        }
        out
    }

    /// The docs with a value of numeric `field` that `keep` accepts, from doc values, and every
    /// doc of a segment without readable doc values for the field. Some may no longer be live.
    // ponytail: decodes the whole doc values column; walk the points tree if that shows up
    pub(crate) fn number_docs(&self, field: &str, keep: impl Fn(i64) -> bool) -> BTreeSet<DiskPos> {
        let mut out = BTreeSet::new();
        for (s, seg) in self.segments.iter().enumerate() {
            match seg.doc_values(field) {
                Ok(DocValues::Numbers(n)) => out.extend(
                    (n.iter().enumerate())
                        .filter(|(_, v)| v.iter().any(|v| keep(*v)))
                        .map(|(d, _)| (s, d)),
                ),
                _ => out.extend((0..seg.max_doc()).map(|d| (s, d))),
            }
        }
        out
    }

    /// The live documents at a position `keep` accepts, segment by segment, read a chunk at a
    /// time; chunks without such documents are not read.
    pub(crate) fn docs<'a: 'k, 'k>(
        &'a self,
        keep: impl Fn(DiskPos) -> bool + Copy + 'k,
    ) -> impl Iterator<Item = es_codec::Result<(&'a str, Document)>> + 'k {
        self.segments.iter().enumerate().flat_map(move |(s, seg)| {
            let docs: Box<dyn Iterator<Item = _>> = match seg.documents_where(move |d| keep((s, d)))
            {
                Ok(docs) => Box::new(docs),
                Err(e) => Box::new(std::iter::once(Err(e))),
            };
            docs.filter_map(move |fields| {
                let read = || {
                    let (doc, fields) = fields?;
                    let (Some(id), source) = id_and_source(seg, fields)? else {
                        return Ok(None);
                    };
                    let Some((id, d)) = self.ids.get_key_value(&id) else {
                        return Ok(None);
                    };
                    if (d.segment, d.doc) != (s, doc) {
                        return Ok(None);
                    }
                    Ok(Some((id.as_str(), document(d, source)?)))
                };
                read().transpose()
            })
        })
    }
}

fn longs(seg: &SegmentReader, field: &str) -> Vec<Option<i64>> {
    match seg.doc_values(field) {
        Ok(DocValues::Numbers(n)) => n.iter().map(|v| v.first().copied()).collect(),
        _ => vec![],
    }
}

fn load_shard(index: &mut Index, dir: &Path) -> es_codec::Result<()> {
    let mut stats = TermStats::default();
    let infos = SegmentInfos::read_latest(dir)?;
    // Operations after the last live doc (deletes) still consumed sequence numbers.
    if let Some(max) = infos
        .user_data
        .get("max_seq_no")
        .and_then(|m| m.parse::<u64>().ok())
    {
        index.next_seq_no = index.next_seq_no.max(max + 1);
    }
    index.commit = infos.user_data;
    let mut shard = Shard {
        segments: vec![],
        ids: HashMap::new(),
    };
    for seg in infos.segments {
        let seg = SegmentReader::open(dir, seg)?;
        for f in seg
            .fields
            .iter()
            .filter(|f| f.index_options != IndexOptions::None && f.name != "_id")
        {
            // ponytail: fields in formats the codec lacks (e.g. ES812Postings `_id`) get no stats
            let Ok(terms) = seg.terms(&f.name) else {
                continue;
            };
            let mut docs = HashSet::new();
            let entry = stats.fields.entry(f.name.clone()).or_default();
            for t in terms {
                entry.1 += t.total_term_freq as f64;
                docs.extend(t.docs.iter().map(|d| d.0));
                *stats
                    .doc_freq
                    .entry((
                        f.name.clone(),
                        String::from_utf8_lossy(&t.term).into_owned(),
                    ))
                    .or_default() += t.docs.len() as f64;
            }
            entry.0 += docs.len() as f64;
        }
        let (versions, seq_nos) = (longs(&seg, "_version"), longs(&seg, "_seq_no"));
        let live = seg.live_docs()?;
        let segment = shard.segments.len();
        let mut add = |id: String, doc: usize| {
            if !live[doc] {
                return;
            }
            let at = |v: &[Option<i64>]| v.get(doc).copied().flatten().unwrap_or(0) as u64;
            let d = DiskDoc {
                segment,
                doc,
                version: at(&versions),
                seq_no: at(&seq_nos),
            };
            index.next_seq_no = index.next_seq_no.max(d.seq_no + 1);
            shard.ids.insert(id, d);
        };
        // Ids come from the `_id` postings, or from the stored fields where the codec cannot
        // read those (Elasticsearch writes `_id` in ES812Postings).
        match seg.terms("_id") {
            Ok(terms) => {
                for t in terms {
                    let id = decode_id(&t.term);
                    t.docs.iter().for_each(|d| add(id.clone(), d.0 as usize));
                }
            }
            Err(_) => {
                for (doc, fields) in seg.documents(0..seg.max_doc())?.enumerate() {
                    if let (Some(id), Some(_)) = id_and_source(&seg, fields?)? {
                        add(id, doc);
                    }
                }
            }
        }
        shard.segments.push(seg);
    }
    index.stats = Some(stats);
    index.disk = Some(shard);
    Ok(())
}

/// The documents of `ix` that the saved segments do not hold: all but the `current` ids.
fn unsaved<'a>(
    ix: &'a Index,
    current: &'a HashSet<String>,
) -> es_codec::Result<Vec<(&'a str, Cow<'a, Document>)>> {
    let mut out: Vec<_> = ix
        .docs
        .iter()
        .filter(|(id, _)| !current.contains(*id))
        .map(|(id, d)| (id.as_str(), Cow::Borrowed(d)))
        .collect();
    if let Some(disk) = &ix.disk {
        let at: HashSet<DiskPos> = (disk.ids.iter())
            .filter(|(id, _)| !current.contains(*id))
            .map(|(_, d)| (d.segment, d.doc))
            .collect();
        if at.is_empty() {
            return Ok(out);
        }
        for doc in disk.docs(|p| at.contains(&p)) {
            let (id, d) = doc?;
            out.push((id, Cow::Owned(d)));
        }
    }
    Ok(out)
}

/// Lucene's `SmallFloat.intToByte4`: the norm byte for a field of `len` terms.
pub fn norm_byte(len: u32) -> i8 {
    let int4 = |i: u32| {
        let bits = 32 - i.leading_zeros();
        if bits < 4 {
            i
        } else {
            let shift = bits - 4;
            ((i >> shift) & 7) | ((shift + 1) << 3)
        }
    };
    (if len < 24 { len } else { 24 + int4(len - 24) }) as u8 as i8
}

/// Lucene's `SmallFloat.byte4ToInt`: the field length a norm byte stands for.
pub fn norm_len(norm: i8) -> u32 {
    let b = norm as u8 as u32;
    if b < 24 {
        b
    } else {
        let i = b - 24;
        24 + match i >> 3 {
            0 => i,
            s => ((i & 7) | 8) << (s - 1),
        }
    }
}

/// Index data for one shard segment, built from the mapped fields whose values are scalars or
/// arrays of scalars. Field numbers start at 6, after the metadata fields.
// ponytail: fields the writer cannot encode yet are left out of the segment, which only
// Elasticsearch notices (es-mini rebuilds every field from `_source` on open): values of the
// wrong type or nested objects, custom date formats, positions, and any field with a term in
// 256+ docs.
#[derive(Default)]
struct UserFields {
    fields: Vec<es_codec::field_infos::FieldInfo>,
    sorted_numeric: Vec<(i32, Vec<Vec<i64>>)>,
    keyword: Vec<(i32, Vec<Vec<Vec<u8>>>)>,
    norms: Vec<(i32, Vec<Option<i8>>)>,
    terms: Vec<(i32, Vec<es_codec::terms::TermPostings>)>,
    points: Vec<(i32, es_codec::points::PointValues)>,
}

/// Converts every value, or `None` if any cannot be.
fn typed<T>(values: &[Vec<&Value>], f: &dyn Fn(&Value) -> Option<T>) -> Option<Vec<Vec<T>>> {
    values
        .iter()
        .map(|vs| vs.iter().map(|v| f(v)).collect())
        .collect()
}

fn user_fields(
    mappings: &Mappings,
    docs: &[(&str, &Document)],
    meta: &[es_codec::field_infos::FieldInfo],
) -> UserFields {
    use crate::{FieldMapping as M, analysis::Analyzer};
    use es_codec::{field_infos::DocValuesType, terms::TermPostings};
    use std::collections::BTreeMap;
    let mut out = UserFields::default();
    for (name, mapping) in &mappings.properties {
        // Values per doc: arrays flatten, nulls and absent fields are no values.
        let values: Vec<Vec<&Value>> = docs
            .iter()
            .map(|(_, d)| match d.source.get(name) {
                Some(Value::Array(a)) => a.iter().filter(|v| !v.is_null()).collect(),
                Some(v) if !v.is_null() => vec![v],
                _ => vec![],
            })
            .collect();
        let flat = || values.iter().flatten();
        if flat().next().is_none() || flat().any(|v| v.is_array() || v.is_object()) {
            continue;
        }
        let number = (meta.len() + out.fields.len()) as i32;
        let postings = |keys: Vec<Vec<Vec<u8>>>| -> Vec<TermPostings> {
            let mut m = BTreeMap::<Vec<u8>, Vec<(u32, u32)>>::new();
            for (doc, terms) in keys.into_iter().enumerate() {
                let mut freq = BTreeMap::<Vec<u8>, u32>::new();
                for t in terms {
                    *freq.entry(t).or_default() += 1;
                }
                for (t, f) in freq {
                    m.entry(t).or_default().push((doc as u32, f));
                }
            }
            m.into_iter()
                .map(|(term, docs)| TermPostings::new(term, docs))
                .collect()
        };
        // Docs index a term once however often it repeats (docs-only postings have no freqs).
        let dedup = |mut keys: Vec<Vec<Vec<u8>>>| {
            keys.iter_mut().for_each(|k| {
                k.sort();
                k.dedup();
            });
            keys
        };
        let mut point_width = 0;
        let (index_options, doc_values, omit_norms) = match mapping {
            M::Text | M::TextKeyword { .. } => {
                let Some(texts) = typed(&values, &|v| v.as_str().map(str::to_owned)) else {
                    continue;
                };
                let toks: Vec<Vec<Vec<u8>>> = texts
                    .iter()
                    .map(|ts| {
                        ts.iter()
                            .flat_map(|t| Analyzer::Standard.analyze(t))
                            .map(String::into_bytes)
                            .collect()
                    })
                    .collect();
                let terms = postings(toks.clone());
                if terms.is_empty() {
                    continue;
                }
                out.norms.push((
                    number,
                    toks.iter()
                        .map(|t| (!t.is_empty()).then(|| norm_byte(t.len() as u32)))
                        .collect(),
                ));
                out.terms.push((number, terms));
                (IndexOptions::DocsAndFreqs, DocValuesType::None, false)
            }
            M::Keyword { .. } => {
                let Some(keys) = typed(&values, &|v| v.as_str().map(|s| s.as_bytes().to_vec()))
                else {
                    continue;
                };
                let keys = dedup(keys);
                out.terms.push((number, postings(keys.clone())));
                out.keyword.push((number, keys));
                (IndexOptions::Docs, DocValuesType::SortedSet, true)
            }
            M::Boolean => {
                let Some(bools) = typed(&values, &|v| v.as_bool()) else {
                    continue;
                };
                let keys = dedup(
                    bools
                        .iter()
                        .map(|bs| {
                            bs.iter()
                                .map(|b| if *b { b"T".to_vec() } else { b"F".to_vec() })
                                .collect()
                        })
                        .collect(),
                );
                out.terms.push((number, postings(keys)));
                out.sorted_numeric.push((
                    number,
                    bools
                        .iter()
                        .map(|bs| bs.iter().map(|b| i64::from(*b)).collect())
                        .collect(),
                ));
                (IndexOptions::Docs, DocValuesType::SortedNumeric, true)
            }
            M::Long | M::Integer | M::Short | M::Byte | M::Double | M::Float | M::Date { .. } => {
                // Doc values are the sortable longs ES stores; points are their big-endian bytes.
                let sortable_long = |f: f64| {
                    let b = f.to_bits() as i64;
                    b ^ ((b >> 63) & i64::MAX)
                };
                let sortable_int = |f: f32| {
                    let b = f.to_bits() as i32;
                    i64::from(b ^ ((b >> 31) & i32::MAX))
                };
                let (width, nums) = match mapping {
                    M::Long => (8, typed(&values, &|v| v.as_i64())),
                    M::Double => (8, typed(&values, &|v| v.as_f64().map(sortable_long))),
                    M::Float => (
                        4,
                        typed(&values, &|v| v.as_f64().map(|f| sortable_int(f as f32))),
                    ),
                    M::Date { .. } => (8, typed(&values, &crate::parse_date_millis)),
                    _ => (4, typed(&values, &|v| v.as_i64())),
                };
                let Some(nums) = nums else {
                    continue;
                };
                out.points.push((
                    number,
                    nums.iter()
                        .enumerate()
                        .flat_map(|(doc, ns)| {
                            ns.iter().map(move |n| {
                                (doc as u32, es_codec::points::sortable_bytes(*n, width))
                            })
                        })
                        .collect(),
                ));
                point_width = width as i32;
                out.sorted_numeric.push((number, nums));
                (IndexOptions::None, DocValuesType::SortedNumeric, true)
            }
        };
        let mut attributes = BTreeMap::new();
        let mut copy = |from: &str, kind: &str| {
            for k in ["format", "suffix"] {
                let key = format!("PerField{kind}Format.{k}");
                attributes.insert(key.clone(), meta_attr(from, &key, meta));
            }
        };
        if index_options != IndexOptions::None {
            copy("_id", "Postings");
        }
        if doc_values != DocValuesType::None {
            copy("_version", "DocValues");
        }
        out.fields.push(es_codec::field_infos::FieldInfo {
            name: name.clone(),
            number,
            omit_norms,
            index_options,
            doc_values,
            attributes,
            point_dimensions: i32::from(point_width > 0),
            point_index_dimensions: i32::from(point_width > 0),
            point_num_bytes: point_width,
            ..meta[2].clone()
        });
    }
    out
}

fn meta_attr(field: &str, key: &str, meta: &[es_codec::field_infos::FieldInfo]) -> String {
    meta.iter()
        .find(|f| f.name == field)
        .expect("meta field")
        .attributes[key]
        .clone()
}

/// The directory beside `data_dir` used while swapping snapshots (`<data_dir>.esmini-<kind>`).
fn sibling(data_dir: &Path, kind: &str) -> std::path::PathBuf {
    let name = data_dir
        .file_name()
        .map_or_else(Default::default, |n| n.to_string_lossy().into_owned());
    data_dir.with_file_name(format!("{name}.esmini-{kind}"))
}

fn io(e: std::io::Error) -> es_codec::Error {
    es_codec::Error(e.to_string())
}

fn remove_dir_if_exists(dir: &Path) -> es_codec::Result<()> {
    match std::fs::remove_dir_all(dir) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(io(e)),
        _ => Ok(()),
    }
}

fn copy_recursive(from: &Path, to: &Path) -> es_codec::Result<()> {
    if from.is_dir() {
        std::fs::create_dir_all(to).map_err(io)?;
        for entry in std::fs::read_dir(from).map_err(io)? {
            let entry = entry.map_err(io)?;
            copy_recursive(&entry.path(), &to.join(entry.file_name()))?;
        }
        return Ok(());
    }
    std::fs::copy(from, to).map(drop).map_err(io)
}

/// Finishes or undoes a [`save`] interrupted by a crash: restores the previous snapshot if it was
/// moved aside but not replaced, and discards any half-written new one.
pub fn recover(data_dir: &Path) -> es_codec::Result<()> {
    let old = sibling(data_dir, "old");
    if !data_dir.exists() && old.exists() {
        std::fs::rename(&old, data_dir).map_err(io)?;
    }
    remove_dir_if_exists(&sibling(data_dir, "new"))?;
    remove_dir_if_exists(&old)
}

/// Replaces `data_dir` with a snapshot of `indices` without ever leaving it half-written: the
/// snapshot is built beside it, then swapped in by two renames that [`recover`] can complete.
/// Other top-level entries (an Elasticsearch node's `nodes/`, `node.lock`, ...) carry over.
/// Saves are serialized process-wide. Each index's previous segments are hard-linked, not
/// rewritten: a save writes one new segment of the documents changed since the last save.
pub fn save(data_dir: &Path, indices: &Indices) -> es_codec::Result<()> {
    let _one_at_a_time = SAVING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    commit(data_dir, indices)
}

/// Serializes saves and persists process-wide.
static SAVING: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Translog operations after which [`persist`] commits segments (Elasticsearch's flush), bounding
/// how much a restart replays.
// ponytail: an operation count; Elasticsearch flushes by translog size (512mb) and on idle
pub const FLUSH_OPS: usize = 5000;

/// Makes the writes since the last call durable, like Elasticsearch: document writes are
/// appended to each shard's translog (one fsynced append and an in-place checkpoint, per
/// index written to), and segments are committed only when cluster metadata changed or a
/// translog reaches [`FLUSH_OPS`]. [`open`] replays the translog.
pub fn persist(data_dir: &Path, indices: &mut Indices) -> es_codec::Result<()> {
    let _one_at_a_time = SAVING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let metas = metas(indices);
    let unchanged = read_indices(data_dir).is_ok_and(|saved| saved == metas);
    let mut logs = vec![];
    if unchanged {
        for (meta, ix) in metas.iter().zip(indices.values()) {
            let dir = shard_dir(data_dir, &meta.uuid).join("../translog");
            let Ok(cp) = std::fs::read(dir.join("translog.ckp"))
                .map_err(io)
                .and_then(|b| es_codec::translog::read_checkpoint(&b))
            else {
                logs.clear();
                break;
            };
            logs.push((dir, cp, ix.unlogged_len()));
        }
    }
    let flush = !unchanged
        || logs.len() != indices.len()
        || logs
            .iter()
            .any(|(_, cp, n)| cp.num_ops as usize + n >= FLUSH_OPS);
    if flush {
        commit(data_dir, indices)?;
        indices.values_mut().for_each(|ix| drop(ix.take_unlogged()));
        return Ok(());
    }
    for ((dir, cp, _), ix) in logs.into_iter().zip(indices.values_mut()) {
        let ops = ix.take_unlogged();
        if !ops.is_empty() {
            append(&dir, cp, &ops)?;
        }
    }
    Ok(())
}

/// Appends `ops` to the current translog generation in `dir` (overwriting any torn bytes past the
/// checkpoint), fsyncs it, then rewrites the checkpoint in place, as Elasticsearch does.
fn append(
    dir: &Path,
    mut cp: es_codec::translog::TranslogCheckpoint,
    ops: &[es_codec::translog::Op],
) -> es_codec::Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    let bytes: Vec<u8> = ops.iter().flat_map(es_codec::translog::encode).collect();
    let path = dir.join(format!("translog-{}.tlog", cp.generation));
    let write = || -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new().write(true).open(&path)?;
        f.set_len(cp.offset as u64)?;
        f.seek(SeekFrom::End(0))?;
        f.write_all(&bytes)?;
        f.sync_all()
    };
    write().map_err(|e| es_codec::Error(format!("{}: {e}", path.display())))?;
    let seq_nos = ops.iter().map(|o| o.seq_no());
    let (min, max) = (
        seq_nos.clone().min().unwrap_or(-1),
        seq_nos.max().unwrap_or(-1),
    );
    if cp.num_ops == 0 {
        cp.min_seq_no = min;
    }
    cp.offset += bytes.len() as i64;
    cp.num_ops += ops.len() as i32;
    cp.min_seq_no = cp.min_seq_no.min(min);
    cp.max_seq_no = cp.max_seq_no.max(max);
    cp.global_checkpoint = cp.global_checkpoint.max(max);
    // A fixed-size record rewritten in place and fsynced, like Elasticsearch's Checkpoint.write.
    let ckp = dir.join("translog.ckp");
    let write = || -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new().write(true).open(&ckp)?;
        f.write_all(&es_codec::translog::checkpoint(&cp))?;
        f.sync_all()
    };
    write().map_err(|e| es_codec::Error(format!("{}: {e}", ckp.display())))
}

/// Commits every index's documents into segments (see [`save`]); the caller holds `SAVING`.
fn commit(data_dir: &Path, indices: &Indices) -> es_codec::Result<()> {
    if save_in_place(data_dir, indices)? {
        return Ok(());
    }
    let (new, old) = (sibling(data_dir, "new"), sibling(data_dir, "old"));
    remove_dir_if_exists(&new)?;
    std::fs::create_dir_all(&new).map_err(io)?;
    if data_dir.is_dir() {
        for entry in std::fs::read_dir(data_dir).map_err(io)? {
            let entry = entry.map_err(io)?;
            if !matches!(entry.file_name().to_str(), Some("_state" | "indices")) {
                copy_recursive(&entry.path(), &new.join(entry.file_name()))?;
            }
        }
    }
    write_snapshot(&new, indices, data_dir)?;
    if data_dir.exists() {
        std::fs::rename(data_dir, &old).map_err(io)?;
    }
    std::fs::rename(&new, data_dir).map_err(io)?;
    remove_dir_if_exists(&old)
}

/// Marks the segments es-mini writes, the only ones a later save reuses: Elasticsearch indexes
/// some fields differently (`_seq_no` points, text positions) and Lucene rejects a field with
/// two schemas, so an Elasticsearch-written index is rewritten whole on its first save.
const WRITER: (&str, &str) = ("es-mini.writer", "1");

// ponytail: an index with this many segments is rewritten as one, a crude stand-in for Lucene's
// TieredMergePolicy; merge segments selectively if the periodic O(docs) rewrite shows up
const MAX_SEGMENTS: usize = 10;

fn link(from: &Path, to: &Path) -> es_codec::Result<()> {
    std::fs::hard_link(from, to)
        .or_else(|_| std::fs::copy(from, to).map(drop))
        .map_err(io)
}

/// Carries the previous commit of an index (in `prev`) over to `dir`: segments that still hold
/// a live doc are hard-linked, with docs that `ix` replaced or deleted marked in new live docs.
/// When `prev` is `dir` (an in-place commit) segments stay where they are. Returns that commit,
/// the ids whose saved copy is current, and whether any saved doc was replaced or deleted; or
/// `None` when the index must be written whole (no readable previous commit, too many segments,
/// or not written by es-mini).
fn reuse_segments(
    prev: &Path,
    dir: &Path,
    ix: &Index,
) -> es_codec::Result<Option<(SegmentInfos, HashSet<String>, bool)>> {
    use es_codec::{live_docs::write_liv, segment_infos::radix36};
    let Ok(mut infos) = SegmentInfos::read_latest(prev) else {
        return Ok(None);
    };
    let ours = |s: &es_codec::SegmentCommitInfo| {
        s.info.diagnostics.get(WRITER.0).map(String::as_str) == Some(WRITER.1)
    };
    if infos.segments.len() >= MAX_SEGMENTS || !infos.segments.iter().all(ours) {
        return Ok(None);
    }
    let in_place = prev == dir;
    let mut current = HashSet::new();
    let mut kept = vec![];
    let mut changed = false;
    for mut seg in std::mem::take(&mut infos.segments) {
        let r = SegmentReader::open(prev, seg.clone())?;
        let seq_nos = longs(&r, "_seq_no");
        let mut live = r.live_docs()?;
        let dead_before = live.iter().filter(|l| !**l).count();
        for t in r.terms("_id")? {
            let id = decode_id(&t.term);
            for doc in t.docs.iter().map(|d| d.0 as usize) {
                if !live[doc] {
                    continue;
                }
                let saved = seq_nos.get(doc).copied().flatten();
                if ix
                    .meta(&id)
                    .is_some_and(|(_, seq_no)| Some(seq_no as i64) == saved)
                {
                    current.insert(id.clone());
                } else {
                    live[doc] = false;
                }
            }
        }
        let dead = live.iter().filter(|l| !**l).count();
        changed |= dead > dead_before;
        if dead == live.len() {
            continue;
        }
        for f in seg
            .info
            .files
            .iter()
            .filter(|f| !in_place && !f.ends_with(".si"))
        {
            link(&prev.join(f), &dir.join(f))?;
        }
        let liv = |g: i64| format!("{}_{}.liv", seg.name, radix36(g as u64));
        if dead > dead_before {
            seg.del_gen = seg.del_gen.max(0) + 1;
            seg.del_count = dead as i32;
            let bytes = write_liv(&live, &seg.info.id, seg.del_gen);
            // Synced into the directory by the commit (`SegmentInfos::write`).
            es_codec::write_file_new(&dir.join(liv(seg.del_gen)), &bytes)?;
        } else if seg.del_gen != -1 && !in_place {
            link(&prev.join(liv(seg.del_gen)), &dir.join(liv(seg.del_gen)))?;
        }
        kept.push(seg);
    }
    infos.segments = kept;
    Ok(Some((infos, current, changed)))
}

/// Writes segment number `counter` of `dir` holding `docs`' `_id`, `_source`, `_version`,
/// `_seq_no` and mapped fields.
fn write_docs(
    dir: &Path,
    counter: i64,
    docs: &[(&str, &Document)],
    mappings: &Mappings,
    fields: &[es_codec::field_infos::FieldInfo],
) -> es_codec::Result<es_codec::SegmentCommitInfo> {
    use es_codec::{
        segment_infos::radix36,
        segment_writer::{NewSegment, write_segment},
        terms::TermPostings,
        uid::encode_id,
    };
    let stored: Vec<_> = docs
        .iter()
        .map(|(id, d)| {
            vec![
                (0, StoredValue::Bytes(encode_id(id))),
                (1, StoredValue::Bytes(d.source.to_string().into_bytes())),
            ]
        })
        .collect();
    let col = |f: fn(&Document) -> u64| docs.iter().map(|(_, d)| f(d) as i64).collect();
    let numeric = [
        (2, col(|d| d.version)),
        (3, col(|d| d.seq_no)),
        (4, vec![1; docs.len()]),
        (5, vec![]),
    ];
    let mut ids: Vec<_> = docs
        .iter()
        .enumerate()
        .map(|(i, (id, _))| (encode_id(id), i as u32))
        .collect();
    ids.sort();
    let ids = TermPostings::from_sorted(ids);
    let user = user_fields(mappings, docs, fields);
    let mut all_fields = fields.to_vec();
    all_fields.extend(user.fields);
    let mut terms = vec![(0, ids)];
    terms.extend(user.terms);
    // Unique per segment of an index; the first is the `[7; 16]` earlier saves always used.
    let mut id = [7; 16];
    id[8..].copy_from_slice(&(0x0707_0707_0707_0707 + counter as u64).to_be_bytes());
    let mut seg = write_segment(
        dir,
        &NewSegment {
            name: &format!("_{}", radix36(counter as u64)),
            id,
            max_doc: docs.len() as i32,
            fields: &all_fields,
            stored: &stored,
            numeric: &numeric,
            sorted_numeric: &user.sorted_numeric,
            keyword: &user.keyword,
            norms: &user.norms,
            terms: &terms,
            points: &user.points,
        },
    )?;
    seg.info
        .diagnostics
        .insert(WRITER.0.into(), WRITER.1.into());
    Ok(seg)
}

/// Cluster metadata for `indices`, as both save paths write it.
fn metas(indices: &Indices) -> Vec<es_codec::cluster_state::IndexMetadata> {
    let uuid = |name: &str| {
        let h = name.bytes().fold(0xcbf29ce484222325u64, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x100000001b3)
        });
        format!("{h:016x}{:06}", name.len() % 1_000_000)
    };
    indices
        .iter()
        .map(|(name, ix)| {
            // Indices read from an Elasticsearch directory keep its identity (index.uuid,
            // index.version.created); only indices es-mini created get generated values.
            let mut settings = ix.settings.flat().clone();
            let uuid = settings
                .entry("index.uuid".into())
                .or_insert_with(|| uuid(name))
                .clone();
            // Elasticsearch refuses to load index metadata without the creation version.
            settings
                .entry("index.version.created".into())
                .or_insert_with(|| crate::VERSION_CREATED.to_string());
            es_codec::cluster_state::IndexMetadata {
                name: name.clone(),
                uuid,
                settings,
                mappings: serde_json::to_string(&serde_json::json!({"_doc": ix.mappings})).ok(),
            }
        })
        .collect()
}

fn shard_dir(root: &Path, uuid: &str) -> std::path::PathBuf {
    root.join("indices").join(uuid).join("0/index")
}

/// Field infos of the metadata fields es-mini writes.
fn meta_fields() -> Vec<es_codec::field_infos::FieldInfo> {
    let mut fields = es_codec::segment_writer::es_meta_fields();
    fields[3].point_dimensions = 0;
    fields[3].point_index_dimensions = 0;
    fields[3].point_num_bytes = 0;
    fields
}

/// Commits `ix` into the shard directory `dir`, reusing the commit in `prev` (the same
/// directory for an in-place commit). An in-place commit of an unchanged index writes nothing,
/// and afterwards deletes the files no longer referenced. Returns whether it committed.
fn write_shard(prev: &Path, dir: &Path, ix: &Index) -> es_codec::Result<bool> {
    use es_codec::segment_infos::{radix36, shard_user_data};
    let in_place = prev == dir;
    let (mut infos, current, changed) = match reuse_segments(prev, dir, ix)? {
        Some(reused) => reused,
        None => (
            SegmentInfos {
                id: [1; 16],
                generation: 1,
                lucene_version: es_codec::segment_infos::LUCENE_VERSION,
                index_created_major: 10,
                version: 1,
                counter: 0,
                segments: vec![],
                user_data: Default::default(),
            },
            HashSet::new(),
            true,
        ),
    };
    let max_seq = ix.next_seq_no as i64 - 1;
    let mut owned = unsaved(ix, &current)?;
    let saved_max = infos
        .user_data
        .get("max_seq_no")
        .and_then(|v| v.parse::<i64>().ok());
    if in_place && !changed && owned.is_empty() && saved_max == Some(max_seq) {
        return Ok(false);
    }
    owned.sort_by_key(|(_, d)| d.seq_no);
    let docs: Vec<_> = owned.iter().map(|(id, d)| (*id, &**d)).collect();
    if !docs.is_empty() {
        infos.segments.push(write_docs(
            dir,
            infos.counter,
            &docs,
            &ix.mappings,
            &meta_fields(),
        )?);
        infos.counter += 1;
    }
    infos.generation += 1;
    infos.version += 1;
    infos.user_data = shard_user_data(
        "esminiHistoryUuid0000A",
        "esminiTranslogUuid000A",
        max_seq,
        max_seq,
    );
    // An index opened from disk continues that shard's history.
    for key in [
        "history_uuid",
        "translog_uuid",
        "max_unsafe_auto_id_timestamp",
    ] {
        if let Some(v) = ix.commit.get(key) {
            infos.user_data.insert(key.into(), v.clone());
        }
    }
    infos.write(dir)?;
    // The commit holds every operation, so the translog starts a new, empty generation.
    let translog = dir.join("../translog");
    let previous = std::fs::read(prev.join("../translog/translog.ckp")).ok();
    let generation = previous
        .and_then(|b| es_codec::translog::read_checkpoint(&b).ok())
        .map_or(1, |cp| cp.generation + 1);
    let cp = es_codec::translog::Checkpoint {
        generation,
        global_checkpoint: max_seq,
        primary_term: 1,
    };
    let (tlog, ckp) = es_codec::translog::write_empty(&infos.user_data["translog_uuid"], &cp);
    es_codec::write_file_atomic(&translog.join(format!("translog-{generation}.tlog")), &tlog)?;
    es_codec::write_file_atomic(&translog.join("translog.ckp"), &ckp)?;
    for entry in std::fs::read_dir(&translog).map_err(io)? {
        let name = entry
            .map_err(io)?
            .file_name()
            .to_string_lossy()
            .into_owned();
        if name.starts_with("translog-") && name != format!("translog-{generation}.tlog") {
            let _ = std::fs::remove_file(translog.join(&name));
        }
    }
    if in_place {
        // Like Lucene's IndexFileDeleter: drop the previous commit's files (and leftovers of an
        // interrupted commit) once the new commit is durable.
        let mut keep: HashSet<String> =
            HashSet::from([format!("segments_{}", radix36(infos.generation))]);
        for seg in &infos.segments {
            keep.extend(seg.info.files.iter().cloned());
            if seg.del_gen != -1 {
                keep.insert(format!("{}_{}.liv", seg.name, radix36(seg.del_gen as u64)));
            }
        }
        for entry in std::fs::read_dir(dir).map_err(io)? {
            let name = entry
                .map_err(io)?
                .file_name()
                .to_string_lossy()
                .into_owned();
            if (name.starts_with('_') || name.starts_with("segments_")) && !keep.contains(&name) {
                let _ = std::fs::remove_file(dir.join(&name));
            }
        }
    }
    Ok(true)
}

/// Commits a save that changes only documents directly into `data_dir`, one shard at a time,
/// with Lucene's commit protocol (see [`write_shard`]). Returns `false`, having written nothing,
/// when the save must rebuild the directory instead: no previous save, changed cluster metadata
/// (indices created, deleted, or reconfigured), or a shard es-mini cannot extend in place.
// ponytail: each shard commits on its own, so a crash during a write spanning indices (a bulk)
// can persist some of them, as with Elasticsearch's per-shard commits
fn save_in_place(data_dir: &Path, indices: &Indices) -> es_codec::Result<bool> {
    let metas = metas(indices);
    match read_indices(data_dir) {
        Ok(saved) if saved == metas => {}
        _ => return Ok(false),
    }
    let mut shards = vec![];
    for (meta, ix) in metas.iter().zip(indices.values()) {
        let dir = shard_dir(data_dir, &meta.uuid);
        let ours = SegmentInfos::read_latest(&dir).is_ok_and(|infos| {
            infos.segments.len() < MAX_SEGMENTS
                && infos
                    .segments
                    .iter()
                    .all(|s| s.info.diagnostics.get(WRITER.0).map(String::as_str) == Some(WRITER.1))
        });
        if !ours {
            return Ok(false);
        }
        shards.push((dir, ix));
    }
    for (dir, ix) in shards {
        write_shard(&dir, &dir, ix)?;
    }
    Ok(true)
}

/// Writes cluster metadata plus every index's shard into the empty directory `data_dir`,
/// reusing the segments of the snapshot at `prev` (see [`reuse_segments`]).
fn write_snapshot(data_dir: &Path, indices: &Indices, prev: &Path) -> es_codec::Result<()> {
    use es_codec::{cluster_state::GlobalMetadata, data_dir::write_empty_data_dir};
    let metas = metas(indices);
    let global = GlobalMetadata {
        cluster_uuid: "esminiClusterUuid00A".into(),
        node_id: "esminiNodeId0000000000".into(),
        term: 1,
    };
    write_empty_data_dir(data_dir, &global, &metas)?;
    for (meta, ix) in metas.iter().zip(indices.values()) {
        write_shard(
            &shard_dir(prev, &meta.uuid),
            &shard_dir(data_dir, &meta.uuid),
            ix,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_index_metadata_has_uuid_and_creation_version() {
        let dir = std::env::temp_dir().join(format!("esm-disk-meta-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut all = Indices::new();
        all.insert("books".into(), Index::default());
        save(&dir, &all).unwrap();
        let metas = read_indices(&dir).unwrap();
        assert_eq!(metas[0].settings["index.uuid"], metas[0].uuid);
        assert!(metas[0].settings.contains_key("index.version.created"));
    }
}

#[cfg(test)]
mod norm_tests {
    use super::*;

    #[test]
    fn norm_len_quantizes_like_lucene() {
        assert_eq!(norm_len(norm_byte(10)), 10);
        assert_eq!(norm_len(norm_byte(24)), 24);
        // 100 -> 24 + int4(76) -> decodes down to a bucket boundary
        assert_eq!(norm_len(norm_byte(100)), 96);
        assert_eq!(norm_len(norm_byte(1000)), 984);
    }
}
