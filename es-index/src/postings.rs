//! In-memory postings over an index's in-memory documents, so term, match and bool queries
//! read only the documents that can match, and BM25 statistics come without a scan.
//!
//! Postings are keyed by source path and whether values are analyzed (standard analyzer) or
//! taken as exact strings, and built the first time a query needs them; writes then keep the
//! built ones current. Terms come from [`field_terms`], which scoring uses too, so a document
//! the postings leave out can never match.

use crate::analysis::Analyzer;
use serde_json::Value;
use std::collections::{HashMap, HashSet};

/// A JSON scalar as the string ES would index (`5` → "5", `true` → "true").
pub fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        v => v.to_string(),
    }
}

/// Scalar values at a dotted path, with arrays flattened.
pub fn values<'a>(source: &'a Value, path: &str) -> Vec<&'a Value> {
    let mut out = Vec::new();
    collect(source, path, &mut out);
    out
}

fn collect<'a>(v: &'a Value, path: &str, out: &mut Vec<&'a Value>) {
    match v {
        Value::Array(a) => a.iter().for_each(|x| collect(x, path, out)),
        _ if path.is_empty() => {
            if !v.is_null() {
                out.push(v);
            }
        }
        Value::Object(o) => {
            if let Some(x) = o.get(path) {
                collect(x, "", out);
            }
            for (i, _) in path.match_indices('.') {
                if let Some(x) = o.get(&path[..i]) {
                    collect(x, &path[i + 1..], out);
                }
            }
        }
        _ => {}
    }
}

/// Indexed terms of `vals`: analyzed tokens, or each value as one exact term.
pub fn terms(analyzed: bool, vals: &[&Value]) -> Vec<String> {
    match analyzed {
        true => vals
            .iter()
            .flat_map(|v| Analyzer::Standard.analyze(&text(v)))
            .collect(),
        false => vals.iter().map(|v| text(v)).collect(),
    }
}

/// The terms a document indexes at `path`.
pub fn field_terms(source: &Value, path: &str, analyzed: bool) -> Vec<String> {
    terms(analyzed, &values(source, path))
}

/// BM25 statistics of one field, counting superseded documents like Lucene's unmerged segments.
#[derive(Debug, Clone, PartialEq)]
pub struct FieldStats {
    /// Documents with at least one term.
    pub docs: f64,
    pub total_terms: f64,
    /// Per requested term: documents containing it.
    pub doc_freq: HashMap<String, f64>,
}

#[derive(Debug, Default)]
struct Field {
    /// Term → ids of live documents containing it.
    docs: HashMap<String, HashSet<String>>,
    /// Live document id → its distinct terms and term count.
    live: HashMap<String, (HashSet<String>, usize)>,
    /// Statistics over live and superseded documents.
    doc_count: f64,
    total_terms: f64,
    doc_freq: HashMap<String, f64>,
}

impl Field {
    /// Links a document, and adds it to the statistics if `counted`.
    fn add(&mut self, id: &str, terms: Vec<String>, counted: bool) {
        if terms.is_empty() {
            return;
        }
        let count = terms.len();
        let distinct: HashSet<String> = terms.into_iter().collect();
        let counted = if counted { 1.0 } else { 0.0 };
        for t in &distinct {
            self.docs
                .entry(t.clone())
                .or_default()
                .insert(id.to_owned());
            *self.doc_freq.entry(t.clone()).or_default() += counted;
        }
        self.doc_count += counted;
        self.total_terms += counted * count as f64;
        self.live.insert(id.to_owned(), (distinct, count));
    }

    /// Unlinks a replaced or deleted document; its statistics stay until [`Field::merge`].
    fn remove(&mut self, id: &str) {
        for t in self.live.remove(id).map(|l| l.0).into_iter().flatten() {
            if let Some(ids) = self.docs.get_mut(&t) {
                ids.remove(id);
                if ids.is_empty() {
                    self.docs.remove(&t);
                }
            }
        }
    }

    /// Statistics of live documents only, after superseded ones are dropped.
    fn merge(&mut self) {
        self.doc_count = self.live.len() as f64;
        self.total_terms = self.live.values().map(|l| l.1 as f64).sum();
        self.doc_freq = self
            .docs
            .iter()
            .map(|(t, ids)| (t.clone(), ids.len() as f64))
            .collect();
    }
}

/// Built postings per (source path, analyzed).
#[derive(Debug, Default)]
pub struct Postings(HashMap<(String, bool), Field>);

impl Postings {
    /// The postings of `path`, built from `live` documents and `superseded` sources if new.
    /// Live documents with ids in `uncounted` are left out of the statistics.
    // ponytail: `uncounted` (a snapshot's documents from disk) is not kept out by a later
    // Field::merge, since snapshots take no writes
    fn field<'d>(
        &mut self,
        path: &str,
        analyzed: bool,
        live: impl Iterator<Item = (&'d str, &'d Value)>,
        superseded: &[Value],
        uncounted: &HashSet<String>,
    ) -> &Field {
        self.0
            .entry((path.to_owned(), analyzed))
            .or_insert_with(|| {
                let mut f = Field::default();
                for (id, source) in live {
                    let counted = !uncounted.contains(id);
                    f.add(id, field_terms(source, path, analyzed), counted);
                }
                for (n, source) in superseded.iter().enumerate() {
                    // Counted in the statistics, then unlinked like any superseded document.
                    let id = format!("\0superseded{n}");
                    f.add(&id, field_terms(source, path, analyzed), true);
                    f.remove(&id);
                }
                f
            })
    }

    pub(crate) fn add(&mut self, id: &str, source: &Value) {
        for ((path, analyzed), f) in &mut self.0 {
            f.add(id, field_terms(source, path, *analyzed), true);
        }
    }

    pub(crate) fn remove(&mut self, id: &str) {
        self.0.values_mut().for_each(|f| f.remove(id));
    }

    pub(crate) fn merge(&mut self) {
        self.0.values_mut().for_each(Field::merge);
    }
}

