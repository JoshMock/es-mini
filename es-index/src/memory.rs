//! The documents a shard holds in memory.

use crate::Document;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::Arc,
};

/// Live documents by id and by sequence number, in segments like Lucene's so that a clone (a
/// snapshot) shares them: a write after one starts a new segment, and removing a document from
/// a shared segment copies its set of removed documents, as Lucene copies its live docs.
/// Segments are in write order; the merges that start one leave each at least twice the size
/// of the next, so there are about log2 of the document count.
#[derive(Debug, Clone, Default)]
pub(crate) struct Docs(Vec<Segment>);

#[derive(Debug, Clone, Default)]
struct Segment {
    docs: Arc<Entries>,
    removed: Arc<HashSet<u64>>,
}

#[derive(Debug, Default)]
struct Entries {
    by_seq_no: BTreeMap<u64, (String, Arc<Document>)>,
    /// The sequence number of each id's latest document.
    ids: HashMap<String, u64>,
}

impl Segment {
    fn of<'a>(docs: impl Iterator<Item = (&'a String, &'a Arc<Document>)>) -> Segment {
        let mut e = Entries::default();
        for (id, d) in docs {
            e.ids.insert(id.clone(), d.seq_no);
            e.by_seq_no.insert(d.seq_no, (id.clone(), d.clone()));
        }
        Segment {
            docs: Arc::new(e),
            removed: Arc::default(),
        }
    }

    fn live(&self) -> usize {
        self.docs.by_seq_no.len() - self.removed.len()
    }

    fn iter(&self) -> impl DoubleEndedIterator<Item = (&String, &Arc<Document>)> {
        (self.docs.by_seq_no.iter())
            .filter(|(s, _)| !self.removed.contains(s))
            .map(|(_, (id, d))| (id, d))
    }
}

impl Docs {
    /// The segment holding the latest document with `id`, and its sequence number, if live.
    fn find(&self, id: &str) -> Option<(usize, u64)> {
        // Newest first: only an id's latest document can be live.
        let (s, seq_no) = (self.0.iter().enumerate().rev())
            .find_map(|(s, seg)| Some((s, *seg.docs.ids.get(id)?)))?;
        (!self.0[s].removed.contains(&seq_no)).then_some((s, seq_no))
    }

    pub(crate) fn get_key_value(&self, id: &str) -> Option<(&String, &Arc<Document>)> {
        let (s, seq_no) = self.find(id)?;
        let (id, d) = &self.0[s].docs.by_seq_no[&seq_no];
        Some((id, d))
    }

    pub(crate) fn get(&self, id: &str) -> Option<&Arc<Document>> {
        self.get_key_value(id).map(|(_, d)| d)
    }

    pub(crate) fn by_seq_no(&self, seq_no: u64) -> Option<(&String, &Arc<Document>)> {
        let seg = (self.0.iter()).find(|s| s.docs.by_seq_no.contains_key(&seq_no))?;
        let (id, d) = (!seg.removed.contains(&seq_no)).then(|| &seg.docs.by_seq_no[&seq_no])?;
        Some((id, d))
    }

    /// Removes the live document with `id`: from its segment if no clone shares it, else from
    /// this one's live documents of it. A segment left with more removed documents than live
    /// ones is rewritten with its live ones, or dropped without any.
    pub(crate) fn remove(&mut self, id: &str) -> Option<Arc<Document>> {
        let (s, seq_no) = self.find(id)?;
        let seg = &mut self.0[s];
        if let Some(e) = Arc::get_mut(&mut seg.docs) {
            e.ids.remove(id);
            let doc = e.by_seq_no.remove(&seq_no).map(|(_, d)| d);
            if seg.live() == 0 {
                self.0.remove(s);
            }
            return doc;
        }
        Arc::make_mut(&mut seg.removed).insert(seq_no);
        let doc = seg.docs.by_seq_no[&seq_no].1.clone();
        if seg.removed.len() > seg.live() {
            match seg.live() {
                0 => drop(self.0.remove(s)),
                _ => self.0[s] = Segment::of(self.0[s].iter()),
            }
        }
        Some(doc)
    }

    /// Adds a document whose id has no live one.
    pub(crate) fn insert(&mut self, id: String, doc: Arc<Document>) {
        let shared = |s: &mut Segment| {
            Arc::get_mut(&mut s.docs).is_none() || Arc::get_mut(&mut s.removed).is_none()
        };
        if self.0.last_mut().is_none_or(shared) {
            while let [.., a, b] = &self.0[..]
                && a.live() < 2 * b.live()
            {
                let merged = Segment::of(a.iter().chain(b.iter()));
                self.0.truncate(self.0.len() - 2);
                self.0.push(merged);
            }
            self.0.push(Segment::default());
        }
        if let Some(e) = self.0.last_mut().and_then(|s| Arc::get_mut(&mut s.docs)) {
            e.ids.insert(id.clone(), doc.seq_no);
            e.by_seq_no.insert(doc.seq_no, (id, doc));
        }
    }

    /// The live documents, oldest segment first, each by sequence number.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (&String, &Arc<Document>)> {
        self.0.iter().flat_map(Segment::iter)
    }

    /// The live document written last.
    pub(crate) fn last(&self) -> Option<&Arc<Document>> {
        (self.0.iter().rev()).find_map(|s| s.iter().next_back().map(|(_, d)| d))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_bounds_2_segments_follow_writes_and_stay_few() {
        let (mut docs, mut want, mut snaps) = (Docs::default(), BTreeMap::new(), vec![]);
        for seq_no in 0..3000u64 {
            let id = (seq_no * 7 % 500).to_string();
            if let Some(old) = docs.remove(&id) {
                assert_eq!(want.remove(&old.seq_no), Some(id.clone()));
            }
            if seq_no % 5 != 0 {
                let doc = Document {
                    source: serde_json::Value::Null,
                    version: 1,
                    seq_no,
                    routing: None,
                };
                docs.insert(id.clone(), Arc::new(doc));
                want.insert(seq_no, id);
            }
            if seq_no % 3 == 0 {
                let model = want.clone();
                snaps.push((docs.clone(), model));
            }
            assert!(docs.0.len() <= 12, "{} segments", docs.0.len());
        }
        snaps.push((docs, want));
        for (docs, want) in &snaps {
            let got: Vec<_> = (docs.iter())
                .map(|(id, d)| (d.seq_no, id.clone()))
                .collect();
            assert_eq!(got, want.clone().into_iter().collect::<Vec<_>>());
            for (&seq_no, id) in want {
                assert_eq!(docs.get(id).unwrap().seq_no, seq_no);
                assert_eq!(docs.by_seq_no(seq_no).unwrap().0, id);
            }
            assert_eq!(
                docs.last().map(|d| d.seq_no),
                want.keys().next_back().copied()
            );
            let live: HashSet<&String> = want.values().collect();
            let gone = (0..500)
                .map(|i| i.to_string())
                .filter(|id| !live.contains(id));
            gone.for_each(|id| assert!(docs.get(&id).is_none(), "{id} is gone"));
        }
    }
}
