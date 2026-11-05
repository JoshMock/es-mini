//! Disk reads that serve several lookups at once answer as one lookup at a time did.

mod common;

use common::Scratch;
use es_index::postings::Indexed;
use es_index::{FieldMapping, Index, Indices, disk};
use serde_json::json;
use std::collections::BTreeMap;

/// Per doc id: (freq, field length as its norm stands for it, positions if read).
type Freqs = BTreeMap<String, (u32, u32, Option<Vec<u32>>)>;

#[test]
fn disk_speed_terms_of_one_field_share_each_segments_norms() {
    let dir = Scratch::new("disk-speed-norms", Some("data"));
    let mut ix = Index::default();
    let text = FieldMapping::Text {
        store: false,
        fields: Default::default(),
    };
    ix.mappings.properties.insert("t".into(), text);
    ix.put("1", json!({"t": "fox dog"}));
    ix.put("2", json!({"t": "fox fox cat"}));
    let mut indices = Indices::from([("i".to_owned(), ix)]);
    disk::save(&dir, &indices).unwrap();
    let ix = indices.get_mut("i").unwrap();
    ix.put("3", json!({"t": "dog bird bird bird"}));
    ix.put("4", json!({"t": "fox"}));
    disk::save(&dir, &indices).unwrap();
    let mut ix = disk::open(&dir).unwrap().remove("i").unwrap();
    ix.put("5", json!({"t": "dog fox"}));

    let shard = &ix.shards()[0];
    let ids: BTreeMap<_, _> = shard.disk_live().map(|(p, (id, _))| (p, id)).collect();
    assert_eq!(ids.keys().map(|p| p.0).max(), Some(1), "two segments");
    let terms = ["fox", "dog", "cat", "bird", "none"].map(String::from);
    let got: Vec<Freqs> = (shard.term_postings("t", "t", Indexed::Analyzed, &terms, true))
        .into_iter()
        .map(|f| {
            let f = f.unwrap();
            let pos = f.positions.unwrap();
            let memory = (f.memory.iter()).map(|(id, p)| (id.clone(), (p.at.0, p.at.1, None)));
            let disk = (f.disk.iter())
                .map(|(p, at)| (ids[p].to_owned(), (at.0, at.1, Some(pos[p].clone()))));
            memory.chain(disk).collect()
        })
        .collect();
    let want = |docs: &[(&str, u32, u32, Option<&[u32]>)]| -> Freqs {
        (docs.iter())
            .map(|&(id, f, n, p)| (id.into(), (f, n, p.map(<[u32]>::to_vec))))
            .collect()
    };
    assert_eq!(
        got,
        [
            want(&[
                ("1", 1, 2, Some(&[0])),
                ("2", 2, 3, Some(&[0, 1])),
                ("4", 1, 1, Some(&[0])),
                ("5", 1, 2, None),
            ]),
            want(&[
                ("1", 1, 2, Some(&[1])),
                ("3", 1, 4, Some(&[0])),
                ("5", 1, 2, None),
            ]),
            want(&[("2", 1, 3, Some(&[2]))]),
            want(&[("3", 3, 4, Some(&[1, 2, 3]))]),
            want(&[]),
        ]
    );
    // one term at a time answers the same
    for (t, want) in terms.iter().zip(&got) {
        let f = shard.term_freqs("t", "t", Indexed::Analyzed, t).unwrap();
        let disk = f.disk.iter().map(|(p, at)| (ids[p], *at));
        let memory = f.memory.iter().map(|(id, p)| (id.as_str(), p.at));
        let one: BTreeMap<_, _> = disk.chain(memory).collect();
        let many: BTreeMap<_, _> = (want.iter())
            .map(|(id, a)| (id.as_str(), (a.0, a.1)))
            .collect();
        assert_eq!(one, many, "{t}");
    }
}
