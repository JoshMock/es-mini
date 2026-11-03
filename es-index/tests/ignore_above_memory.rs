//! Keyword values longer than `ignore_above` are left out of the in-memory postings as they are
//! left out of the segments, so an index answers the same before and after a save and reopen.

mod common;

use common::Scratch;
use es_index::postings::Indexed;
use es_index::{FieldMapping, Index, Indices, disk};
use serde_json::json;
use std::collections::BTreeMap;

fn index() -> Index {
    let keyword = || FieldMapping::Keyword {
        ignore_above: Some(3),
        split_queries_on_whitespace: false,
    };
    let mut idx = Index::default();
    let props = &mut idx.mappings.properties;
    props.insert("k".into(), keyword());
    let fields = [("keyword".to_owned(), keyword())].into();
    props.insert("s".into(), FieldMapping::TextKeyword { fields });
    idx.put("1", json!({"k": ["abc", "abcd"], "s": "long text"}));
    idx.put("2", json!({"k": "ab", "s": "ab"}));
    idx.put("3", json!({"k": "abcde", "s": "abcde"}));
    idx.put("4", json!({"k": "abc"}));
    idx
}

/// Per term: the documents holding it (term and match queries), with their frequency and
/// norm (scores), and the field's BM25 statistics.
type Answers = (
    BTreeMap<String, BTreeMap<String, (u32, u32)>>,
    (f64, f64, Vec<f64>),
);

fn answers(idx: &Index, field: &str, path: &str, terms: &[&str]) -> Answers {
    let shard = &idx.shards()[0];
    let ids: BTreeMap<_, _> = shard.disk_live().map(|(p, (id, _))| (p, id)).collect();
    let indexed = Indexed::Exact(Some(3));
    let postings = (terms.iter())
        .map(|t| {
            let f = shard.term_freqs(field, path, indexed, t).unwrap();
            let memory = (f.memory.iter()).map(|(id, p)| (id.clone(), p.at));
            let disk = (f.disk.iter()).map(|(p, at)| (ids[p].to_owned(), *at));
            (t.to_string(), memory.chain(disk).collect())
        })
        .collect();
    let wanted: Vec<String> = terms.iter().map(|t| t.to_string()).collect();
    let s = shard.field_stats(field, path, indexed, &wanted).unwrap();
    let doc_freq = wanted.iter().map(|t| s.doc_freq[t]).collect();
    (postings, (s.docs, s.total_terms, doc_freq))
}

#[test]
fn ignore_above_values_are_left_out_of_postings_in_memory_as_on_disk() {
    let dir = Scratch::new("ignore-above-memory", Some("data"));
    disk::save(&dir, &Indices::from([("i".into(), index())])).unwrap();
    let opened = disk::open(&dir).unwrap().remove("i").unwrap();
    let memory = index();
    let terms = ["ab", "abc", "abcd", "abcde", "long text"];
    for (field, path) in [("k", "k"), ("s.keyword", "s")] {
        let want = answers(&opened, field, path, &terms);
        assert_eq!(answers(&memory, field, path, &terms), want, "{field}");
    }
    // Only fitting values are indexed, and only documents with one count in the statistics.
    let (postings, stats) = answers(&memory, "k", "k", &terms);
    let docs = |t: &str| postings[t].keys().cloned().collect::<Vec<_>>();
    assert_eq!(docs("abc"), ["1", "4"]);
    assert_eq!(docs("ab"), ["2"]);
    assert!(docs("abcd").is_empty() && docs("abcde").is_empty());
    assert_eq!(stats, (3.0, 3.0, vec![1.0, 2.0, 0.0, 0.0, 0.0]));
}
