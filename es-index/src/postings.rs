//! In-memory postings over an index's in-memory documents, so term, match and bool queries
//! read only the documents that can match, and BM25 statistics come without a scan.
//!
//! Postings are keyed by source path and how values are [`Indexed`] (analyzed by the standard
//! analyzer, or taken as exact strings), and built the first time a query needs them; writes then keep the
//! built ones current. Terms come from [`field_terms`], which scoring uses too, so a document
//! the postings leave out can never match.
//!
//! Numeric and date fields get a sorted index of their values (dates as epoch millis) for
//! range and term queries, built and kept current the same way.

use crate::{FieldMapping, analysis::Analyzer};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

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

/// How a field's source values become its terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Indexed {
    /// Standard-analyzed tokens.
    Analyzed,
    /// Each value as one term, but none longer than `ignore_above`.
    Exact(Option<u32>),
}

impl Indexed {
    /// How the document `seq_no` was indexed at `path` (see [`limit_at`]).
    pub fn at(self, changes: &[LimitChange], path: &str, seq_no: u64) -> Indexed {
        match self {
            Indexed::Exact(limit) => Indexed::Exact(limit_at(changes, path, limit, seq_no)),
            Indexed::Analyzed => self,
        }
    }
}

/// A keyword field at source `path` changing its `ignore_above` from `old` to `new` when the
/// shard's next sequence number was `seq_no`. As in Elasticsearch, documents indexed before
/// keep `old`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LimitChange {
    pub seq_no: u64,
    pub path: String,
    pub old: Option<u32>,
    pub new: Option<u32>,
}

/// The `ignore_above` the document `seq_no` was indexed with at `path`, by a keyword field whose
/// limit is now `limit`: `changes` (oldest first) followed back to before the document.
// ponytail: changes are told apart by source path and limit, so two keyword multi-fields of one
// path that changed to the same limit share their history; key them by field name if that matters
pub fn limit_at(
    changes: &[LimitChange],
    path: &str,
    mut limit: Option<u32>,
    seq_no: u64,
) -> Option<u32> {
    for c in changes.iter().rev().take_while(|c| c.seq_no > seq_no) {
        if c.path == path && c.new == limit {
            limit = c.old;
        }
    }
    limit
}

/// Whether a keyword value is longer than `ignore_above` UTF-16 units, so that, as in
/// Elasticsearch, it is not indexed and its field is listed in `_ignored`.
pub fn ignored(ignore_above: Option<u32>, s: &str) -> bool {
    ignore_above.is_some_and(|n| s.encode_utf16().count() > n as usize)
}

/// Indexed terms of `vals`: analyzed tokens, or each value as one exact term.
pub fn terms(indexed: Indexed, vals: &[&Value]) -> Vec<String> {
    match indexed {
        Indexed::Analyzed => vals
            .iter()
            .flat_map(|v| Analyzer::Standard.analyze(&text(v)))
            .collect(),
        Indexed::Exact(limit) => (vals.iter().map(|v| text(v)))
            .filter(|t| !ignored(limit, t))
            .collect(),
    }
}

/// The analyzed tokens of one document's values of a text field, with their positions. Like
/// Lucene's IndexingChain, positions run on across values, with ES's default
/// position_increment_gap of 100 between them.
pub fn positions<S: AsRef<str>>(vals: &[S]) -> Vec<(String, u32)> {
    let mut out = vec![];
    let mut position = -1i64;
    for v in vals {
        for tok in Analyzer::Standard.analyze(v.as_ref()) {
            position += 1;
            out.push((tok, position as u32));
        }
        position += 100;
    }
    out
}

/// The terms a document indexes at `path`.
pub fn field_terms(source: &Value, path: &str, indexed: Indexed) -> Vec<String> {
    terms(indexed, &values(source, path))
}

/// BM25 statistics of one field, counting superseded documents like Elasticsearch's soft deletes.
#[derive(Debug, Clone, PartialEq)]
pub struct FieldStats {
    /// Documents with at least one term.
    pub docs: f64,
    pub total_terms: f64,
    /// Per requested term: documents containing it.
    pub doc_freq: HashMap<String, f64>,
}