impl crate::Index {
    fn postings_for<R>(&self, path: &str, analyzed: bool, read: impl FnOnce(&Field) -> R) -> R {
        let mut postings = self
            .postings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let live = self.docs.iter().map(|(id, d)| (id.as_str(), &d.source));
        read(postings.field(path, analyzed, live, &self.superseded, &self.from_disk))
    }

    /// The in-memory documents indexing all (or any) of `terms` at `path`. Documents on disk
    /// are narrowed separately ([`Index::disk_term`](crate::Index::disk_term)).
    pub fn memory_docs(
        &self,
        path: &str,
        analyzed: bool,
        terms: &[String],
        all: bool,
    ) -> Option<HashSet<String>> {
        self.postings_for(path, analyzed, |f| {
            let set = |t: &String| f.docs.get(t).cloned().unwrap_or_default();
            let mut sets = terms.iter().map(set);
            let first = sets.next().unwrap_or_default();
            Some(sets.fold(first, |a, b| if all { &a & &b } else { &a | &b }))
        })
    }

    /// BM25 statistics of `field` (at source `path`) for `wanted` terms: those of the segments
    /// of an index opened from disk plus those of the documents in memory. `None` when the
    /// segments have no statistics for the field.
    pub fn field_stats(
        &self,
        field: &str,
        path: &str,
        analyzed: bool,
        wanted: &[String],
    ) -> Option<FieldStats> {
        let disk = match &self.stats {
            Some(s) => Some((s.fields.get(field)?, s)),
            None => None,
        };
        let (docs, total_terms) = disk.map_or((0.0, 0.0), |d| *d.0);
        let disk_freq = |w: &String| {
            let df = disk.and_then(|d| d.1.doc_freq.get(&(field.to_owned(), w.clone())));
            df.copied().unwrap_or(0.0)
        };
        self.postings_for(path, analyzed, |f| {
            Some(FieldStats {
                docs: docs + f.doc_count,
                total_terms: total_terms + f.total_terms,
                doc_freq: wanted
                    .iter()
                    .map(|w| {
                        let df = f.doc_freq.get(w).copied().unwrap_or(0.0);
                        (w.clone(), disk_freq(w) + df)
                    })
                    .collect(),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::Index;
    use serde_json::json;
    use std::collections::HashSet;

    fn ids(v: Option<HashSet<String>>) -> Vec<String> {
        let mut v: Vec<_> = v.unwrap().into_iter().collect();
        v.sort();
        v
    }

    fn index() -> Index {
        let mut i = Index::default();
        i.put("1", json!({"t": "Quick fox", "k": "a", "o": {"x": "Deep"}}));
        i.put("2", json!({"t": ["lazy dog", "quick"], "k": "b"}));
        i.put("3", json!({"n": 5}));
        i
    }

    #[test]
    fn analyzed_and_exact_terms_by_source_path() {
        let i = index();
        assert_eq!(
            ids(i.memory_docs("t", true, &["quick".into()], false)),
            ["1", "2"]
        );
        assert_eq!(
            ids(i.memory_docs("t", true, &["quick".into(), "fox".into()], true)),
            ["1"]
        );
        assert_eq!(ids(i.memory_docs("k", false, &["b".into()], false)), ["2"]);
        assert_eq!(
            ids(i.memory_docs("o.x", true, &["deep".into()], false)),
            ["1"]
        );
        // Exact terms are whole values: "Quick fox" is not "quick", but the array value is.
        assert_eq!(
            ids(i.memory_docs("t", false, &["quick".into()], false)),
            ["2"]
        );
    }

    #[test]
    fn writes_keep_built_postings_current() {
        let mut i = index();
        assert_eq!(ids(i.memory_docs("t", true, &["fox".into()], false)), ["1"]);
        i.put("1", json!({"t": "slow cat"}));
        i.put("4", json!({"t": "red fox"}));
        i.delete("2");
        assert_eq!(ids(i.memory_docs("t", true, &["fox".into()], false)), ["4"]);
        assert_eq!(ids(i.memory_docs("t", true, &["cat".into()], false)), ["1"]);
        assert!(ids(i.memory_docs("t", true, &["lazy".into()], false)).is_empty());
    }

    /// Like Lucene's unmerged segments: replaced and deleted docs still count until merged.
    #[test]
    fn stats_count_superseded_docs_until_merged() {
        let mut i = Index::default();
        for n in 0..6 {
            i.put(&n.to_string(), json!({"t": "fox fox"}));
        }
        i.put("0", json!({"t": "cat"}));
        let s = i
            .field_stats("t", "t", true, &["fox".into(), "cat".into()])
            .unwrap();
        assert_eq!((s.docs, s.total_terms), (7.0, 13.0));
        assert_eq!((s.doc_freq["fox"], s.doc_freq["cat"]), (6.0, 1.0));
        i.delete("1");
        i.delete("2"); // 3 superseded of 7 physical docs: past a third, so merged away
        let s = i.field_stats("t", "t", true, &["fox".into()]).unwrap();
        assert_eq!((s.docs, s.total_terms, s.doc_freq["fox"]), (4.0, 7.0, 3.0));
    }
}
