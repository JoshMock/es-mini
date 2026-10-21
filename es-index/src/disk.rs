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

/// Replaces `data_dir` with a snapshot of `indices`: cluster metadata plus, per index, one
/// segment holding every live document's `_id`, `_source`, `_version` and `_seq_no`.
// ponytail: whole-directory rewrite per save and no postings for user fields (they are rebuilt
// from `_source` on open); incremental segments and full field indexing when writes get large
// or when Elasticsearch itself must search these files.
pub fn save(data_dir: &Path, indices: &Indices) -> es_codec::Result<()> {
    use es_codec::{
        cluster_state::{GlobalMetadata, IndexMetadata},
        data_dir::write_empty_data_dir,
        segment_infos::shard_user_data,
        segment_writer::{NewSegment, es_meta_fields, write_segment},
        terms::TermPostings,
        uid::encode_id,
    };
    for sub in ["_state", "indices"] {
        match std::fs::remove_dir_all(data_dir.join(sub)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(es_codec::Error(e.to_string()));
            }
            _ => {}
        }
    }
    let uuid = |name: &str| {
        let h = name.bytes().fold(0xcbf29ce484222325u64, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x100000001b3)
        });
        format!("{h:016x}{:06}", name.len() % 1_000_000)
    };
    let metas: Vec<_> = indices
        .iter()
        .map(|(name, ix)| IndexMetadata {
            name: name.clone(),
            uuid: uuid(name),
            settings: {
                // Elasticsearch refuses to load index metadata without the creation version.
                let mut s = ix.settings.flat().clone();
                s.entry("index.version.created".into())
                    .or_insert_with(|| "9111000".into());
                s.insert("index.uuid".into(), uuid(name));
                s
            },
            mappings: serde_json::to_string(&serde_json::json!({"_doc": ix.mappings})).ok(),
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
        let seg = write_segment(
            &dir,
            &NewSegment {
                name: "_0",
                id: [7; 16],
                max_doc: docs.len() as i32,
                fields: &fields,
                stored: &stored,
                numeric: &numeric,
                keyword: &[],
                norms: &[],
                terms: &[(0, ids)],
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