/// A term's frequency in each live document holding it, with the document's field length as
/// its norm stands for it (see [`Shard::term_freqs`](crate::Shard::term_freqs)).
#[derive(Debug, Default)]
pub struct TermFreqs {
    /// On-disk documents by position.
    pub disk: HashMap<crate::disk::DiskPos, (u32, u32)>,
    /// The term's positions in each on-disk document holding it, ascending, if they were asked
    /// for and every segment has them to read.
    pub positions: Option<crate::disk::Positions>,
    /// In-memory documents by id: the term's in-memory postings, shared, not copied.
    pub memory: Arc<TermDocs>,
}

/// The live in-memory documents holding a term, by id.
pub type TermDocs = HashMap<String, Posting>;

/// A live in-memory document in a term's postings.
#[derive(Debug, Clone, Copy)]
pub struct Posting {
    /// The term's frequency and the field length as its norm stands for it; (1, 1) for a term
    /// that is not analyzed (see [`Shard::term_freqs`](crate::Shard::term_freqs)).
    pub at: (u32, u32),
    pub seq_no: u64,
}

#[derive(Debug, Default)]
struct Field {
    /// Term → live documents containing it, shared with searches.
    docs: HashMap<String, Arc<TermDocs>>,
    /// Live document id → its distinct terms.
    live: HashMap<String, HashSet<String>>,
    /// Statistics over live and superseded documents.
    doc_count: f64,
    total_terms: f64,
    doc_freq: HashMap<String, f64>,
}

impl Field {
    /// Links a document, and adds it to the statistics if `counted`.
    fn add(&mut self, id: &str, seq_no: u64, indexed: Indexed, terms: Vec<String>, counted: bool) {
        if terms.is_empty() {
            return;
        }
        let count = terms.len();
        let mut freqs: HashMap<String, u32> = HashMap::new();
        terms
            .into_iter()
            .for_each(|t| *freqs.entry(t).or_default() += 1);
        let counted = if counted { 1.0 } else { 0.0 };
        let len = crate::disk::norm_len(crate::disk::norm_byte(count as u32));
        for (t, freq) in &freqs {
            let at = match indexed {
                Indexed::Analyzed => (*freq, len),
                Indexed::Exact(_) => (1, 1),
            };
            let docs = Arc::make_mut(self.docs.entry(t.clone()).or_default());
            docs.insert(id.to_owned(), Posting { at, seq_no });
            *self.doc_freq.entry(t.clone()).or_default() += counted;
        }
        self.doc_count += counted;
        self.total_terms += counted * count as f64;
        let distinct = freqs.into_keys().collect();
        self.live.insert(id.to_owned(), distinct);
    }

    /// Unlinks a replaced or deleted document; its statistics stay.
    fn remove(&mut self, id: &str) {
        for t in self.live.remove(id).into_iter().flatten() {
            if let Some(ids) = self.docs.get_mut(&t) {
                Arc::make_mut(ids).remove(id);
                if ids.is_empty() {
                    self.docs.remove(&t);
                }
            }
        }
    }
}

/// How a numeric or date field reads a value as a number.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Key {
    Number,
    /// A number truncated toward zero, as integer fields index it.
    Integer,
    /// Epoch millis, with the mapping's `format` and `locale`.
    Date(Option<String>, Option<String>),
}

impl Key {
    fn of(m: &FieldMapping) -> Option<Key> {
        match m {
            FieldMapping::Date { format, locale, .. }
            | FieldMapping::DateNanos { format, locale } => {
                Some(Key::Date(format.clone(), locale.clone()))
            }
            m if m.is_integer() => Some(Key::Integer),
            m if m.is_numeric() => Some(Key::Number),
            _ => None,
        }
    }

