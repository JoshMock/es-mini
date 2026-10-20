//! Loads indices from an Elasticsearch data directory (read-only).
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
