//! Changing a keyword's `ignore_above` applies only to documents indexed afterwards, as in
//! Elasticsearch 9.5.5: earlier documents keep the terms, doc values and `_ignored` entries
//! they were indexed with, in memory, in the segments a flush writes, and after a reopen.
//!
//! Elasticsearch's answers for these documents (`k` mapped with `ignore_above` 5, raised to 8,
//! then lowered to 2): `term` finds `abc` in a and c, `abcd` in e, `abcdefg` in d and `ab` in g;
//! `_ignored` lists `k` for b, c, e, f and g; `docvalue_fields` gives a and c `abc`, d `abcdefg`,
//! e `abcd`, g `ab`, and b and f nothing.

mod common;

use common::Scratch;
use es_index::postings::Indexed;
use es_index::{FieldMapping, Index, Indices, disk};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;

fn limit(n: u32) -> FieldMapping {
    FieldMapping::Keyword {
        ignore_above: Some(n),
        split_queries_on_whitespace: false,
    }
}

fn set_limit(idx: &mut Index, n: u32) {
    let mut m = idx.mappings.clone();
    m.properties.insert("k".into(), limit(n));
    idx.set_mappings(m);
}

/// The writes of the scenario, with `step` run after each (a persist, or nothing).
fn scenario(idx: &mut Index, step: &mut dyn FnMut(&mut Index)) {
    idx.mappings.properties.insert("k".into(), limit(5));
    idx.put("a", json!({"k": "abc"}));
    idx.put("b", json!({"k": "abcdefg"}));
    idx.put("c", json!({"k": ["abc", "abcdefghij"]}));
    step(idx);
    set_limit(idx, 8);
    step(idx);
    idx.put("d", json!({"k": "abcdefg"}));
    idx.put("e", json!({"k": ["abcdefghij", "abcd"]}));
    step(idx);
    set_limit(idx, 2);
    step(idx);
    idx.put("f", json!({"k": "abc"}));
    idx.put("g", json!({"k": ["ab", "abcdefg"]}));
    step(idx);
}

/// The scenario's index, persisted to `dir` after each step as the server does.
fn persisted(dir: &Path) -> Index {
    let mut idx = Index::default();
    scenario(&mut idx, &mut |ix| {
        let mut all = Indices::from([("i".to_owned(), std::mem::take(ix))]);
        disk::persist(dir, &mut all).unwrap();
        *ix = all.remove("i").unwrap();
    });
    idx
}

/// Per term, the documents indexing it in `k`, in memory or on disk.
fn term_docs(idx: &Index) -> BTreeMap<&'static str, Vec<String>> {
    let shard = &idx.shards()[0];
    let ids: BTreeMap<_, _> = shard.disk_live().map(|(p, (id, _))| (p, id)).collect();
    ["ab", "abc", "abcd", "abcdefg", "abcdefghij"]
        .into_iter()
        .map(|t| {
            let f = shard
                .term_freqs("k", "k", Indexed::Exact(Some(2)), t)
                .unwrap();
            let memory = f.memory.keys().cloned();
            let disk = f.disk.keys().map(|p| ids[p].to_owned());
            let mut docs: Vec<String> = memory.chain(disk).collect();
            docs.sort();
            (t, docs)
        })
        .collect()
}

fn ignored(idx: &Index) -> Vec<String> {
    let mut docs: Vec<String> = (idx.shards()[0].ignored(&idx.mappings).into_iter())
        .inspect(|(_, fields)| assert_eq!(fields, &["k"]))
        .map(|(id, _)| id)
        .collect();
    docs.sort();
    docs
}

fn check(idx: &Index, how: &str) {
    let want: BTreeMap<_, _> = [
        ("ab", vec!["g"]),
        ("abc", vec!["a", "c"]),
        ("abcd", vec!["e"]),
        ("abcdefg", vec!["d"]),
        ("abcdefghij", vec![]),
    ]
    .into_iter()
    .map(|(t, ids)| (t, ids.into_iter().map(String::from).collect::<Vec<_>>()))
    .collect();
    assert_eq!(term_docs(idx), want, "terms {how}");
    assert_eq!(ignored(idx), ["b", "c", "e", "f", "g"], "_ignored {how}");
}

#[test]
fn ignore_above_changes_apply_to_later_documents_in_memory() {
    let mut idx = Index::default();
    scenario(&mut idx, &mut |_| {});
    check(&idx, "in memory");
}

#[test]
fn ignore_above_changes_apply_to_later_documents_after_flush_and_reopen() {
    let dir = Scratch::new("ignore-above-change", Some("data"));
    let mut idx = persisted(&dir);
    check(&idx, "after the persists");
    idx.flush_requested = true;
    let mut all = Indices::from([("i".to_owned(), idx)]);
    disk::persist(&dir, &mut all).unwrap();
    check(&all["i"], "after a flush");

    // The segments hold what each document was indexed with.
    let opened = disk::open(&dir).unwrap().remove("i").unwrap();
    check(&opened, "after reopen");
    let shard = &opened.shards()[0];
    let mut read = shard.disk_values(&opened.mappings, "k");
    let doc_values: BTreeMap<_, _> = ["a", "b", "c", "d", "e", "f", "g"]
        .map(|id| (id, read(id).unwrap_or_default()))
        .into();
    let v = |s: &[&str]| s.iter().map(|&s| Value::from(s)).collect::<Vec<_>>();
    let want = BTreeMap::from([
        ("a", v(&["abc"])),
        ("b", v(&[])),
        ("c", v(&["abc"])),
        ("d", v(&["abcdefg"])),
        ("e", v(&["abcd"])),
        ("f", v(&[])),
        ("g", v(&["ab"])),
    ]);
    assert_eq!(doc_values, want);
}

#[test]
fn reopen_replays_the_translog_with_the_limit_each_document_was_indexed_with() {
    let dir = Scratch::new("ignore-above-change-replay", Some("data"));
    // The last mapping change committed a to e; f and g are in the translog only.
    persisted(&dir);
    let opened = disk::open(&dir).unwrap().remove("i").unwrap();
    check(&opened, "after replaying the translog");
    assert_eq!(opened.shards()[0].ignore_above("k", Some(2), 0), Some(5));
    assert_eq!(opened.shards()[0].ignore_above("k", Some(2), 3), Some(8));
    assert_eq!(opened.shards()[0].ignore_above("k", Some(2), 5), Some(2));
}