    /// `v` as a number, ordered like `f64::total_cmp` (Lucene's `doubleToSortableLong`).
    fn read(&self, v: &Value) -> Option<i64> {
        let x = match self {
            Key::Number => crate::json_num(v)?,
            Key::Integer => crate::json_int(v)? as f64,
            Key::Date(format, locale) => {
                crate::date_millis(format.as_deref(), locale.as_deref(), v)? as f64
            }
        };
        let bits = x.to_bits() as i64;
        Some(bits ^ ((bits >> 63) & i64::MAX))
    }
}

/// The values of one numeric or date field, sorted.
// ponytail: longs above 2^53 key as their nearest f64, so ranges narrow to a superset there
#[derive(Debug, Default)]
struct Numbers {
    docs: BTreeMap<i64, HashSet<String>>,
    /// Live document id → its distinct values.
    live: HashMap<String, HashSet<i64>>,
    /// Live documents whose values do not sort as their key: more than one, or one that is not
    /// a [`plain`] number.
    odd: HashSet<String>,
}

impl Numbers {
    fn add(&mut self, id: &str, (values, plain): (HashSet<i64>, bool)) {
        for v in &values {
            self.docs.entry(*v).or_default().insert(id.to_owned());
        }
        self.live.insert(id.to_owned(), values);
        if !plain {
            self.odd.insert(id.to_owned());
        }
    }

    fn remove(&mut self, id: &str) {
        self.odd.remove(id);
        for v in self.live.remove(id).into_iter().flatten() {
            if let Some(ids) = self.docs.get_mut(&v) {
                ids.remove(id);
                if ids.is_empty() {
                    self.docs.remove(&v);
                }
            }
        }
    }
}

/// The keys of a document's values at `path`, and whether it has at most one, [`plain`].
fn keys(key: &Key, source: &Value, path: &str) -> (HashSet<i64>, bool) {
    let vals = values(source, path);
    let plain = vals.len() <= 1 && vals.iter().all(|v| plain(v));
    (
        vals.into_iter().filter_map(|v| key.read(v)).collect(),
        plain,
    )
}

/// A JSON number an f64 holds exactly, which compares with others as its key does (as
/// es-query's `scalar_cmp` compares sources).
fn plain(v: &Value) -> bool {
    match v.as_i64() {
        Some(i) => i.unsigned_abs() <= 1 << 53,
        None => v.is_f64(),
    }
}

/// Built postings per (source path, how it is indexed), and numeric indices per (source path, key).
#[derive(Debug, Default)]
pub struct Postings(
    HashMap<(String, Indexed), Field>,
    HashMap<(String, Key), Numbers>,
);

impl Postings {
    /// The postings of `path`, built from `live` documents and `superseded` sources if new.
    /// Live documents with sequence numbers below `persisted` are left out of the statistics.
    fn field<'d>(
        &mut self,
        path: &str,
        indexed: Indexed,
        live: impl Iterator<Item = (&'d str, &'d crate::Document)>,
        superseded: &[std::sync::Arc<crate::Document>],
        persisted: u64,
        changes: &[LimitChange],
    ) -> &Field {
        self.0.entry((path.to_owned(), indexed)).or_insert_with(|| {
            let mut f = Field::default();
            let terms = |d: &crate::Document| {
                field_terms(&d.source, path, indexed.at(changes, path, d.seq_no))
            };
            for (id, d) in live {
                let counted = d.seq_no >= persisted;
                let terms = terms(d);
                f.add(id, d.seq_no, indexed, terms, counted);
            }
            for (n, d) in superseded.iter().enumerate() {
                // Counted in the statistics, then unlinked like any superseded document.
                let id = format!("\0superseded{n}");
                let terms = terms(d);
                f.add(&id, d.seq_no, indexed, terms, true);
                f.remove(&id);
            }
            f
        })
    }

    /// Links a new document, which every [`LimitChange`] precedes.
    pub(crate) fn add(&mut self, id: &str, seq_no: u64, source: &Value) {
        for ((path, indexed), f) in &mut self.0 {
            let terms = field_terms(source, path, *indexed);
            f.add(id, seq_no, *indexed, terms, true);
        }
        for ((path, key), n) in &mut self.1 {
            n.add(id, keys(key, source, path));
        }
    }

    pub(crate) fn remove(&mut self, id: &str) {
        self.0.values_mut().for_each(|f| f.remove(id));
        self.1.values_mut().for_each(|n| n.remove(id));
    }
}

