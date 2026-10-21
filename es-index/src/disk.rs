//! Loads indices from, and saves them to, an Elasticsearch data directory.
//!
//! ponytail: live documents are copied into memory once at open; stream from the segments when
//! indices outgrow RAM.

use crate::{Document, Index, Indices, Mappings, TermStats};
use es_codec::{
    SegmentInfos, SegmentReader, cluster_state::read_indices, doc_values::DocValues,
    field_infos::IndexOptions, stored_fields::StoredValue,
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

/// ES `Uid.encodeId` inverse.
fn decode_id(b: &[u8]) -> String {
    match b.first() {
        Some(0xfe) => b[1..]
            .iter()
            .flat_map(|x| [x >> 4, x & 0xf])
            .take_while(|n| *n != 0xf)
            .map(|n| char::from(b'0' + n))
            .collect(),
        Some(0xff) => String::from_utf8_lossy(&b[1..]).into_owned(),
        Some(0xfd) => b[1..]
            .chunks(3)
            .flat_map(|c| {
                const A: &[u8; 64] =
                    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
                let n = c
                    .iter()
                    .enumerate()
                    .fold(0u32, |n, (i, b)| n | u32::from(*b) << (16 - 8 * i));
                (0..=c.len()).map(move |i| char::from(A[(n >> (18 - 6 * i) & 63) as usize]))
            })
            .collect(),
        _ => String::from_utf8_lossy(b).into_owned(),
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
    for seg in SegmentInfos::read_latest(dir)?.segments {
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
fn norm_byte(len: u32) -> i8 {
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

/// Index data for one shard segment, built from the mapped fields that every doc has a scalar
/// value for. Field numbers start at 6, after the metadata fields.
// ponytail: fields the writer cannot encode yet are left out of the segment, which only
// Elasticsearch notices (es-mini rebuilds every field from `_source` on open): `date`/`float`,
// sparse or multi-valued fields, positions, and any field with a term in 256+ docs.
#[derive(Default)]
struct UserFields {
    fields: Vec<es_codec::field_infos::FieldInfo>,
    sorted_numeric: Vec<(i32, Vec<i64>)>,
    keyword: Vec<(i32, Vec<Vec<u8>>)>,
    norms: Vec<(i32, Vec<i8>)>,
    terms: Vec<(i32, Vec<es_codec::terms::TermPostings>)>,
    points: Vec<(i32, Vec<Vec<u8>>)>,
}

/// Whether es-codec can write these postings: terms in 256+ docs need full postings blocks.
fn writable(terms: &[es_codec::terms::TermPostings]) -> bool {
    terms
        .iter()
        .all(|t| t.docs.len() < es_codec::postings::BLOCK_SIZE)
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
        let values: Vec<_> = docs.iter().map(|(_, d)| d.source.get(name)).collect();
        let scalar = |v: &Option<&Value>| matches!(v, Some(v) if !v.is_array() && !v.is_object() && !v.is_null());
        if !values.iter().all(scalar) {
            continue;
        }
        let values: Vec<&Value> = values.into_iter().flatten().collect();
        let number = (meta.len() + out.fields.len()) as i32;
        let text = |v: &Value| v.as_str().map(str::to_owned);
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
                .map(|(term, docs)| TermPostings {
                    term,
                    total_term_freq: docs.iter().map(|d| u64::from(d.1)).sum(),
                    docs,
                    positions: vec![],
                })
                .collect()
        };
        let mut point_width = 0;
        let (index_options, doc_values, omit_norms) = match mapping {
            M::Text | M::TextKeyword { .. } => {
                let Some(texts) = values.iter().map(|v| text(v)).collect::<Option<Vec<_>>>() else {
                    continue;
                };
                let toks: Vec<Vec<Vec<u8>>> = texts
                    .iter()
                    .map(|t| {
                        Analyzer::Standard
                            .analyze(t)
                            .into_iter()
                            .map(String::into_bytes)
                            .collect()
                    })
                    .collect();
                let terms = postings(toks.clone());
                if terms.is_empty() || !writable(&terms) {
                    continue;
                }
                out.norms.push((
                    number,
                    toks.iter().map(|t| norm_byte(t.len() as u32)).collect(),
                ));
                out.terms.push((number, terms));
                (IndexOptions::DocsAndFreqs, DocValuesType::None, false)
            }
            M::Keyword => {
                let Some(keys) = values
                    .iter()
                    .map(|v| text(v).map(String::into_bytes))
                    .collect::<Option<Vec<_>>>()
                else {
                    continue;
                };
                let terms = postings(keys.iter().map(|k| vec![k.clone()]).collect());
                if !writable(&terms) {
                    continue;
                }
                out.terms.push((number, terms));
                out.keyword.push((number, keys));
                (IndexOptions::Docs, DocValuesType::SortedSet, true)
            }
            M::Boolean => {
                let Some(bools) = values
                    .iter()
                    .map(|v| v.as_bool())
                    .collect::<Option<Vec<_>>>()
                else {
                    continue;
                };
                let terms = postings(
                    bools
                        .iter()
                        .map(|b| vec![if *b { b"T".to_vec() } else { b"F".to_vec() }])
                        .collect(),
                );
                if !writable(&terms) {
                    continue;
                }
                out.terms.push((number, terms));
                out.sorted_numeric
                    .push((number, bools.iter().map(|b| i64::from(*b)).collect()));
                (IndexOptions::Docs, DocValuesType::SortedNumeric, true)
            }
            M::Long | M::Integer | M::Short | M::Byte => {
                let Some(ints) = values
                    .iter()
                    .map(|v| v.as_i64())
                    .collect::<Option<Vec<_>>>()
                else {
                    continue;
                };
                let width = if matches!(mapping, M::Long) { 8 } else { 4 };
                out.points.push((
                    number,
                    ints.iter()
                        .map(|v| es_codec::points::sortable_bytes(*v, width))
                        .collect(),
                ));
                point_width = width as i32;
                out.sorted_numeric.push((number, ints));
                (IndexOptions::None, DocValuesType::SortedNumeric, true)
            }
            M::Double => {
                let Some(fs) = values
                    .iter()
                    .map(|v| v.as_f64())
                    .collect::<Option<Vec<_>>>()
                else {
                    continue;
                };
                // NumericUtils.doubleToSortableLong
                let sortable = |f: f64| {
                    let b = f.to_bits() as i64;
                    b ^ ((b >> 63) & i64::MAX)
                };
                out.points.push((
                    number,
                    fs.iter()
                        .map(|f| es_codec::points::sortable_bytes(sortable(*f), 8))
                        .collect(),
                ));
                point_width = 8;
                out.sorted_numeric
                    .push((number, fs.into_iter().map(sortable).collect()));
                (IndexOptions::None, DocValuesType::SortedNumeric, true)
            }
            M::Date { .. } | M::Float => continue,
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
/// Saves are serialized process-wide.
// ponytail: rewrites every index on every save, O(total docs) per write; incremental segments
// when write throughput matters
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
    write_snapshot(&new, indices)?;
    if data_dir.exists() {
        std::fs::rename(data_dir, &old).map_err(io)?;
    }
    std::fs::rename(&new, data_dir).map_err(io)?;
    remove_dir_if_exists(&old)
}

/// Writes cluster metadata plus, per index, one segment holding every live document's `_id`,
/// `_source`, `_version` and `_seq_no` into the empty directory `data_dir`.
fn write_snapshot(data_dir: &Path, indices: &Indices) -> es_codec::Result<()> {
    use es_codec::{
        cluster_state::{GlobalMetadata, IndexMetadata},
        data_dir::write_empty_data_dir,
        segment_infos::shard_user_data,
        segment_writer::{NewSegment, es_meta_fields, write_segment},
        terms::TermPostings,
        uid::encode_id,
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
                .or_insert_with(|| crate::VERSION_CREATED.into());
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
        let mut docs: Vec<_> = ix.docs().collect();
        if docs.is_empty() {
            continue;
        }
        docs.sort_by_key(|(_, d)| d.seq_no);
        let dir = data_dir.join("indices").join(&meta.uuid).join("0/index");
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
        let ids: Vec<_> = ids
            .into_iter()
            .map(|(term, doc)| TermPostings {
                term,
                total_term_freq: 1,
                docs: vec![(doc, 1)],
                positions: vec![],
            })
            .collect();
        let user = user_fields(&ix.mappings, &docs, &fields);
        let mut all_fields = fields.clone();
        all_fields.extend(user.fields);
        let mut terms = vec![(0, ids)];
        terms.extend(user.terms);
        let seg = write_segment(
            &dir,
            &NewSegment {
                name: "_0",
                id: [7; 16],
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
        let max_seq = docs.last().map_or(-1, |(_, d)| d.seq_no as i64);
        SegmentInfos {
            id: [1; 16],
            generation: 2,
            lucene_version: (10, 0, 0),
            index_created_major: 10,
            version: 2,
            counter: 1,
            segments: vec![seg],
            user_data: shard_user_data(
                "esminiHistoryUuid0000A",
                "esminiTranslogUuid000A",
                max_seq,
                max_seq,
            ),
        }
        .write(&dir)?;
        let cp = es_codec::translog::Checkpoint {
            generation: 1,
            global_checkpoint: max_seq,
            primary_term: 1,
        };
        let (_, ckp) = es_codec::translog::write_empty("esminiTranslogUuid000A", &cp);
        std::fs::write(dir.join("../translog/translog.ckp"), ckp)
            .map_err(|e| es_codec::Error(e.to_string()))?;
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
