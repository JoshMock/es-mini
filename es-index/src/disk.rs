//! Loads indices from, and saves them to, an Elasticsearch data directory.
//!
//! ponytail: live documents are copied into memory once at open; stream from the segments when
//! indices outgrow RAM.

use crate::{Document, Index, Indices, Mappings, TermStats};
use es_codec::{
    SegmentInfos, SegmentReader, cluster_state::read_indices, doc_values::DocValues,
    field_infos::IndexOptions, stored_fields::StoredValue, uid::decode_id,
};
use serde::Deserialize;
use serde_json::Value;
use std::{collections::HashSet, path::Path};

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
            out.insert(meta.name, index);
        }
    }
    Ok(out)
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
    for seg in infos.segments {
        let seg = SegmentReader::open(dir, seg)?;
        for f in seg
            .fields
            .iter()
            .filter(|f| f.index_options != IndexOptions::None)
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
        for (doc, fields) in seg.stored_fields()?.into_iter().enumerate() {
            if !live[doc] {
                continue;
            }
            let mut id = None;
            let mut source = None;
            for (n, v) in fields {
                match (seg.field_by_number(n)?.name.as_str(), v) {
                    ("_id", StoredValue::Bytes(b)) => id = Some(decode_id(&b)),
                    ("_source", StoredValue::Bytes(b)) => source = serde_json::from_slice(&b).ok(),
                    _ => {}
                }
            }
            let (Some(id), Some(source)) = (id, source) else {
                continue;
            };
            let at = |v: &[Option<i64>]| v.get(doc).copied().flatten().unwrap_or(0) as u64;
            index.insert_loaded(
                id,
                Document {
                    source,
                    version: at(&versions),
                    seq_no: at(&seq_nos),
                },
            );
        }
    }
    index.stats = Some(stats);
    Ok(())
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
    static SAVING: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _one_at_a_time = SAVING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
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
/// Returns that commit and the ids whose saved copy is current, or `None` when the index must be
/// written whole (no readable previous commit, too many segments, or not written by es-mini).
fn reuse_segments(
    prev: &Path,
    dir: &Path,
    ix: &Index,
) -> es_codec::Result<Option<(SegmentInfos, HashSet<String>)>> {
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
    let mut current = HashSet::new();
    let mut kept = vec![];
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
                if ix.get(&id).is_some_and(|d| Some(d.seq_no as i64) == saved) {
                    current.insert(id.clone());
                } else {
                    live[doc] = false;
                }
            }
        }
        let dead = live.iter().filter(|l| !**l).count();
        if dead == live.len() {
            continue;
        }
        for f in seg.info.files.iter().filter(|f| !f.ends_with(".si")) {
            link(&prev.join(f), &dir.join(f))?;
        }
        let liv = |g: i64| format!("{}_{}.liv", seg.name, radix36(g as u64));
        if dead > dead_before {
            seg.del_gen = seg.del_gen.max(0) + 1;
            seg.del_count = dead as i32;
            let bytes = write_liv(&live, &seg.info.id, seg.del_gen);
            es_codec::write_file_atomic(&dir.join(liv(seg.del_gen)), &bytes)?;
        } else if seg.del_gen != -1 {
            link(&prev.join(liv(seg.del_gen)), &dir.join(liv(seg.del_gen)))?;
        }
        kept.push(seg);
    }
    infos.segments = kept;
    Ok(Some((infos, current)))
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

/// Writes cluster metadata plus every index's shard into the empty directory `data_dir`,
/// reusing the segments of the snapshot at `prev` (see [`reuse_segments`]).
fn write_snapshot(data_dir: &Path, indices: &Indices, prev: &Path) -> es_codec::Result<()> {
    use es_codec::{
        cluster_state::{GlobalMetadata, IndexMetadata},
        data_dir::write_empty_data_dir,
        segment_infos::shard_user_data,
        segment_writer::es_meta_fields,
    };
    let uuid = |name: &str| {
        let h = name.bytes().fold(0xcbf29ce484222325u64, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x100000001b3)
        });
        format!("{h:016x}{:06}", name.len() % 1_000_000)
    };
    let metas: Vec<_> = indices
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
            IndexMetadata {
                name: name.clone(),
                uuid,
                settings,
                mappings: serde_json::to_string(&serde_json::json!({"_doc": ix.mappings})).ok(),
            }
        })
        .collect();
    let global = GlobalMetadata {
        cluster_uuid: "esminiClusterUuid00A".into(),
        node_id: "esminiNodeId0000000000".into(),
        term: 1,
    };
    write_empty_data_dir(data_dir, &global, &metas)?;
    let mut fields = es_meta_fields();
    fields[3].point_dimensions = 0;
    fields[3].point_index_dimensions = 0;
    fields[3].point_num_bytes = 0;
    for (meta, ix) in metas.iter().zip(indices.values()) {
        let shard = |root: &Path| root.join("indices").join(&meta.uuid).join("0/index");
        let dir = shard(data_dir);
        let (mut infos, current) = match reuse_segments(&shard(prev), &dir, ix)? {
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
            ),
        };
        let mut docs: Vec<_> = ix.docs().filter(|(id, _)| !current.contains(*id)).collect();
        if !docs.is_empty() {
            docs.sort_by_key(|(_, d)| d.seq_no);
            infos.segments.push(write_docs(
                &dir,
                infos.counter,
                &docs,
                &ix.mappings,
                &fields,
            )?);
            infos.counter += 1;
        }
        let max_seq = ix.next_seq_no as i64 - 1;
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
        infos.write(&dir)?;
        let cp = es_codec::translog::Checkpoint {
            generation: 1,
            global_checkpoint: max_seq,
            primary_term: 1,
        };
        let (tlog, ckp) = es_codec::translog::write_empty(&infos.user_data["translog_uuid"], &cp);
        let translog = dir.join("../translog");
        for (name, bytes) in [("translog-1.tlog", tlog), ("translog.ckp", ckp)] {
            es_codec::write_file_atomic(&translog.join(name), &bytes)?;
        }
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