impl crate::Shard {
    fn postings_for<R>(&self, path: &str, indexed: Indexed, read: impl FnOnce(&Field) -> R) -> R {
        let mut postings = self
            .postings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let live = self.docs.iter().map(|(id, d)| (id.as_str(), &**d));
        read(postings.field(
            path,
            indexed,
            live,
            &self.superseded,
            self.persisted,
            &self.ignore_above,
        ))
    }

    /// The in-memory documents indexing all (or any) of `terms` at `path`. Documents on disk
    /// are narrowed separately ([`Shard::disk_term`](crate::Shard::disk_term)).
    pub fn memory_docs(
        &self,
        path: &str,
        indexed: Indexed,
        terms: &[String],
        all: bool,
    ) -> Option<HashSet<String>> {
        self.postings_for(path, indexed, |f| {
            let set =
                |t: &String| (f.docs.get(t).into_iter().flat_map(|d| d.keys().cloned())).collect();
            let mut sets = terms.iter().map(set);
            let first = sets.next().unwrap_or_default();
            Some(sets.fold(first, |a, b| if all { &a & &b } else { &a | &b }))
        })
    }

    /// The documents of `within` that [`memory_docs`](Self::memory_docs) gives, without
    /// reading the others that index the terms.
    pub fn memory_docs_in(
        &self,
        path: &str,
        indexed: Indexed,
        terms: &[String],
        all: bool,
        within: &HashSet<String>,
    ) -> HashSet<String> {
        if terms.is_empty() {
            return HashSet::new();
        }
        self.postings_for(path, indexed, |f| {
            let has = |id: &String, t: &String| f.docs.get(t).is_some_and(|d| d.contains_key(id));
            (within.iter())
                .filter(|id| match all {
                    true => terms.iter().all(|t| has(id, t)),
                    false => terms.iter().any(|t| has(id, t)),
                })
                .cloned()
                .collect()
        })
    }

    /// How many in-memory documents index `term` at `path`: the size of
    /// [`memory_docs`](Self::memory_docs) for one term, without collecting them.
    pub fn memory_term_count(&self, path: &str, indexed: Indexed, term: &str) -> usize {
        // writes unlink replaced and deleted documents, so postings list live ones only
        self.postings_for(path, indexed, |f| f.docs.get(term).map_or(0, |d| d.len()))
    }

    /// How many in-memory documents below `visible` index `term` at `path`, and the first `n`
    /// of them in write order (in no order).
    pub fn memory_term_first(
        &self,
        path: &str,
        indexed: Indexed,
        term: &str,
        n: usize,
        visible: u64,
    ) -> (usize, Vec<(&str, &crate::Document)>) {
        let mut seq_nos: Vec<u64> = self.postings_for(path, indexed, |f| {
            let docs = f.docs.get(term).into_iter().flat_map(|d| d.values());
            docs.map(|p| p.seq_no).filter(|s| *s < visible).collect()
        });
        let count = seq_nos.len();
        if n < count {
            seq_nos.select_nth_unstable(n);
            seq_nos.truncate(n);
        }
        let docs = seq_nos.iter().filter_map(|&s| self.docs.by_seq_no(s));
        (count, docs.map(|(id, d)| (id.as_str(), &**d)).collect())
    }

    /// The frequency of `term` in each live document holding it in `field` (at source `path`),
    /// with the document's field length as its norm stands for it: from postings and norms on
    /// disk, and from the in-memory postings. As in Elasticsearch, a term that is not analyzed
    /// counts once per document and has no norms (length 1). `None` if a segment has no postings
    /// or norms for the field the codec can read.
    pub fn term_freqs(
        &self,
        field: &str,
        path: &str,
        indexed: Indexed,
        term: &str,
    ) -> Option<TermFreqs> {
        let terms = [term.to_owned()];
        self.term_postings(field, path, indexed, &terms, false)
            .pop()?
    }

    /// [`term_freqs`](Self::term_freqs) of each of `terms`, with their on-disk positions if
    /// `positions`.
    pub fn term_postings(
        &self,
        field: &str,
        path: &str,
        indexed: Indexed,
        terms: &[String],
        positions: bool,
    ) -> Vec<Option<TermFreqs>> {
        let disk = match &self.disk {
            Some(d) => d.term_freqs(field, terms, positions),
            None => (terms.iter())
                .map(|_| {
                    Some(TermFreqs {
                        positions: positions.then(HashMap::new),
                        ..Default::default()
                    })
                })
                .collect(),
        };
        (terms.iter().zip(disk))
            .map(|(term, freqs)| {
                let mut freqs = freqs?;
                let memory = self.postings_for(path, indexed, |f| f.docs.get(term).cloned());
                freqs.memory = memory.unwrap_or_default();
                Some(freqs)
            })
            .collect()
    }

    /// The in-memory documents with a value of the numeric or date field `mapping` maps (at source `path`)
    /// within `[gt, gte, lt, lte]` (or at them: the bounds are inclusive), compared as numbers
    /// (dates as instants); `None` for other fields, or for a bound that is not a number or
    /// date of the field.
    pub fn memory_numbers(
        &self,
        mapping: &FieldMapping,
        path: &str,
        bounds: [Option<&Value>; 4],
    ) -> Option<HashSet<String>> {
        let key = Key::of(mapping)?;
        // a date range's bounds are epoch millis once resolved (es-query's `resolve_dates`)
        let read_bound = |b: &Value| match (&key, b.as_i64()) {
            (Key::Date(..), Some(t)) => Key::Number.read(&t.into()),
            (Key::Date(..), None) => key.read(b),
            _ => Key::Number.read(b),
        };
        let read = |b: Option<&Value>| b.map_or(Some(None), |b| read_bound(b).map(Some));
        let [gt, gte, lt, lte] = bounds.map(read);
        // Inclusive bounds narrow to a superset; scoring checks each candidate.
        let lo = gt?.max(gte?).unwrap_or(i64::MIN);
        let hi = [lt?, lte?].into_iter().flatten().min().unwrap_or(i64::MAX);
        if lo > hi {
            return Some(HashSet::new());
        }
        self.numbers_for(path, key, |numbers| {
            let ids = numbers.docs.range(lo..=hi).flat_map(|(_, ids)| ids);
            Some(ids.cloned().collect())
        })
    }

    /// How many in-memory documents a range of the numeric field `mapping` maps (at source `path`) matches,
    /// and the first `n` of them by value (highest first if `desc`), equal ones in write order.
    /// `None` unless every in-memory document is visible below `visible` and every value of the
    /// field and bound is a lone JSON number an f64 holds exactly, so keys order them as sources.
    pub fn memory_numbers_first(
        &self,
        mapping: &FieldMapping,
        path: &str,
        [gt, gte, lt, lte]: [Option<&Value>; 4],
        desc: bool,
        n: usize,
        visible: u64,
    ) -> Option<(usize, Vec<(&str, &crate::Document)>)> {
        use std::ops::Bound::{Excluded, Included, Unbounded};
        let key = Key::of(mapping)?;
        let all_visible = self.docs.last().is_none_or(|d| d.seq_no < visible);
        let plain_bounds = [gt, gte, lt, lte].into_iter().flatten().all(plain);
        if matches!(key, Key::Date(..)) || !all_visible || !plain_bounds {
            return None;
        }
        let read = |b: Option<&Value>| b.and_then(|b| Key::Number.read(b));
        // the tighter bound of each side; at the same key, the strict one
        let lo = [read(gt).map(|k| (k, true)), read(gte).map(|k| (k, false))];
        let hi = [read(lt).map(|k| (k, false)), read(lte).map(|k| (k, true))];
        let (lo, hi) = (
            lo.into_iter().flatten().max(),
            hi.into_iter().flatten().min(),
        );
        if let (Some((a, strict)), Some((b, inclusive))) = (lo, hi)
            && (a > b || (a == b && (strict || !inclusive)))
        {
            return Some((0, vec![]));
        }
        let lo = lo.map_or(
            Unbounded,
            |(k, strict)| if strict { Excluded(k) } else { Included(k) },
        );
        let hi = hi.map_or(
            Unbounded,
            |(k, inc)| if inc { Included(k) } else { Excluded(k) },
        );
        self.numbers_for(path, key, |numbers| {
            if !numbers.odd.is_empty() {
                return None;
            }
            let (mut count, mut first) = (0, vec![]);
            let mut visit = |ids: &HashSet<String>| {
                count += ids.len();
                if first.len() < n {
                    let mut docs: Vec<_> = (ids.iter())
                        .filter_map(|id| self.docs.get_key_value(id))
                        .map(|(id, d)| (id.as_str(), &**d))
                        .collect();
                    docs.sort_unstable_by_key(|(_, d)| d.seq_no);
                    first.extend(docs.into_iter().take(n - first.len()));
                }
            };
            let range = numbers.docs.range((lo, hi));
            match desc {
                true => range.rev().for_each(|(_, ids)| visit(ids)),
                false => range.for_each(|(_, ids)| visit(ids)),
            }
            Some((count, first))
        })
    }

    /// Reads the numeric index of `path` by `key`, built from the in-memory documents if new.
    fn numbers_for<R>(&self, path: &str, key: Key, read: impl FnOnce(&Numbers) -> R) -> R {
        let mut postings =
            (self.postings.lock()).unwrap_or_else(std::sync::PoisonError::into_inner);
        let numbers = (postings.1)
            .entry((path.to_owned(), key))
            .or_insert_with_key(|(path, key)| {
                let mut n = Numbers::default();
                for (id, d) in self.docs.iter() {
                    n.add(id, keys(key, &d.source, path));
                }
                n
            });
        read(numbers)
    }

    /// The first `max` terms of analyzed `field` (at source `path`) starting with `prefix`, in
    /// byte order, from the term dictionaries on disk and in memory.
    // ponytail: Lucene collects up to `max` per segment in segment order; this is its order
    // for one segment
    pub fn prefixed_terms(&self, field: &str, path: &str, prefix: &str, max: usize) -> Vec<String> {
        let disk = (self.stats.iter().flat_map(|s| s.doc_freq.keys()))
            .filter(|(f, t)| f == field && t.starts_with(prefix))
            .map(|(_, t)| t.clone());
        let mut all: Vec<String> = self.postings_for(path, Indexed::Analyzed, |f| {
            let memory = f.doc_freq.keys().filter(|t| t.starts_with(prefix));
            disk.chain(memory.cloned()).collect()
        });
        all.sort_unstable();
        all.dedup();
        all.truncate(max);
        all
    }

    /// BM25 statistics of `field` (at source `path`) for `wanted` terms: those of the segments
    /// of an index opened from disk plus those of the documents in memory. `None` when the
    /// segments have no statistics for the field.
    pub fn field_stats(
        &self,
        field: &str,
        path: &str,
        indexed: Indexed,
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
        self.postings_for(path, indexed, |f| {
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
    use super::Indexed::{Analyzed, Exact};
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
            ids(i.shards[0].memory_docs("t", Analyzed, &["quick".into()], false)),
            ["1", "2"]
        );
        assert_eq!(
            ids(i.shards[0].memory_docs("t", Analyzed, &["quick".into(), "fox".into()], true)),
            ["1"]
        );
        assert_eq!(
            ids(i.shards[0].memory_docs("k", Exact(None), &["b".into()], false)),
            ["2"]
        );
        assert_eq!(
            ids(i.shards[0].memory_docs("o.x", Analyzed, &["deep".into()], false)),
            ["1"]
        );
        // Exact terms are whole values: "Quick fox" is not "quick", but the array value is.
        assert_eq!(
            ids(i.shards[0].memory_docs("t", Exact(None), &["quick".into()], false)),
            ["2"]
        );
    }

    #[test]
    fn memory_docs_in_narrows_given_documents() {
        let i = index();
        let within = HashSet::from(["2".to_owned(), "3".to_owned()]);
        let quick = ["quick".into()];
        let ids = |v| ids(Some(v));
        assert_eq!(
            ids(i.shards[0].memory_docs_in("t", Analyzed, &quick, false, &within)),
            ["2"]
        );
        let quick_dog = ["quick".into(), "dog".into()];
        assert_eq!(
            ids(i.shards[0].memory_docs_in("t", Analyzed, &quick_dog, true, &within)),
            ["2"]
        );
        let quick_fox = ["quick".into(), "fox".into()];
        assert!(
            ids(i.shards[0].memory_docs_in("t", Analyzed, &quick_fox, true, &within)).is_empty()
        );
        assert!(ids(i.shards[0].memory_docs_in("t", Analyzed, &[], false, &within)).is_empty());
        assert!(ids(i.shards[0].memory_docs_in("t", Analyzed, &[], true, &within)).is_empty());
    }

    #[test]
    fn writes_keep_built_postings_current() {
        let mut i = index();
        assert_eq!(
            ids(i.shards[0].memory_docs("t", Analyzed, &["fox".into()], false)),
            ["1"]
        );
        i.put("1", json!({"t": "slow cat"}));
        i.put("4", json!({"t": "red fox"}));
        i.delete("2");
        assert_eq!(
            ids(i.shards[0].memory_docs("t", Analyzed, &["fox".into()], false)),
            ["4"]
        );
        assert_eq!(
            ids(i.shards[0].memory_docs("t", Analyzed, &["cat".into()], false)),
            ["1"]
        );
        assert!(ids(i.shards[0].memory_docs("t", Analyzed, &["lazy".into()], false)).is_empty());
    }

    #[test]
    fn numeric_ranges_narrow_memory_docs() {
        let mappings = json!({"properties": {
            "n": {"type": "long"},
            "d": {"type": "date"},
            "k": {"type": "keyword"},
        }});
        let mut i = Index {
            mappings: serde_json::from_value(mappings).unwrap(),
            ..Index::default()
        };
        i.put("1", json!({"n": 5, "d": "2024-01-01T00:00:00Z"}));
        i.put(
            "2",
            json!({"n": [10, 30], "d": "2024-01-01T01:00:00+02:00"}),
        );
        i.put("3", json!({"n": "20", "d": 1704067200001i64}));
        i.put("4", json!({"k": "x"}));
        let range = |i: &Index, f: &str, b: [Option<serde_json::Value>; 4]| {
            ids(i.shards[0].memory_numbers(
                i.mappings.field(f).unwrap(),
                f,
                b.each_ref().map(Option::as_ref),
            ))
        };
        let (n, s) = (|x: i64| Some(json!(x)), |x: &str| Some(json!(x)));
        assert_eq!(range(&i, "n", [None, n(10), None, n(20)]), ["2", "3"]);
        // Bounds are inclusive: scoring, not the index, leaves out 2's values at them.
        assert_eq!(range(&i, "n", [n(10), None, n(30), None]), ["2", "3"]);
        assert_eq!(range(&i, "n", [None, s("20"), None, s("20")]), ["3"]);
        // Dates compare as instants, whatever their notation: 2 is 23:00 the day before.
        assert_eq!(
            range(&i, "d", [None, s("2024-01-01"), None, None]),
            ["1", "3"]
        );
        assert_eq!(
            range(&i, "d", [None, None, None, s("2023-12-31T23:30:00Z")]),
            ["2"]
        );
        // Writes keep a built index current.
        i.put("1", json!({"n": 15}));
        i.delete("2");
        assert_eq!(range(&i, "n", [None, n(10), None, n(20)]), ["1", "3"]);
        // Fields that are not numbers or dates, and bounds that are not, cannot narrow.
        assert_eq!(
            i.shards[0].memory_numbers(
                i.mappings.field("k").unwrap(),
                "k",
                [None, Some(&json!("x")), None, None]
            ),
            None
        );
        assert_eq!(
            i.shards[0].memory_numbers(
                i.mappings.field("n").unwrap(),
                "n",
                [None, Some(&json!("x")), None, None]
            ),
            None
        );
    }

    /// Only lone numbers an f64 holds order as their keys, so others leave paging by value
    /// to the sources, as do unrefreshed documents.
    #[test]
    fn ranges_page_by_value_only_where_keys_order_documents() {
        let mut i = Index::default();
        i.mappings
            .properties
            .insert("n".into(), crate::FieldMapping::Double);
        i.put("1", json!({"n": 5}));
        i.put("2", json!({"n": 3.5}));
        i.put("3", json!({"n": 5}));
        let first = |i: &Index, b: [Option<serde_json::Value>; 4], desc, visible| {
            let b = b.each_ref().map(Option::as_ref);
            let r = i.shards[0].memory_numbers_first(
                i.mappings.field("n")?,
                "n",
                b,
                desc,
                2,
                visible,
            )?;
            Some((
                r.0,
                r.1.iter().map(|(id, _)| *id).collect::<Vec<_>>().join(","),
            ))
        };
        let all = [None, None, None, None];
        assert_eq!(first(&i, all.clone(), false, 3), Some((3, "2,1".into())));
        assert_eq!(first(&i, all.clone(), true, 3), Some((3, "1,3".into())));
        assert_eq!(
            first(&i, [None, Some(json!(3.5)), Some(json!(5)), None], false, 3),
            Some((1, "2".into()))
        );
        assert_eq!(first(&i, all.clone(), false, 2), None);
        assert_eq!(
            first(&i, [Some(json!("1")), None, None, None], false, 3),
            None
        );
        assert_eq!(
            first(&i, [Some(json!(1u64 << 60)), None, None, None], false, 3),
            None
        );
        for odd in [json!([1, 2]), json!("4"), json!(1u64 << 60)] {
            i.put("4", json!({"n": odd}));
            assert_eq!(first(&i, all.clone(), false, 9), None, "{odd}");
        }
        i.put("4", json!({"n": [7]}));
        assert_eq!(first(&i, all, true, 9), Some((4, "4,1".into())));
    }

    /// Like Elasticsearch's soft deletes, replaced and deleted docs still count: merges keep
    /// them while they are above the last commit's checkpoint. The numbers are Elasticsearch
    /// 9.5.5's (`explain` of a match query, writes with `refresh=true`).
    #[test]
    fn stats_count_superseded_docs_like_soft_deletes() {
        let stats = |i: &Index| {
            let s = i.shards[0]
                .field_stats("t", "t", Analyzed, &["fox".into()])
                .unwrap();
            (s.docs, s.total_terms, s.doc_freq["fox"])
        };
        let mut i = Index::default();
        i.put("1", json!({"t": "fox"}));
        i.put("2", json!({"t": "fox dog"}));
        i.delete("1");
        assert_eq!(stats(&i), (2.0, 3.0, 2.0));
        i.put("2", json!({"t": "fox cat"}));
        assert_eq!(stats(&i), (3.0, 5.0, 3.0));

        let mut i = Index::default();
        for n in 1..=12 {
            i.put(&n.to_string(), json!({"t": "fox fox"}));
        }
        for n in 1..=10 {
            i.put(&n.to_string(), json!({"t": "fox"}));
        }
        // 10 superseded of 22 physical docs, all still counted.
        assert_eq!(stats(&i), (22.0, 34.0, 22.0));
    }
}
