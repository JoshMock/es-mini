//! Loads indices from, and saves them to, an Elasticsearch data directory.
//!
//! An opened index serves its documents from the segments: only ids, versions and term
//! statistics are read into memory, and a `_source` is read from disk when a document is.

use crate::{Document, Index, Indices, Mappings, TermStats};
use es_codec::{
    SegmentInfos, SegmentReader,
    cluster_state::read_indices,
    doc_values::DocValues,
    field_infos::IndexOptions,
    stored_fields::{Doc, StoredValue},
    uid::decode_id,
};
use serde::Deserialize;
use serde_json::Value;
use std::{
    borrow::Cow,
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    path::Path,
    sync::Arc,
    time::Duration,
};

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
        settings.extend(meta.list_settings.into_iter().map(|(k, v)| (k, v.into())));
        index.settings.merge(&Value::Object(settings));
        if let Some(m) = meta.mappings {
            let m: Value = serde_json::from_str(&m).map_err(|e| es_codec::Error(e.to_string()))?;
            index.mappings = Mappings::deserialize(&m["_doc"]).unwrap_or_default();
        }
        let dir = data_dir.join("indices").join(&meta.uuid).join("0/index");
        // An index whose shard never started (e.g. a system index made just before shutdown)
        // has no shard directory.
        if !dir.exists() {
            continue;
        }
        load_shard(&mut index, &dir)
            .map_err(|e| es_codec::Error(format!("index [{}]: {}", meta.name, e.0)))?;
        replay_translog(&mut index, &dir.join("../translog"))?;
        out.insert(meta.name, index);
    }
    Ok(out)
}

/// Re-applies the operations in a shard's translog that its commit does not hold: those above
/// the commit's `local_checkpoint`, up to the checkpoint's offset in the current generation.
fn replay_translog(index: &mut Index, dir: &Path) -> es_codec::Result<()> {
    use es_codec::translog::{read, read_checkpoint};
    let Ok(bytes) = std::fs::read(dir.join("translog.ckp")) else {
        return Ok(());
    };
    let cp = read_checkpoint(&bytes)?;
    let committed = index
        .commit
        .get("local_checkpoint")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(-1);
    for generation in cp.min_generation..=cp.generation {
        let path = dir.join(format!("translog-{generation}.tlog"));
        let file = std::fs::read(&path)
            .map_err(|e| es_codec::Error(format!("{}: {e}", path.display())))?;
        let durable = if generation == cp.generation {
            (cp.offset as usize).min(file.len())
        } else {
            file.len()
        };
        for op in read(&file[..durable])?.1 {
            if op.seq_no() > committed {
                index.replay(op);
            }
        }
    }
    Ok(())
}

/// A document's place in an opened index's segments: `(segment, doc id)`.
pub type DiskPos = (usize, usize);

/// Where a live document of a [`Shard`] is, with its version and sequence number.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DiskDoc {
    segment: usize,
    doc: usize,
    pub(crate) version: u64,
    pub(crate) seq_no: u64,
}

/// The segments of a shard opened from disk. Documents stay in them until read. A clone shares
/// the segments, which keep their files open, so it can read them after a save deletes them.
#[derive(Clone)]
pub(crate) struct Shard {
    segments: Arc<Vec<SegmentReader>>,
    /// Live documents by id. Writes to the index remove the ones they replace or delete.
    ids: HashMap<String, DiskDoc>,
    /// Per segment, the paths of its nested documents (`_nested_path` terms)
    nested: Vec<Vec<String>>,
}

impl std::fmt::Debug for Shard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shard")
            .field("segments", &self.segments.len())
            .field("docs", &self.ids.len())
            .finish()
    }
}

/// The `_id` and `_source` bytes of a document's stored fields.
fn id_and_source(
    seg: &SegmentReader,
    fields: Doc,
) -> es_codec::Result<(Option<String>, Option<Vec<u8>>)> {
    let (mut id, mut source) = (None, None);
    for (n, v) in fields {
        match (seg.field_by_number(n)?.name.as_str(), v) {
            ("_id", StoredValue::Bytes(b)) => id = Some(decode_id(&b)),
            ("_source", StoredValue::Bytes(b)) => source = Some(b),
            _ => {}
        }
    }
    Ok((id, source))
}

fn document(d: &DiskDoc, source: Option<Vec<u8>>) -> es_codec::Result<Document> {
    let source = source.ok_or_else(|| es_codec::Error("document has no _source".into()))?;
    Ok(Document {
        source: serde_json::from_slice(&source).map_err(|e| es_codec::Error(e.to_string()))?,
        version: d.version,
        seq_no: d.seq_no,
    })
}

impl Shard {
    pub(crate) fn meta(&self, id: &str) -> Option<DiskDoc> {
        self.ids.get(id).copied()
    }

    /// Where a live document is.
    pub(crate) fn pos(&self, id: &str) -> Option<DiskPos> {
        self.ids.get(id).map(|d| (d.segment, d.doc))
    }

    /// A segment's doc values of `field`; `None` if it has none the codec reads.
    pub(crate) fn column(&self, segment: usize, field: &str) -> Option<DocValues> {
        if self.nested(segment, field) {
            return None;
        }
        self.segments[segment].doc_values(field).ok()
    }

    /// Whether segment `s` indexes `field` in nested documents, which hold its postings, points
    /// and doc values instead of the roots es-mini reads as documents; lookups treat such a
    /// field as one the segment cannot read.
    fn nested(&self, s: usize, field: &str) -> bool {
        self.nested[s].iter().any(|p| under(field, p))
    }

    /// Forgets a document the index replaced or deleted.
    pub(crate) fn remove(&mut self, id: &str) -> Option<DiskDoc> {
        self.ids.remove(id)
    }

    /// Reads a live document from its segment.
    pub(crate) fn load(&self, d: &DiskDoc) -> es_codec::Result<Document> {
        let seg = &self.segments[d.segment];
        document(d, id_and_source(seg, seg.document(d.doc)?)?.1)
    }

    /// The docs that may hold `term` in `field`: those whose postings do, and every doc of a
    /// segment without readable postings for the field, or `None` there if `exact`. Some may no
    /// longer be live.
    pub(crate) fn term_docs(
        &self,
        field: &str,
        term: &str,
        exact: bool,
    ) -> Option<BTreeSet<DiskPos>> {
        if field == "_id" {
            return Some(self.pos(term).into_iter().collect());
        }
        let mut out = BTreeSet::new();
        for (s, seg) in self.segments.iter().enumerate() {
            match seg.term(field, term.as_bytes()) {
                Ok(t) if !self.nested(s, field) => {
                    out.extend(t.iter().flat_map(|t| &t.docs).map(|d| (s, d.0 as usize)))
                }
                // e.g. a field this segment does not index, or a postings format we lack
                _ if exact => return None,
                _ => out.extend((0..seg.max_doc()).map(|d| (s, d))),
            }
        }
        Some(out)
    }

    /// Per doc holding `term` in `field`: its frequency, and the field's length as the norm
    /// stands for it (1 for a field without norms). `None` if a segment has no postings or norms
    /// for the field the codec can read (e.g. one that does not index it). Some docs may no longer be live.
    // ponytail: reads the field's norms once per term; cache them per query if that shows up
    pub(crate) fn term_freqs(
        &self,
        field: &str,
        term: &str,
    ) -> Option<HashMap<DiskPos, (u32, u32)>> {
        let mut out = HashMap::new();
        for (s, seg) in self.segments.iter().enumerate() {
            if self.nested(s, field) {
                return None;
            }
            let Some(t) = seg.term(field, term.as_bytes()).ok()? else {
                continue;
            };
            let norms = seg.norms(field).ok()?;
            for (doc, freq) in t.docs {
                let norm = norms
                    .as_ref()
                    .map_or(Some(1), |n| n.get(doc as usize).copied())?;
                out.insert((s, doc as usize), (freq, norm_len(norm as i8)));
            }
        }
        Some(out)
    }

    /// The docs with a value of numeric `field` that monotone `place` puts in a range, walking
    /// its points tree or else decoding its doc values, and every doc of a segment with neither
    /// readable, or `None` there if `exact`. Some may no longer be live.
    pub(crate) fn number_docs(
        &self,
        field: &str,
        place: impl Fn(i64) -> Ordering,
        exact: bool,
    ) -> Option<BTreeSet<DiskPos>> {
        let mut out = BTreeSet::new();
        for (s, seg) in self.segments.iter().enumerate() {
            if let Ok(docs) = seg.point_docs(field, &place)
                && !self.nested(s, field)
            {
                out.extend(docs.into_iter().map(|d| (s, d as usize)));
                continue;
            }
            match self.column(s, field) {
                Some(DocValues::Numbers(n)) => out.extend(
                    (n.iter().enumerate())
                        .filter(|(_, v)| v.iter().any(|v| place(*v).is_eq()))
                        .map(|(d, _)| (s, d)),
                ),
                _ if exact => return None,
                _ => out.extend((0..seg.max_doc()).map(|d| (s, d))),
            }
        }
        Some(out)
    }

    /// Where every live document is, with its id and sequence number.
    pub(crate) fn positions(&self) -> impl Iterator<Item = (DiskPos, (&str, u64))> {
        (self.ids.iter()).map(|(id, d)| ((d.segment, d.doc), (id.as_str(), d.seq_no)))
    }

    /// The live documents at a position `keep` accepts, segment by segment, read a chunk at a
    /// time; chunks without such documents are not read.
    pub(crate) fn docs<'a: 'k, 'k>(
        &'a self,
        keep: impl Fn(DiskPos) -> bool + Copy + 'k,
    ) -> impl Iterator<Item = es_codec::Result<(&'a str, Document)>> + 'k {
        self.segments.iter().enumerate().flat_map(move |(s, seg)| {
            let docs: Box<dyn Iterator<Item = _>> = match seg.documents_where(move |d| keep((s, d)))
            {
                Ok(docs) => Box::new(docs),
                Err(e) => Box::new(std::iter::once(Err(e))),
            };
            docs.filter_map(move |fields| {
                let read = || {
                    let (doc, fields) = fields?;
                    let (Some(id), source) = id_and_source(seg, fields)? else {
                        return Ok(None);
                    };
                    let Some((id, d)) = self.ids.get_key_value(&id) else {
                        return Ok(None);
                    };
                    if (d.segment, d.doc) != (s, doc) {
                        return Ok(None);
                    }
                    Ok(Some((id.as_str(), document(d, source)?)))
                };
                read().transpose()
            })
        })
    }
}

fn longs(seg: &SegmentReader, field: &str) -> Vec<Option<i64>> {
    match seg.doc_values(field) {
        Ok(DocValues::Numbers(n)) => n.iter().map(|v| v.first().copied()).collect(),
        _ => vec![],
    }
}

/// Adds the term statistics of `seg`, deleted documents included, to `stats`.
fn add_stats(stats: &mut TermStats, seg: &SegmentReader) {
    for f in seg
        .fields
        .iter()
        .filter(|f| f.index_options != IndexOptions::None && f.name != "_id")
    {
        // ponytail: only Lucene104 postings are read, which Elasticsearch writes for every field
        // but `_id` (ES812Postings, never scored); indices created with
        // `index.use_legacy_postings_format` hold every field in ES812Postings and get no stats
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
}

/// The term statistics of the latest commit in the shard directory `dir`; `None` if it has no
/// segments.
fn commit_stats(dir: &Path) -> es_codec::Result<Option<TermStats>> {
    let segments = SegmentInfos::read_latest(dir)?.segments;
    if segments.is_empty() {
        return Ok(None);
    }
    let mut stats = TermStats::default();
    for seg in segments {
        add_stats(&mut stats, &SegmentReader::open(dir, seg)?);
    }
    Ok(Some(stats))
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
    let (mut segments, mut shard) = (
        vec![],
        Shard {
            segments: Arc::default(),
            ids: HashMap::new(),
            nested: vec![],
        },
    );
    for seg in infos.segments {
        let seg = SegmentReader::open(dir, seg)?;
        // Documents are read from the segment later; a corrupt one fails here instead.
        seg.cfs
            .verify()
            .map_err(|e| es_codec::Error(format!("{}: {}", seg.seg.name, e.0)))?;
        add_stats(&mut stats, &seg);
        let (versions, seq_nos) = (longs(&seg, "_version"), longs(&seg, "_seq_no"));
        let live = seg.live_docs()?;
        let segment = segments.len();
        let mut add = |id: String, doc: usize| {
            if !live[doc] {
                return;
            }
            let at = |v: &[Option<i64>]| v.get(doc).copied().flatten().unwrap_or(0) as u64;
            let d = DiskDoc {
                segment,
                doc,
                version: at(&versions),
                seq_no: at(&seq_nos),
            };
            index.next_seq_no = index.next_seq_no.max(d.seq_no + 1);
            shard.ids.insert(id, d);
        };
        // Ids come from the `_id` postings, or from the stored fields where the codec cannot
        // read those (Elasticsearch writes `_id` in ES812Postings). Nested docs share their
        // root's `_id` and come before it, so the root's position is the one kept.
        match seg.terms("_id") {
            Ok(terms) => {
                for t in terms {
                    let id = decode_id(&t.term);
                    t.docs.iter().for_each(|d| add(id.clone(), d.0 as usize));
                }
            }
            Err(_) => {
                for (doc, fields) in seg.documents(0..seg.max_doc())?.enumerate() {
                    if let (Some(id), Some(_)) = id_and_source(&seg, fields?)? {
                        add(id, doc);
                    }
                }
            }
        }
        let paths = seg.terms("_nested_path").unwrap_or_default().into_iter();
        (shard.nested).push(
            paths
                .map(|t| String::from_utf8_lossy(&t.term).into())
                .collect(),
        );
        segments.push(seg);
    }
    shard.segments = Arc::new(segments);
    index.stats = Some(stats);
    index.disk = Some(shard);
    index.persisted = index.next_seq_no;
    Ok(())
}

/// The documents of `ix` that the saved segments do not hold: all but the `current` ids.
fn unsaved<'a>(
    ix: &'a Index,
    current: &'a HashSet<String>,
) -> es_codec::Result<Vec<(&'a str, Cow<'a, Document>)>> {
    let mut out: Vec<_> = ix
        .docs
        .iter()
        .filter(|(id, _)| !current.contains(*id))
        .map(|(id, d)| (id.as_str(), Cow::Borrowed(&**d)))
        .collect();
    if let Some(disk) = &ix.disk {
        let at: HashSet<DiskPos> = (disk.ids.iter())
            .filter(|(id, _)| !current.contains(*id))
            .map(|(_, d)| (d.segment, d.doc))
            .collect();
        if at.is_empty() {
            return Ok(out);
        }
        for doc in disk.docs(|p| at.contains(&p)) {
            let (id, d) = doc?;
            out.push((id, Cow::Owned(d)));
        }
    }
    Ok(out)
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

/// Index data for one shard segment's Lucene documents (see [`lucene_docs`]), built from the
/// mapped fields whose values are scalars or arrays of scalars. Field numbers start at 6, after
/// the metadata fields.
// ponytail: fields holding values Elasticsearch would truncate or reject but es-mini matches as
// they are (integer fractions or out-of-range values, `date_nanos` before 1970, objects at a text
// or keyword leaf) are left out of the segment; only Elasticsearch notices, as es-mini reads
// `_source` where a segment lacks a field. Writing them needs es-mini to truncate or reject those
// values at index time, as Elasticsearch does.
#[derive(Default)]
struct UserFields {
    fields: Vec<es_codec::field_infos::FieldInfo>,
    sorted_numeric: Vec<(i32, Vec<Vec<i64>>)>,
    keyword: Vec<(i32, Vec<Vec<Vec<u8>>>)>,
    binary: Vec<(i32, Vec<Option<Vec<u8>>>)>,
    norms: Vec<(i32, Vec<Option<i8>>)>,
    terms: Vec<(i32, Vec<es_codec::terms::TermPostings>)>,
    points: Vec<(i32, es_codec::points::PointValues)>,
    /// `(field number, values per doc)` of the fields mapped with `store`
    stored: Vec<(i32, Vec<Vec<String>>)>,
    /// The mapped fields no doc has a value of
    empty: Vec<String>,
}

/// Converts the values `f` maps to `Some(Some(_))`, leaving out those it maps to `Some(None)`
/// (malformed: Elasticsearch rejects their documents, and es-mini never matches them). `None`
/// if `f` maps any to `None` (es-mini matches it, but the field cannot hold it), or no value is
/// left.
fn typed<T>(
    values: &[Vec<&Value>],
    f: &dyn Fn(&Value) -> Option<Option<T>>,
) -> Option<Vec<Vec<T>>> {
    let out: Vec<Vec<T>> = (values.iter())
        .map(|vs| {
            Some(
                vs.iter()
                    .map(|v| f(v))
                    .collect::<Option<Vec<_>>>()?
                    .into_iter()
                    .flatten()
                    .collect(),
            )
        })
        .collect::<Option<_>>()?;
    out.iter().any(|v| !v.is_empty()).then_some(out)
}

/// A scalar as the string Elasticsearch indexes for a text or keyword field (`5` → "5").
fn scalar_text(v: &Value) -> Option<String> {
    (!v.is_object()).then(|| crate::postings::text(v))
}

/// A number, or a string holding one, as an integer in `min..=max`; `None` if it has a fraction or
/// is out of range (Elasticsearch truncates or rejects it, where es-mini matches it as is).
fn integer(v: &Value, min: i64, max: i64) -> Option<Option<i64>> {
    let Some(f) = crate::json_num(v) else {
        return Some(None);
    };
    let n = (v.as_i64())
        .or_else(|| v.as_str()?.parse().ok())
        .or_else(|| (f.fract() == 0.0 && f.abs() < 2f64.powi(63)).then_some(f as i64))?;
    (min..=max).contains(&n).then_some(Some(n))
}

/// Epoch nanos of a `date_nanos` value, as Elasticsearch indexes it: [`crate::date_millis`] plus
/// the sub-millisecond digits of its fraction (of a millisecond for `epoch_millis`, of a second
/// otherwise). `None` before 1970 or after 2262, which Elasticsearch rejects and es-mini matches.
pub fn date_nanos(format: Option<&str>, locale: Option<&str>, v: &Value) -> Option<Option<i64>> {
    let Some(millis) = crate::date_millis(format, locale, v) else {
        return Some(None);
    };
    let s = crate::postings::text(v);
    let number = s.parse::<f64>().is_ok();
    if number && s.starts_with('-') {
        return None;
    }
    let digits = s.split_once(['.', ',']).map_or("", |(_, f)| f);
    let digits = &digits[..digits.bytes().take_while(u8::is_ascii_digit).count()];
    let epoch_millis = number
        && format.is_none_or(|f| {
            f.split("||").find(|a| a.starts_with("epoch_")) == Some("epoch_millis")
        });
    let sub = if epoch_millis {
        digits
    } else {
        digits.get(3..).unwrap_or("")
    };
    let sub: i64 = format!("{:0<6}", &sub[..sub.len().min(6)]).parse().ok()?;
    let nanos = millis.checked_mul(1_000_000)?.checked_add(sub)?;
    (nanos >= 0).then_some(Some(nanos))
}

/// `(from, to, include_from, include_to)` of a range field's value.
type Range = (i64, i64, bool, bool);

/// A range field's value as RangeFieldMapper parses it, its bounds parsed by `end` (rounding up a `gt` or `lte` date) and made inclusive,
/// an open end at the long limits. Malformed if not an object (ES rejects the document); `None` for
/// an unknown key, an overflowing bound or `from > to` (ES rejects those too; es-mini ignores them).
fn range(v: &Value, end: &dyn Fn(&Value, bool) -> Option<Option<i64>>) -> Option<Option<Range>> {
    let Some(o) = v.as_object() else {
        return Some(None);
    };
    let (mut from, mut to, mut include_from, mut include_to) = (None, None, true, true);
    for (k, x) in o {
        let (bound, include, step) = match k.as_str() {
            "gt" | "gte" => (&mut from, &mut include_from, 1),
            "lt" | "lte" => (&mut to, &mut include_to, -1),
            _ => return None,
        };
        *include = k.ends_with('e');
        if !x.is_null() {
            // `from` rounds up when excluded, `to` when included
            let Some(n) = end(x, *include == (step < 0))? else {
                return Some(None);
            };
            *bound = Some(if *include { n } else { n.checked_add(step)? });
        }
    }
    let from = from.unwrap_or(i64::MIN + i64::from(!include_from));
    let to = to.unwrap_or(i64::MAX - i64::from(!include_to));
    (from <= to).then_some(Some((from, to, include_from, include_to)))
}

/// `BinaryRangeUtil.encodeLongRanges` of a doc's sorted ranges: their count, then each bound as
/// a sign bit, 4 bits of length and the value's significant bytes (negatives inverted).
fn encode_ranges(ranges: &[Range]) -> Vec<u8> {
    let mut out = es_codec::output::Output::default();
    out.vint(ranges.len() as i32);
    for &(from, to, ..) in ranges {
        for n in [from, to] {
            let (l, negative) = if n < 0 {
                (!n as u64, true)
            } else {
                (n as u64, false)
            };
            let extra = (64 - l.leading_zeros() as usize)
                .saturating_sub(3)
                .div_ceil(8);
            let mut b = u128::from(l).to_be_bytes()[15 - extra..].to_vec();
            if negative {
                b.iter_mut().for_each(|x| *x = !*x);
                b[0] = (b[0] & 7) | ((15 - extra as u8) << 3);
            } else {
                b[0] |= 0x80 | (extra as u8) << 3;
            }
            out.bytes(&b);
        }
    }
    out.0
}

/// A Lucene document of a saved one: the object at `path`, a `nested` mapping's path, or the
/// root's `_source` at "".
struct LuceneDoc<'a> {
    /// The saved document's position in the segment's list
    doc: usize,
    path: String,
    object: &'a Value,
}

/// The Lucene documents of `docs`, as Elasticsearch's DocumentParser orders them: each nested
/// object right before the one holding it, after those nested in it, in source order.
// ponytail: a dotted source key that crosses into a nested path (`{"c.a": 1}` with `c` nested)
// stays in its parent; Elasticsearch expands the dots and makes it a nested document
fn lucene_docs<'a>(mappings: &Mappings, docs: &[(&str, &'a Document)]) -> Vec<LuceneDoc<'a>> {
    fn walk<'a>(
        nested: &[&str],
        doc: usize,
        prefix: &str,
        v: &'a Value,
        out: &mut Vec<LuceneDoc<'a>>,
    ) {
        match v {
            Value::Array(a) => a.iter().for_each(|x| walk(nested, doc, prefix, x, out)),
            Value::Object(o) => {
                for (k, x) in o {
                    let path = format!("{prefix}{k}");
                    if !nested.contains(&path.as_str()) {
                        walk(nested, doc, &format!("{path}."), x, out);
                        continue;
                    }
                    // Elasticsearch rejects documents with anything else at a nested path.
                    let objects = crate::postings::values(x, "").into_iter();
                    for object in objects.filter(|o| o.is_object()) {
                        walk(nested, doc, &format!("{path}."), object, out);
                        let path = path.clone();
                        out.push(LuceneDoc { doc, path, object });
                    }
                }
            }
            _ => {}
        }
    }
    let nested = nested_paths(mappings);
    let mut out = vec![];
    for (doc, (_, d)) in docs.iter().enumerate() {
        walk(&nested, doc, "", &d.source, &mut out);
        let (path, object) = (String::new(), &d.source);
        out.push(LuceneDoc { doc, path, object });
    }
    out
}

fn nested_paths(mappings: &Mappings) -> Vec<&str> {
    (mappings.properties.iter())
        .filter(|(_, m)| **m == crate::FieldMapping::Nested)
        .map(|(p, _)| p.as_str())
        .collect()
}

/// Whether `field` is a path under the object at `path`.
fn under(field: &str, path: &str) -> bool {
    field.strip_prefix(path).is_some_and(|r| r.starts_with('.'))
}

/// The innermost of `nested` paths holding `path`, or "" for the root.
fn nested_scope<'a>(nested: &[&'a str], path: &str) -> &'a str {
    let holds = nested.iter().filter(|n| under(path, n));
    holds.max_by_key(|n| n.len()).unwrap_or(&"")
}

fn user_fields(
    mappings: &Mappings,
    docs: &[LuceneDoc],
    meta: &[es_codec::field_infos::FieldInfo],
) -> UserFields {
    use crate::FieldMapping as M;
    use es_codec::{field_infos::DocValuesType, terms::TermPostings};
    use std::collections::BTreeMap;
    let mut out = UserFields::default();
    // Multi-fields (`path.sub`) index the values at their parent's path.
    let fields = mappings.properties.iter().flat_map(|(path, m)| {
        let subs = (m.fields().into_iter().flatten())
            .map(move |(sub, sm)| (format!("{path}.{sub}"), path, sm));
        std::iter::once((path.clone(), path, m)).chain(subs)
    });
    // Last, `_ignored` lists the fields that left a value out per doc, like IgnoredFieldMapper
    // (unless a source field took the name, which Elasticsearch refuses).
    let ignored_mapping = M::Keyword {
        ignore_above: None,
        split_queries_on_whitespace: false,
    };
    let ignored_path = "_ignored".to_owned();
    let fields = fields.chain(
        (!mappings.properties.contains_key(&ignored_path))
            .then(|| (ignored_path.clone(), &ignored_path, &ignored_mapping)),
    );
    let mut ignored: Vec<Vec<Value>> = vec![vec![]; docs.len()];
    let nested = nested_paths(mappings);
    for (name, path, mapping) in fields {
        let ignored_now;
        // Values per doc at the dotted path: arrays flatten, nulls and absent fields are no values.
        let values: Vec<Vec<&Value>> = if *path == ignored_path {
            // Like IgnoredFieldMapper, on the root, which comes after its nested docs.
            let mut block = vec![];
            ignored_now = (docs.iter().zip(std::mem::take(&mut ignored)))
                .map(|(d, names)| {
                    block.extend(names);
                    match d.path.is_empty() {
                        true => std::mem::take(&mut block),
                        false => vec![],
                    }
                })
                .collect::<Vec<_>>();
            ignored_now.iter().map(|vs| vs.iter().collect()).collect()
        } else {
            // Each field is in the docs of the innermost nested object holding it.
            let scope = nested_scope(&nested, path);
            let relative = path[scope.len()..].trim_start_matches('.');
            (docs.iter())
                .map(|d| match d.path == scope {
                    true => crate::postings::values(d.object, relative),
                    false => vec![],
                })
                .collect()
        };
        if values.iter().all(Vec::is_empty) && !matches!(mapping, M::Nested | M::Object) {
            out.empty.push(name);
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
        let (mut point_width, mut point_dims) = (0, 1);
        let (index_options, doc_values, omit_norms) = match mapping {
            M::Text { .. } | M::TextKeyword { .. } => {
                let Some(texts) = typed(&values, &|v| scalar_text(v).map(Some)) else {
                    continue;
                };
                let mut m = BTreeMap::<Vec<u8>, Vec<(u32, Vec<u32>)>>::new();
                let mut lens = vec![];
                for (doc, ts) in (0u32..).zip(&texts) {
                    let tokens = crate::postings::positions(ts);
                    lens.push(tokens.len() as u32);
                    for (tok, position) in tokens {
                        let docs = m.entry(tok.into_bytes()).or_default();
                        match docs.last_mut() {
                            Some((d, ps)) if *d == doc => ps.push(position),
                            _ => docs.push((doc, vec![position])),
                        }
                    }
                }
                if m.is_empty() {
                    continue;
                }
                if matches!(mapping, M::Text { store: true }) {
                    out.stored.push((number, texts));
                }
                out.norms.push((
                    number,
                    lens.iter()
                        .map(|&l| (l > 0).then(|| norm_byte(l)))
                        .collect(),
                ));
                let at = |&position: &u32| es_codec::terms::Position {
                    position,
                    offsets: None,
                    payload: vec![],
                };
                let terms = m.into_iter().map(|(term, docs)| TermPostings {
                    positions: docs.iter().map(|d| d.1.iter().map(at).collect()).collect(),
                    ..TermPostings::new(
                        term,
                        docs.iter().map(|d| (d.0, d.1.len() as u32)).collect(),
                    )
                });
                out.terms.push((number, terms.collect()));
                (
                    IndexOptions::DocsAndFreqsAndPositions,
                    DocValuesType::None,
                    false,
                )
            }
            M::Keyword { ignore_above, .. } => {
                // Like ES, values longer than ignore_above UTF-16 units are not indexed.
                let fits =
                    |s: &str| ignore_above.is_none_or(|n| s.encode_utf16().count() <= n as usize);
                for (doc, vs) in values.iter().enumerate() {
                    if vs.iter().any(|v| scalar_text(v).is_some_and(|s| !fits(&s))) {
                        ignored[doc].push(Value::from(name.as_str()));
                    }
                }
                let Some(keys) = typed(&values, &|v| {
                    scalar_text(v).map(|s| fits(&s).then(|| s.into_bytes()))
                }) else {
                    continue;
                };
                let keys = dedup(keys);
                out.terms.push((number, postings(keys.clone())));
                out.keyword.push((number, keys));
                (IndexOptions::Docs, DocValuesType::SortedSet, true)
            }
            M::LongRange | M::DateRange { .. } => {
                // Like ES: a two-dimensional LongRange point per range, and binary doc values of
                // the doc's distinct ranges.
                let end = |v: &Value, round_up: bool| match mapping {
                    M::DateRange { format } => Some(crate::date_millis_in(
                        format.as_deref(),
                        None,
                        v,
                        &crate::tz::TimeZone::UTC,
                        round_up,
                    )),
                    _ => integer(v, i64::MIN, i64::MAX),
                };
                let Some(mut ranges) = typed(&values, &|v| range(v, &end)) else {
                    continue;
                };
                out.points.push((
                    number,
                    (ranges.iter().enumerate())
                        .flat_map(|(doc, rs)| {
                            rs.iter().map(move |r| {
                                let bytes = |n| es_codec::points::sortable_bytes(n, 8);
                                (doc as u32, [bytes(r.0), bytes(r.1)].concat())
                            })
                        })
                        .collect(),
                ));
                (point_width, point_dims) = (8, 2);
                ranges.iter_mut().for_each(|rs| {
                    rs.sort();
                    rs.dedup();
                });
                out.binary.push((
                    number,
                    (ranges.iter())
                        .map(|rs| (!rs.is_empty()).then(|| encode_ranges(rs)))
                        .collect(),
                ));
                (IndexOptions::None, DocValuesType::Binary, false)
            }
            // The path of nested documents (see `lucene_docs`), not a field
            M::Nested | M::Object => continue,
            M::Ip => {
                // Like ES: points and sorted-set doc values of the 16-byte encoding, no postings.
                let Some(ips) = typed(&values, &|v| Some(crate::ip_bytes(v))) else {
                    continue;
                };
                out.points.push((
                    number,
                    (ips.iter().enumerate())
                        .flat_map(|(doc, ips)| ips.iter().map(move |ip| (doc as u32, ip.to_vec())))
                        .collect(),
                ));
                point_width = 16;
                let keys = ips
                    .iter()
                    .map(|ips| ips.iter().map(|ip| ip.to_vec()).collect());
                out.keyword.push((number, dedup(keys.collect())));
                (IndexOptions::None, DocValuesType::SortedSet, true)
            }
            M::Boolean => {
                let Some(bools) = typed(&values, &|v| {
                    Some(v.as_bool().or_else(|| v.as_str()?.parse().ok()))
                }) else {
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
            M::Long
            | M::Integer
            | M::Short
            | M::Byte
            | M::Double
            | M::Float
            | M::HalfFloat
            | M::Date { .. }
            | M::DateNanos { .. } => {
                // Doc values are the sortable longs ES stores; points are their big-endian bytes.
                let sortable_long = |f: f64| {
                    let b = f.to_bits() as i64;
                    b ^ ((b >> 63) & i64::MAX)
                };
                let sortable_int = |f: f32| {
                    let b = f.to_bits() as i32;
                    i64::from(b ^ ((b >> 31) & i32::MAX))
                };
                // Like ES, numeric strings are numbers, and values that do not fit the type
                // finitely are malformed.
                let float = |v: &Value| crate::json_num(v).map(|f| f as f32);
                let int = |min, max| typed(&values, &move |v| integer(v, min, max));
                let (width, nums) = match mapping {
                    M::Long => (8, int(i64::MIN, i64::MAX)),
                    M::Integer => (4, int(i32::MIN.into(), i32::MAX.into())),
                    M::Short => (4, int(i16::MIN.into(), i16::MAX.into())),
                    M::Byte => (4, int(i8::MIN.into(), i8::MAX.into())),
                    M::Double => (
                        8,
                        typed(&values, &|v| {
                            Some(
                                crate::json_num(v)
                                    .filter(|f| f.is_finite())
                                    .map(sortable_long),
                            )
                        }),
                    ),
                    M::Float => (
                        4,
                        typed(&values, &|v| {
                            Some(float(v).filter(|f| f.is_finite()).map(sortable_int))
                        }),
                    ),
                    M::HalfFloat => (
                        2,
                        typed(&values, &|v| {
                            let finite =
                                |f: &f32| crate::half_float(crate::half_float_bits(*f)).is_finite();
                            Some(float(v).filter(finite).map(crate::half_float_sortable))
                        }),
                    ),
                    M::Date { format, locale, .. } => (
                        8,
                        typed(&values, &|v| {
                            Some(crate::date_millis(format.as_deref(), locale.as_deref(), v))
                        }),
                    ),
                    M::DateNanos { format, locale } => (
                        8,
                        typed(&values, &|v| {
                            date_nanos(format.as_deref(), locale.as_deref(), v)
                        }),
                    ),
                    _ => continue,
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
            point_dimensions: if point_width > 0 { point_dims } else { 0 },
            point_index_dimensions: if point_width > 0 { point_dims } else { 0 },
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
/// rewritten: a save writes one new segment of the documents changed since the last save, and
/// may merge a few similar-size segments into one, like Lucene's `TieredMergePolicy`.
pub fn save(data_dir: &Path, indices: &Indices) -> es_codec::Result<()> {
    let mut saving = SAVING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    commit(data_dir, indices)?;
    forget_retries(&mut saving, data_dir);
    Ok(())
}

/// Serializes saves and persists process-wide, and keeps the operations of failed translog
/// appends, by translog directory, for the next persist or sync to log first.
static SAVING: std::sync::Mutex<Retries> = std::sync::Mutex::new(Retries::new());

type Retries = std::collections::BTreeMap<std::path::PathBuf, Vec<es_codec::translog::Op>>;

/// A commit holds every write, so no translog operation under `data_dir` is left to retry.
fn forget_retries(retries: &mut Retries, data_dir: &Path) {
    retries.retain(|dir, _| !dir.starts_with(data_dir));
}

/// Elasticsearch's `index.translog.flush_threshold_size`: [`persist`] commits segments (a flush)
/// once a shard's translog outgrows it, bounding how much a restart replays.
fn flush_threshold(ix: &Index) -> u64 {
    ix.settings
        .get(crate::index_settings::TRANSLOG_FLUSH_THRESHOLD_SIZE)
        .and_then(crate::parse_bytes)
        .unwrap_or(512 << 20)
}

/// Makes the writes since the last call durable, like Elasticsearch: document writes are
/// appended to each shard's translog (one fsynced append and an in-place checkpoint, per
/// index written to), and segments are committed only when cluster metadata changed or a
/// translog outgrows `index.translog.flush_threshold_size` (default 512mb). [`open`] replays
/// the translog. Writes to indices with `index.translog.durability: async` are left for [`sync`].
pub fn persist(data_dir: &Path, indices: &mut Indices) -> es_codec::Result<()> {
    begin_persist(data_dir, indices)?.finish()
}

/// [`persist`] in two steps, for a caller that locks `indices`: this one takes the writes to log
/// (or commits, when a flush is due) and [`Pending::finish`] appends and fsyncs them without
/// `indices`, so the caller can release its lock meanwhile. Saves stay serialized until then.
pub fn begin_persist(data_dir: &Path, indices: &mut Indices) -> es_codec::Result<Pending> {
    log(data_dir, indices, false)
}

/// Like [`persist`], also logging the writes of `async` durability indices, and flushing once a
/// shard with translog operations has had no writes for 5 minutes (Elasticsearch's
/// `indices.memory.shard_inactive_time`). Run it every [`sync_interval`]. It commits no cluster
/// metadata: that is [`persist`]'s job.
pub fn sync(data_dir: &Path, indices: &mut Indices) -> es_codec::Result<()> {
    begin_sync(data_dir, indices)?.finish()
}

/// [`sync`] in two steps, like [`begin_persist`].
pub fn begin_sync(data_dir: &Path, indices: &mut Indices) -> es_codec::Result<Pending> {
    log(data_dir, indices, true)
}

/// Translog appends taken by [`begin_persist`] or [`begin_sync`], not yet durable.
pub struct Pending {
    appends: Vec<(
        std::path::PathBuf,
        es_codec::translog::TranslogCheckpoint,
        Vec<u8>,
        Vec<es_codec::translog::Op>,
    )>,
    saving: std::sync::MutexGuard<'static, Retries>,
}

impl Pending {
    /// Appends and fsyncs the taken writes to their translogs. Those not appended when one
    /// fails are logged first by the next persist or sync.
    pub fn finish(mut self) -> es_codec::Result<()> {
        while let Some((dir, cp, bytes, _)) = self.appends.first() {
            append(dir, cp, bytes)?;
            self.appends.remove(0);
        }
        Ok(())
    }
}

impl Drop for Pending {
    /// Keeps the operations not appended (a failed append or flush) for a retry.
    fn drop(&mut self) {
        for (dir, _, _, ops) in self.appends.drain(..) {
            self.saving.insert(dir, ops);
        }
    }
}

/// Elasticsearch's default `indices.memory.shard_inactive_time`.
const INACTIVE: Duration = Duration::from_secs(300);

/// The shortest `index.translog.sync_interval` (default 5s) of all indices.
// ponytail: one timer for all indices, so some sync more often than they ask (never less)
pub fn sync_interval(indices: &crate::aliases::Map<impl std::borrow::Borrow<Index>>) -> Duration {
    let default = Duration::from_secs(5);
    let every = |ix: &Index| {
        ix.settings
            .get(crate::index_settings::TRANSLOG_SYNC_INTERVAL)
            .and_then(crate::parse_time)
            .unwrap_or(default)
    };
    indices
        .values()
        .map(|ix| every(ix.borrow()))
        .min()
        .unwrap_or(default)
}

fn log(data_dir: &Path, indices: &mut Indices, all: bool) -> es_codec::Result<Pending> {
    let mut pending = Pending {
        appends: vec![],
        saving: SAVING
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    };
    let metas = metas(indices);
    let unchanged = read_indices(data_dir).is_ok_and(|saved| saved == metas);
    let mut logs = vec![];
    if unchanged {
        for meta in &metas {
            let dir = shard_dir(data_dir, &meta.uuid).join("../translog");
            let Ok(cp) = std::fs::read(dir.join("translog.ckp"))
                .map_err(io)
                .and_then(|b| es_codec::translog::read_checkpoint(&b))
            else {
                logs.clear();
                break;
            };
            logs.push((dir, cp));
        }
    }
    let mut flush = !unchanged || logs.len() != indices.len();
    if flush && all {
        return Ok(pending);
    }
    if !flush {
        for ((dir, cp), ix) in logs.into_iter().zip(indices.values_mut()) {
            let deferred =
                ix.settings.get(crate::index_settings::TRANSLOG_DURABILITY) == Some("async");
            let mut ops = pending.saving.remove(&dir).unwrap_or_default();
            if all || !deferred {
                ops.extend(ix.take_unlogged());
            }
            // The checkpoint is rewritten on every append, so its age is the shard's idle time.
            let idle = || {
                std::fs::metadata(dir.join("translog.ckp"))
                    .and_then(|m| m.modified())
                    .is_ok_and(|t| t.elapsed().is_ok_and(|e| e >= INACTIVE))
            };
            flush |= all && ops.is_empty() && cp.num_ops > 0 && idle();
            if ops.is_empty() {
                continue;
            }
            let bytes: Vec<u8> = ops.iter().flat_map(es_codec::translog::encode).collect();
            // A flush commits these ops too, so they need no append first.
            flush |= (cp.offset as u64 + bytes.len() as u64) > flush_threshold(ix);
            pending
                .appends
                .push((dir, advance(cp, &ops, bytes.len()), bytes, ops));
        }
    }
    if flush {
        commit(data_dir, indices)?;
        pending.appends.clear();
        forget_retries(&mut pending.saving, data_dir);
        // ponytail: rereads every term of every index; cache per segment if commits get slow
        for (meta, ix) in metas.iter().zip(indices.values_mut()) {
            drop(ix.take_unlogged());
            ix.committed(commit_stats(&shard_dir(data_dir, &meta.uuid))?);
        }
    }
    Ok(pending)
}

/// `cp` after appending `ops`, encoded in `len` bytes.
fn advance(
    mut cp: es_codec::translog::TranslogCheckpoint,
    ops: &[es_codec::translog::Op],
    len: usize,
) -> es_codec::translog::TranslogCheckpoint {
    let seq_nos = ops.iter().map(|o| o.seq_no());
    let (min, max) = (
        seq_nos.clone().min().unwrap_or(-1),
        seq_nos.max().unwrap_or(-1),
    );
    if cp.num_ops == 0 {
        cp.min_seq_no = min;
    }
    cp.offset += len as i64;
    cp.num_ops += ops.len() as i32;
    cp.min_seq_no = cp.min_seq_no.min(min);
    cp.max_seq_no = cp.max_seq_no.max(max);
    cp.global_checkpoint = cp.global_checkpoint.max(max);
    cp
}

/// Appends `bytes` to the current translog generation in `dir` (overwriting any torn bytes past
/// the old checkpoint), fsyncs it, then rewrites the checkpoint in place as `cp`, the one after
/// the append, as Elasticsearch does.
fn append(
    dir: &Path,
    cp: &es_codec::translog::TranslogCheckpoint,
    bytes: &[u8],
) -> es_codec::Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    let path = dir.join(format!("translog-{}.tlog", cp.generation));
    let write = || -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new().write(true).open(&path)?;
        f.set_len(cp.offset as u64 - bytes.len() as u64)?;
        f.seek(SeekFrom::End(0))?;
        f.write_all(bytes)?;
        // fdatasync, like TranslogWriter's force(false): it still persists the new length.
        f.sync_data()
    };
    write().map_err(|e| es_codec::Error(format!("{}: {e}", path.display())))?;
    // A fixed-size record rewritten in place and fsynced, like Elasticsearch's Checkpoint.write.
    let ckp = dir.join("translog.ckp");
    let write = || -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new().write(true).open(&ckp)?;
        f.write_all(&es_codec::translog::checkpoint(cp))?;
        // Same size, so no metadata to force (Checkpoint.write's force(false)).
        f.sync_data()
    };
    write().map_err(|e| es_codec::Error(format!("{}: {e}", ckp.display())))
}

/// Commits every index's documents into segments (see [`save`]); the caller holds `SAVING`.
fn commit(data_dir: &Path, indices: &Indices) -> es_codec::Result<()> {
    if save_in_place(data_dir, indices)? {
        return Ok(());
    }
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
/// some fields differently (`_seq_no` points) and Lucene rejects a field with two schemas, so an
/// Elasticsearch-written index is rewritten whole on its first save. Version 2 added text
/// positions, version 3 read dates with their mapping's format, version 4 wrote nested
/// documents and version 5 lists [`EMPTY`] fields, so older segments are rewritten too.
const WRITER: (&str, &str) = ("es-mini.writer", "5");

/// The diagnostic listing (as a JSON array) the mapped fields a segment has no values of, unlike
/// those it leaves out (mapped later, or holding values it cannot encode), which a merge must
/// not index for its docs.
const EMPTY: &str = "es-mini.empty_fields";

// Elasticsearch 9.5.5's merge policy defaults (MergePolicyConfig) for TieredMergePolicy.
const FLOOR_SEGMENT: u64 = 16 << 20;
const MAX_MERGED_SEGMENT: u64 = 5 << 30;
const SEGS_PER_TIER: f64 = 8.0;
const MAX_MERGE_AT_ONCE: usize = 16;
const DELETES_PCT_ALLOWED: f64 = 20.0;

/// Lucene 10.5.1 `TieredMergePolicy.findMerges`' first merge (natural merges, search
/// concurrency 1) of segments given as `(bytes, max_doc, del_count)`: the positions of a few
/// similar-size segments to merge into at most [`MAX_MERGED_SEGMENT`] bytes, or none while the
/// index is within its segment and deleted-doc budgets.
// ponytail: one merge per commit and ties broken by position, not name; Lucene may pick several
fn find_merge(segments: &[(u64, i32, i32)]) -> Vec<usize> {
    struct Seg {
        at: usize,
        total: u64,
        bytes: u64,
        max_doc: f64,
        del: f64,
    }
    let mut segs: Vec<Seg> = (segments.iter().enumerate())
        .map(|(at, &(total, max_doc, del))| {
            let (max_doc, del) = (f64::from(max_doc), f64::from(del));
            let live = if max_doc <= 0.0 {
                1.0
            } else {
                1.0 - del / max_doc
            };
            let bytes = (total as f64 * live) as u64;
            Seg {
                at,
                total,
                bytes,
                max_doc,
                del,
            }
        })
        .collect();
    segs.sort_by_key(|s| std::cmp::Reverse(s.bytes));
    let total_max_doc: f64 = segs.iter().map(|s| s.max_doc).sum();
    let total_del_pct = 100.0 * segs.iter().map(|s| s.del).sum::<f64>() / total_max_doc;
    let mut allowed_del = DELETES_PCT_ALLOWED * total_max_doc / 100.0;
    let min_bytes = segs.iter().map(|s| s.bytes).min().unwrap_or(0);
    let mut bytes_left: u64 = segs.iter().map(|s| s.bytes).sum();
    segs.retain(|s| {
        let too_big = s.bytes > MAX_MERGED_SEGMENT / 2
            && (total_del_pct <= DELETES_PCT_ALLOWED
                || 100.0 * s.del / s.max_doc <= DELETES_PCT_ALLOWED);
        if too_big {
            bytes_left -= s.bytes;
            allowed_del -= s.del;
        }
        !too_big
    });
    let merge_factor = MAX_MERGE_AT_ONCE.min(SEGS_PER_TIER as usize);
    let mut level = min_bytes.max(FLOOR_SEGMENT);
    let mut allowed = 0.0;
    loop {
        let count = bytes_left as f64 / level as f64;
        if count < SEGS_PER_TIER || level == MAX_MERGED_SEGMENT {
            allowed += count.ceil();
            break;
        }
        allowed += SEGS_PER_TIER;
        bytes_left -= (SEGS_PER_TIER * level as f64) as u64;
        level = (level * merge_factor as u64).min(MAX_MERGED_SEGMENT);
    }
    if segs.len() as f64 <= allowed.max(SEGS_PER_TIER)
        && segs.iter().map(|s| s.del).sum::<f64>() <= allowed_del.max(0.0).floor()
    {
        return vec![];
    }
    let floor = |b: u64| b.max(FLOOR_SEGMENT) as f64;
    let mut best: Option<(f64, Vec<&Seg>)> = None;
    for start in 0..segs.len() {
        let (mut merge, mut bytes, mut too_large) = (Vec::<&Seg>::new(), 0, false);
        for s in &segs[start..] {
            if merge.len() >= MAX_MERGE_AT_ONCE
                || (merge.len() >= merge_factor && bytes >= FLOOR_SEGMENT)
                || bytes >= MAX_MERGED_SEGMENT
            {
                break;
            }
            if bytes + s.bytes > MAX_MERGED_SEGMENT {
                too_large = true;
                // Try packing smaller segments in instead.
                if !merge.is_empty() {
                    continue;
                }
            }
            merge.push(s);
            bytes += s.bytes;
        }
        let first = merge[0];
        // Skip merges that grow their largest segment by under 50%, unless they reclaim many of
        // its deletes: they would rewrite it again and again.
        if !too_large
            && (bytes as f64) < first.bytes as f64 * 1.5
            && first.del < first.max_doc * DELETES_PCT_ALLOWED / 100.0
        {
            continue;
        }
        if merge.len() == 1 && first.del == 0.0 {
            continue;
        }
        if best.is_some() && !too_large && merge.len() < merge_factor {
            break;
        }
        let skew = if too_large {
            1.0 / merge_factor as f64
        } else {
            floor(first.bytes) / merge.iter().map(|s| floor(s.bytes)).sum::<f64>()
        };
        let before: u64 = merge.iter().map(|s| s.total).sum();
        let score = skew * (bytes as f64).powf(0.05) * (bytes as f64 / before as f64).powi(2);
        if best.as_ref().is_none_or(|(b, _)| score < *b) {
            best = Some((score, merge));
        }
    }
    best.map_or(vec![], |(_, merge)| merge.iter().map(|s| s.at).collect())
}

fn link(from: &Path, to: &Path) -> es_codec::Result<()> {
    std::fs::hard_link(from, to)
        .or_else(|_| std::fs::copy(from, to).map(drop))
        .map_err(io)
}

/// Carries the previous commit of an index (in `prev`) over to `dir`: segments that still hold
/// a live doc are hard-linked, with docs that `ix` replaced or deleted marked in new live docs.
/// When `prev` is `dir` (an in-place commit) segments stay where they are. Returns that commit,
/// the ids whose saved copy is current, and whether any saved doc was replaced or deleted; or
/// `None` when the index must be written whole (no readable previous commit, or not written by
/// es-mini).
fn reuse_segments(
    prev: &Path,
    dir: &Path,
    ix: &Index,
) -> es_codec::Result<Option<(SegmentInfos, HashSet<String>, bool)>> {
    use es_codec::{live_docs::write_liv, segment_infos::radix36};
    let Ok(mut infos) = SegmentInfos::read_latest(prev) else {
        return Ok(None);
    };
    let ours = |s: &es_codec::SegmentCommitInfo| {
        s.info.diagnostics.get(WRITER.0).map(String::as_str) == Some(WRITER.1)
    };
    if !infos.segments.iter().all(ours) {
        return Ok(None);
    }
    let in_place = prev == dir;
    let mut current = HashSet::new();
    let mut kept = vec![];
    let mut changed = false;
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
                if ix
                    .meta(&id)
                    .is_some_and(|(_, seq_no)| Some(seq_no as i64) == saved)
                {
                    current.insert(id.clone());
                } else {
                    live[doc] = false;
                }
            }
        }
        let dead = live.iter().filter(|l| !**l).count();
        changed |= dead > dead_before;
        if dead == live.len() {
            continue;
        }
        for f in seg
            .info
            .files
            .iter()
            .filter(|f| !in_place && !f.ends_with(".si"))
        {
            link(&prev.join(f), &dir.join(f))?;
        }
        let liv = |g: i64| format!("{}_{}.liv", seg.name, radix36(g as u64));
        if dead > dead_before {
            seg.del_gen = seg.del_gen.max(0) + 1;
            seg.del_count = dead as i32;
            let bytes = write_liv(&live, &seg.info.id, seg.del_gen);
            // Synced into the directory by the commit (`SegmentInfos::write`).
            es_codec::write_file_new(&dir.join(liv(seg.del_gen)), &bytes)?;
        } else if seg.del_gen != -1 && !in_place {
            link(&prev.join(liv(seg.del_gen)), &dir.join(liv(seg.del_gen)))?;
        }
        kept.push(seg);
    }
    infos.segments = kept;
    Ok(Some((infos, current, changed)))
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
        segment_writer::{NewSegment, write_segment},
        terms::TermPostings,
        uid::encode_id,
    };
    let lucene = lucene_docs(mappings, docs);
    let user = user_fields(mappings, &lucene, fields);
    // Built a doc at a time as the stored fields writer asks for them.
    let stored = (lucene.iter().enumerate()).map(|(doc, l)| {
        let (id, d) = docs[l.doc];
        let mut stored = vec![];
        if l.path.is_empty() {
            stored.push((0, StoredValue::Bytes(encode_id(id))));
            stored.push((1, StoredValue::Bytes(d.source.to_string().into_bytes())));
        }
        for (n, values) in &user.stored {
            let text = |v: &String| (*n, StoredValue::String(v.clone()));
            stored.extend(values[doc].iter().map(text));
        }
        Ok(stored)
    });
    // Like DocumentParser and SeqNoFieldMapper: nested docs share their root's `_id`, `_version`
    // and `_seq_no`; only roots have a `_primary_term`, which tells Elasticsearch them apart.
    let col =
        |f: fn(&Document) -> u64| (lucene.iter().map(|l| vec![f(docs[l.doc].1) as i64])).collect();
    let roots = (lucene.iter())
        .map(|l| match l.path.is_empty() {
            true => vec![1],
            false => vec![],
        })
        .collect();
    let numeric = [
        (2, col(|d| d.version)),
        (3, col(|d| d.seq_no)),
        (4, roots),
        (5, vec![]),
    ];
    let postings = |term: &dyn Fn(&LuceneDoc) -> Option<Vec<u8>>| {
        let mut entries: Vec<_> = (lucene.iter().zip(0..))
            .filter_map(|(l, i)| Some((term(l)?, i)))
            .collect();
        entries.sort();
        TermPostings::from_sorted(entries)
    };
    let mut all_fields = fields.to_vec();
    let nested_path = (fields.len() + user.fields.len()) as i32;
    all_fields.extend(user.fields);
    let mut terms = vec![(0, postings(&|l| Some(encode_id(docs[l.doc].0))))];
    terms.extend(user.terms);
    let paths = postings(&|l| (!l.path.is_empty()).then(|| l.path.clone().into_bytes()));
    if !paths.is_empty() {
        // A StringField, indexed like `_id`
        all_fields.push(es_codec::field_infos::FieldInfo {
            name: "_nested_path".into(),
            number: nested_path,
            ..fields[0].clone()
        });
        terms.push((nested_path, paths));
    }
    let (name, id) = segment_name(counter);
    let seg = write_segment(
        dir,
        &NewSegment {
            name: &name,
            id,
            max_doc: lucene.len() as i32,
            fields: &all_fields,
            numeric: &numeric,
            sorted_numeric: &user.sorted_numeric,
            keyword: &user.keyword,
            binary: &user.binary,
            norms: &user.norms,
            terms: &terms,
            points: &user.points,
        },
        stored,
    )?;
    Ok(ours(seg, &user.empty))
}

/// The name and id of segment number `counter` of an index. Ids are unique per segment of an
/// index; the first is the `[7; 16]` earlier saves always used.
fn segment_name(counter: i64) -> (String, [u8; 16]) {
    let mut id = [7; 16];
    id[8..].copy_from_slice(&(0x0707_0707_0707_0707 + counter as u64).to_be_bytes());
    (
        format!("_{}", es_codec::segment_infos::radix36(counter as u64)),
        id,
    )
}

/// Marks a segment as es-mini's ([`WRITER`]), with no values of the mapped fields `empty`.
fn ours(mut seg: es_codec::SegmentCommitInfo, empty: &[String]) -> es_codec::SegmentCommitInfo {
    let diagnostics = &mut seg.info.diagnostics;
    diagnostics.insert(WRITER.0.into(), WRITER.1.into());
    diagnostics.insert(EMPTY.into(), Value::from(empty.to_vec()).to_string());
    seg
}

/// Merges the segments of `infos` that [`find_merge`] picks into one new segment of their live
/// documents, in the first one's place, as Lucene's `SegmentInfos.applyMergeChanges` does. Like
/// Lucene's `SegmentMerger`, it appends the segments' live docs in the merge's order, copying
/// each format from them a field at a time, and stored fields a doc at a time. Fields a segment
/// leaves out without listing them [`EMPTY`] are left out of the merge.
// ponytail: holds the merged documents in memory; stream them if large merges need it. That is,
// their index data (every field's postings, doc values, norms and points), not their sources.
fn merge(dir: &Path, infos: &mut SegmentInfos) -> es_codec::Result<()> {
    use es_codec::{
        field_infos::DocValuesType as Dv,
        segment_writer::{NewSegment, write_segment},
        terms::TermPostings,
    };
    let size = |s: &es_codec::SegmentCommitInfo| {
        let files = s.info.files.iter();
        let bytes = files.filter_map(|f| std::fs::metadata(dir.join(f)).ok());
        (bytes.map(|m| m.len()).sum(), s.info.max_doc, s.del_count)
    };
    let picked = find_merge(&infos.segments.iter().map(size).collect::<Vec<_>>());
    let Some(&first) = picked.iter().min() else {
        return Ok(());
    };
    let readers = (picked.iter())
        .map(|&at| SegmentReader::open(dir, infos.segments[at].clone()))
        .collect::<es_codec::Result<Vec<_>>>()?;
    // Per segment, each doc's id in the merged one, `None` for deleted docs.
    let mut max_doc = 0u32;
    let mut maps: Vec<Vec<Option<u32>>> = vec![];
    for r in &readers {
        let mut next = || (max_doc, max_doc += 1).0;
        maps.push(
            r.live_docs()?
                .into_iter()
                .map(|l| l.then(&mut next))
                .collect(),
        );
    }
    let new_doc = |map: &[Option<u32>], doc: usize| map.get(doc).copied().flatten();
    let empties: Vec<HashSet<String>> = (readers.iter())
        .map(|r| {
            let empty = r.seg.info.diagnostics.get(EMPTY);
            empty
                .and_then(|e| serde_json::from_str(e).ok())
                .unwrap_or_default()
        })
        .collect();
    let covered = |name: &str| {
        // A segment without nested documents has no `_nested_path`.
        name == "_nested_path"
            || (readers.iter().zip(&empties))
                .all(|(r, e)| r.field(name).is_ok() || e.contains(name))
    };
    let meta = meta_fields();
    let mut out = UserFields::default();
    let mut numeric = vec![];
    for f in readers.iter().flat_map(|r| &r.fields) {
        let name = f.name.as_str();
        if out.fields.iter().any(|g| g.name == name) || out.empty.iter().any(|e| e == name) {
            continue;
        }
        if !covered(name) {
            continue;
        }
        let number = out.fields.len() as i32;
        let mut terms = BTreeMap::<Vec<u8>, TermPostings>::new();
        if f.index_options != IndexOptions::None {
            for (r, map) in readers.iter().zip(&maps) {
                if r.field(name).is_err() {
                    continue;
                }
                for t in r.terms(name)? {
                    let mut positions = t.positions.into_iter();
                    let m = terms
                        .entry(t.term)
                        .or_insert_with_key(|t| TermPostings::new(t.clone(), vec![]));
                    for (doc, freq) in t.docs {
                        let at = positions.next();
                        if let Some(doc) = new_doc(map, doc as usize) {
                            m.docs.push((doc, freq));
                            m.total_term_freq += u64::from(freq);
                            m.positions.extend(at);
                        }
                    }
                }
            }
            terms.retain(|_, t| !t.docs.is_empty());
        }
        let mut points = vec![];
        if f.point_dimensions > 0 {
            for (r, map) in readers.iter().zip(&maps) {
                if r.field(name).is_ok() {
                    let live = r.points(name)?.into_iter();
                    points.extend(live.filter_map(|(d, v)| Some((new_doc(map, d as usize)?, v))));
                }
            }
        }
        let wrong = || es_codec::Error(format!("unexpected doc values type for {name}"));
        let numbers = || {
            concat_live(&readers, &maps, name, |r| match r.doc_values(name)? {
                DocValues::Numbers(n) => Ok(n),
                _ => Err(wrong()),
            })
        };
        let mut dv = UserFields::default();
        let mut dv_numeric = vec![];
        let has_dv = match f.doc_values {
            Dv::None => false,
            Dv::Numeric => {
                let mut n = numbers()?;
                let has = n.iter().any(|v| !v.is_empty());
                if !has {
                    n.clear(); // as a flush writes `__soft_deletes`
                }
                dv_numeric.push((number, n));
                has
            }
            Dv::SortedNumeric => {
                let n = numbers()?;
                let has = n.iter().any(|v| !v.is_empty());
                dv.sorted_numeric.push((number, n));
                has
            }
            Dv::SortedSet => {
                let k: Vec<Vec<Vec<u8>>> =
                    concat_live(&readers, &maps, name, |r| match r.doc_values(name)? {
                        DocValues::Ordinals { ords, terms } => (ords.into_iter())
                            .map(|o| {
                                (o.into_iter())
                                    .map(|o| terms.get(o).cloned().ok_or_else(wrong))
                                    .collect()
                            })
                            .collect(),
                        _ => Err(wrong()),
                    })?;
                let has = k.iter().any(|v| !v.is_empty());
                dv.keyword.push((number, k));
                has
            }
            Dv::Binary => {
                let b = concat_live(&readers, &maps, name, |r| match r.doc_values(name)? {
                    DocValues::Bytes(b) => Ok(b),
                    _ => Err(wrong()),
                })?;
                let has = b.iter().any(Option::is_some);
                dv.binary.push((number, b));
                has
            }
            Dv::Sorted => return Err(wrong()),
        };
        let has = has_dv || !terms.is_empty() || !points.is_empty();
        if !has && !meta.iter().any(|m| m.name == name) {
            out.empty.push(f.name.clone());
            continue;
        }
        numeric.extend(dv_numeric);
        out.sorted_numeric.extend(dv.sorted_numeric);
        out.keyword.extend(dv.keyword);
        out.binary.extend(dv.binary);
        if !f.omit_norms && f.index_options != IndexOptions::None {
            let norms = concat_live(&readers, &maps, name, |r| {
                let norms = r.norms(name)?.unwrap_or_default();
                Ok(norms
                    .into_iter()
                    .map(|n| (n != 0).then_some(n as i8))
                    .collect())
            })?;
            out.norms.push((number, norms));
        }
        if !terms.is_empty() {
            out.terms.push((number, terms.into_values().collect()));
        }
        if !points.is_empty() {
            out.points.push((number, points));
        }
        out.fields.push(es_codec::field_infos::FieldInfo {
            number,
            dv_gen: -1,
            ..f.clone()
        });
    }
    // Fields no merged segment has values of
    for name in empties.first().into_iter().flatten() {
        if empties.iter().all(|e| e.contains(name)) && !out.empty.contains(name) {
            out.empty.push(name.clone());
        }
    }
    let numbers: HashMap<&str, i32> = (out.fields.iter())
        .map(|f| (f.name.as_str(), f.number))
        .collect();
    let stored = readers.iter().zip(&maps).flat_map(|(r, map)| {
        let docs: Box<dyn Iterator<Item = _>> = match r.documents_where(|d| map[d].is_some()) {
            Ok(docs) => Box::new(docs),
            Err(e) => Box::new(std::iter::once(Err(e))),
        };
        let numbers = &numbers;
        docs.map(move |doc| {
            let mut out = vec![];
            for (n, v) in doc?.1 {
                if let Some(&n) = numbers.get(r.field_by_number(n)?.name.as_str()) {
                    out.push((n, v));
                }
            }
            Ok(out)
        })
    });
    let (name, id) = segment_name(infos.counter);
    let seg = write_segment(
        dir,
        &NewSegment {
            name: &name,
            id,
            max_doc: max_doc as i32,
            fields: &out.fields,
            numeric: &numeric,
            sorted_numeric: &out.sorted_numeric,
            keyword: &out.keyword,
            binary: &out.binary,
            norms: &out.norms,
            terms: &out.terms,
            points: &out.points,
        },
        stored,
    )?;
    infos.counter += 1;
    let mut at = 0..;
    infos
        .segments
        .retain(|_| !picked.contains(&at.next().unwrap()));
    infos.segments.insert(first, ours(seg, &out.empty));
    Ok(())
}

/// Per live doc of `readers` in turn (where `maps` gives it a merged id), what `read` gives for
/// it in its segment, or the default where the segment lacks `field`.
fn concat_live<T: Default>(
    readers: &[SegmentReader],
    maps: &[Vec<Option<u32>>],
    field: &str,
    read: impl Fn(&SegmentReader) -> es_codec::Result<Vec<T>>,
) -> es_codec::Result<Vec<T>> {
    let mut out = vec![];
    for (r, map) in readers.iter().zip(maps) {
        let mut values = match r.field(field) {
            Ok(_) => read(r)?,
            Err(_) => vec![],
        };
        let live = (0..map.len()).filter(|&d| map[d].is_some());
        out.extend(live.map(|d| values.get_mut(d).map(std::mem::take).unwrap_or_default()));
    }
    Ok(out)
}

/// Cluster metadata for `indices`, as both save paths write it.
fn metas(indices: &Indices) -> Vec<es_codec::cluster_state::IndexMetadata> {
    let uuid = |name: &str| {
        let h = name.bytes().fold(0xcbf29ce484222325u64, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x100000001b3)
        });
        format!("{h:016x}{:06}", name.len() % 1_000_000)
    };
    indices
        .iter()
        .map(|(name, ix)| {
            // Indices read from an Elasticsearch directory keep its identity (index.uuid,
            // index.version.created); only indices es-mini created get generated values.
            let (lists, strings): (Vec<_>, Vec<_>) =
                ix.settings.flat().iter().partition(|(_, v)| v.is_array());
            let mut settings: BTreeMap<String, String> = strings
                .into_iter()
                .map(|(k, v)| (k.clone(), crate::setting_string(v)))
                .collect();
            let list_settings = lists
                .into_iter()
                .map(|(k, v)| {
                    let items = v.as_array().into_iter().flatten();
                    (k.clone(), items.map(crate::setting_string).collect())
                })
                .collect();
            let uuid = settings
                .entry(crate::index_settings::UUID.into())
                .or_insert_with(|| uuid(name))
                .clone();
            // Elasticsearch refuses to load index metadata without the creation version.
            settings
                .entry(crate::index_settings::VERSION_CREATED.into())
                .or_insert_with(|| crate::VERSION_CREATED.to_string());
            es_codec::cluster_state::IndexMetadata {
                name: name.clone(),
                uuid,
                settings,
                list_settings,
                mappings: serde_json::to_string(&serde_json::json!({"_doc": ix.mappings})).ok(),
            }
        })
        .collect()
}

fn shard_dir(root: &Path, uuid: &str) -> std::path::PathBuf {
    root.join("indices").join(uuid).join("0/index")
}

/// Field infos of the metadata fields es-mini writes.
fn meta_fields() -> Vec<es_codec::field_infos::FieldInfo> {
    let mut fields = es_codec::segment_writer::es_meta_fields();
    fields[3].point_dimensions = 0;
    fields[3].point_index_dimensions = 0;
    fields[3].point_num_bytes = 0;
    fields
}

/// Commits `ix` into the shard directory `dir`, reusing the commit in `prev` (the same
/// directory for an in-place commit). An in-place commit of an unchanged index writes nothing.
/// A commit afterwards deletes the files in `dir` no longer referenced. Returns whether it
/// committed.
fn write_shard(prev: &Path, dir: &Path, ix: &Index) -> es_codec::Result<bool> {
    use es_codec::segment_infos::{radix36, shard_user_data};
    let in_place = prev == dir;
    let (mut infos, current, changed) = match reuse_segments(prev, dir, ix)? {
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
            true,
        ),
    };
    let max_seq = ix.next_seq_no as i64 - 1;
    let mut owned = unsaved(ix, &current)?;
    let saved_max = infos
        .user_data
        .get("max_seq_no")
        .and_then(|v| v.parse::<i64>().ok());
    if in_place && !changed && owned.is_empty() && saved_max == Some(max_seq) {
        return Ok(false);
    }
    owned.sort_by_key(|(_, d)| d.seq_no);
    let docs: Vec<_> = owned.iter().map(|(id, d)| (*id, &**d)).collect();
    let sorted = ix
        .settings
        .flat()
        .keys()
        .any(|k| k.starts_with("index.sort."));
    // ponytail: Lucene refuses unsorted segments in a sorted index; sort docs to support it
    if sorted && !docs.is_empty() {
        return Err(es_codec::Error(
            "writing documents to an index with index.sort.* settings is unsupported".into(),
        ));
    }
    if !docs.is_empty() {
        infos.segments.push(write_docs(
            dir,
            infos.counter,
            &docs,
            &ix.mappings,
            &meta_fields(),
        )?);
        infos.counter += 1;
    }
    merge(dir, &mut infos)?;
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
    infos.write(dir)?;
    // The commit holds every operation, so the translog starts a new, empty generation.
    let translog = dir.join("../translog");
    let previous = std::fs::read(prev.join("../translog/translog.ckp")).ok();
    let generation = previous
        .and_then(|b| es_codec::translog::read_checkpoint(&b).ok())
        .map_or(1, |cp| cp.generation + 1);
    let cp = es_codec::translog::Checkpoint {
        generation,
        global_checkpoint: max_seq,
        primary_term: 1,
    };
    let (tlog, ckp) = es_codec::translog::write_empty(&infos.user_data["translog_uuid"], &cp);
    es_codec::write_file_atomic(&translog.join(format!("translog-{generation}.tlog")), &tlog)?;
    es_codec::write_file_atomic(&translog.join("translog.ckp"), &ckp)?;
    for entry in std::fs::read_dir(&translog).map_err(io)? {
        let name = entry
            .map_err(io)?
            .file_name()
            .to_string_lossy()
            .into_owned();
        if name.starts_with("translog-") && name != format!("translog-{generation}.tlog") {
            let _ = std::fs::remove_file(translog.join(&name));
        }
    }
    // Like Lucene's IndexFileDeleter: drop the previous commit's files (and leftovers of an
    // interrupted commit, or the empty commit of a new shard) once the new commit is durable.
    let mut keep: HashSet<String> =
        HashSet::from([format!("segments_{}", radix36(infos.generation))]);
    for seg in &infos.segments {
        keep.extend(seg.info.files.iter().cloned());
        if seg.del_gen != -1 {
            keep.insert(format!("{}_{}.liv", seg.name, radix36(seg.del_gen as u64)));
        }
    }
    for entry in std::fs::read_dir(dir).map_err(io)? {
        let name = entry
            .map_err(io)?
            .file_name()
            .to_string_lossy()
            .into_owned();
        if (name.starts_with('_') || name.starts_with("segments_")) && !keep.contains(&name) {
            let _ = std::fs::remove_file(dir.join(&name));
        }
    }
    Ok(true)
}

/// Commits a save that changes only documents directly into `data_dir`, one shard at a time,
/// with Lucene's commit protocol (see [`write_shard`]). Returns `false`, having written nothing,
/// when the save must rebuild the directory instead: no previous save, changed cluster metadata
/// (indices created, deleted, or reconfigured), or a shard es-mini cannot extend in place.
// ponytail: each shard commits on its own, so a crash during a write spanning indices (a bulk)
// can persist some of them, as with Elasticsearch's per-shard commits
fn save_in_place(data_dir: &Path, indices: &Indices) -> es_codec::Result<bool> {
    let metas = metas(indices);
    match read_indices(data_dir) {
        Ok(saved) if saved == metas => {}
        _ => return Ok(false),
    }
    let mut shards = vec![];
    for (meta, ix) in metas.iter().zip(indices.values()) {
        let dir = shard_dir(data_dir, &meta.uuid);
        let ours = SegmentInfos::read_latest(&dir).is_ok_and(|infos| {
            infos
                .segments
                .iter()
                .all(|s| s.info.diagnostics.get(WRITER.0).map(String::as_str) == Some(WRITER.1))
        });
        if !ours {
            return Ok(false);
        }
        shards.push((dir, ix));
    }
    for (dir, ix) in shards {
        write_shard(&dir, &dir, ix)?;
    }
    Ok(true)
}

/// Writes cluster metadata plus every index's shard into the empty directory `data_dir`,
/// reusing the segments of the snapshot at `prev` (see [`reuse_segments`]).
fn write_snapshot(data_dir: &Path, indices: &Indices, prev: &Path) -> es_codec::Result<()> {
    use es_codec::{cluster_state::GlobalMetadata, data_dir::write_empty_data_dir};
    let metas = metas(indices);
    let global = GlobalMetadata {
        cluster_uuid: "esminiClusterUuid00A".into(),
        node_id: "esminiNodeId0000000000".into(),
        term: 1,
    };
    write_empty_data_dir(data_dir, &global, &metas)?;
    for (meta, ix) in metas.iter().zip(indices.values()) {
        write_shard(
            &shard_dir(prev, &meta.uuid),
            &shard_dir(data_dir, &meta.uuid),
            ix,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn saved_index_metadata_has_uuid_and_creation_version() {
        let dir = std::env::temp_dir().join(format!("esm-disk-meta-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut all = Indices::new();
        all.insert("books".into(), Index::default());
        save(&dir, &all).unwrap();
        let metas = read_indices(&dir).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(metas[0].settings["index.uuid"], metas[0].uuid);
        assert!(metas[0].settings.contains_key("index.version.created"));
    }

    #[test]
    fn a_merge_writes_at_most_the_max_merged_segment_size() {
        let gb = 1 << 30;
        assert!(find_merge(&[(gb, 100, 0); 9]).is_empty());
        // Twelve 1.1gb segments exceed the 9 allowed; only four fit under 5gb.
        let picked = find_merge(&[(gb * 11 / 10, 100, 0); 12]);
        assert_eq!(picked, [0, 1, 2, 3]);
    }

    /// Per live document id, every value the segments of the commit in shard directory `dir`
    /// index for it (in each format) or store, sorted.
    fn index_data(dir: &Path) -> BTreeMap<String, Vec<String>> {
        let mut out = BTreeMap::<String, Vec<String>>::new();
        for seg in SegmentInfos::read_latest(dir).unwrap().segments {
            let r = SegmentReader::open(dir, seg).unwrap();
            let live = r.live_docs().unwrap();
            let mut ids = vec![String::new(); r.max_doc()];
            for t in r.terms("_id").unwrap() {
                t.docs
                    .iter()
                    .for_each(|d| ids[d.0 as usize] = decode_id(&t.term));
            }
            let mut add = |doc: usize, value: String| {
                if live[doc] {
                    out.entry(ids[doc].clone()).or_default().push(value);
                }
            };
            for f in &r.fields {
                let name = &f.name;
                for t in r.terms(name).unwrap_or_default() {
                    for (i, (doc, freq)) in t.docs.iter().enumerate() {
                        let at = t.positions.get(i);
                        add(*doc as usize, format!("{name} {:?} {freq} {at:?}", t.term));
                    }
                }
                let values: Vec<String> = match r.doc_values(name) {
                    Ok(DocValues::Numbers(n)) => n.iter().map(|v| format!("{v:?}")).collect(),
                    Ok(DocValues::Ordinals { ords, terms }) => (ords.iter())
                        .map(|o| format!("{:?}", o.iter().map(|&o| &terms[o]).collect::<Vec<_>>()))
                        .collect(),
                    Ok(DocValues::Bytes(b)) => b.iter().map(|v| format!("{v:?}")).collect(),
                    Err(_) => vec![],
                };
                // docs without a value
                let values = values
                    .into_iter()
                    .enumerate()
                    .filter(|(_, v)| v != "[]" && v != "None");
                for (doc, v) in values {
                    add(doc, format!("{name} dv {v}"));
                }
                for (doc, n) in (r.norms(name).ok().flatten().into_iter().flatten()).enumerate() {
                    add(doc, format!("{name} norm {n}"));
                }
                if f.point_dimensions > 0 {
                    for (doc, v) in r.points(name).unwrap() {
                        add(doc as usize, format!("{name} point {v:?}"));
                    }
                }
            }
            for read in r.documents_where(|_| true).unwrap() {
                let (doc, fields) = read.unwrap();
                for (n, v) in fields {
                    add(
                        doc,
                        format!("{} stored {v:?}", r.field_by_number(n).unwrap().name),
                    );
                }
            }
        }
        out.values_mut().for_each(|v| v.sort());
        out
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("esm-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn shard(dir: &Path) -> std::path::PathBuf {
        shard_dir(dir, &read_indices(dir).unwrap()[0].uuid)
    }

    /// Like Lucene's SegmentMerger, a merge copies each format from the segments it merges (so
    /// it never holds their documents), and indexes what a flush of the same documents does.
    #[test]
    fn memory_bounds_a_merge_copies_each_format_as_a_flush_would_write_it() {
        let mappings: Mappings = serde_json::from_value(json!({"properties": {
            "title": {"type": "text", "store": true},
            "tag": {"type": "keyword"},
            "n": {"type": "long"},
            "addr": {"type": "ip"},
            "span": {"type": "long_range"},
            "ok": {"type": "boolean"},
            "c": {"type": "nested", "properties": {"a": {"type": "keyword"}}},
        }}))
        .unwrap();
        let (merged, flushed) = (scratch("merge-each"), scratch("merge-flush"));
        let index = || {
            let mappings = mappings.clone();
            Indices::from([(
                "i".to_owned(),
                Index {
                    mappings,
                    ..Index::default()
                },
            )])
        };
        let (mut each, mut all) = (index(), index());
        for round in 0..12 {
            for i in 0..3 {
                let mut doc = json!({"title": format!("doc {round} of {i} {i}"), "tag": ["t", round.to_string()], "n": round * 10 + i, "ok": i % 2 == 0, "span": {"gte": round, "lt": 99}, "c": [{"a": "x"}, {"a": i.to_string()}]});
                // a field only some segments have values of
                if round % 4 == 0 {
                    doc["addr"] = json!(format!("10.0.0.{i}"));
                }
                for ix in [&mut each, &mut all].map(|i| i.get_mut("i").unwrap()) {
                    ix.put(&format!("{round}-{i}"), doc.clone());
                    // deleted docs are left out
                    ix.put("again", json!({"tag": round.to_string()}));
                }
            }
            save(&merged, &each).unwrap();
        }
        save(&flushed, &all).unwrap();
        let (merged, flushed) = (shard(&merged), shard(&flushed));
        assert!(SegmentInfos::read_latest(&merged).unwrap().segments.len() < 12);
        let want = index_data(&flushed);
        assert_eq!(want.len(), 37);
        assert_eq!(index_data(&merged), want);
    }

    /// A merge copies what its segments indexed: a field mapped after a segment was written
    /// stays out of the merge for every doc (es-mini reads it from `_source` there; Lucene would
    /// merge the values later segments indexed), while a field a segment has no values of merges
    /// with those that do.
    #[test]
    fn memory_bounds_a_merge_indexes_no_field_some_segment_left_out() {
        let dir = scratch("merge-unknown");
        let mut indices = Indices::from([("i".to_owned(), Index::default())]);
        let ix = indices.get_mut("i").unwrap();
        ix.mappings
            .properties
            .insert("n".into(), crate::FieldMapping::Long);
        for round in 0..12 {
            let ix = indices.get_mut("i").unwrap();
            if round == 6 {
                ix.mappings
                    .properties
                    .insert("late".into(), crate::FieldMapping::Long);
            }
            let mut doc = json!({"late": round});
            if round % 2 == 0 {
                doc["n"] = json!(round);
            }
            ix.put(&round.to_string(), doc);
            save(&dir, &indices).unwrap();
        }
        let shard = shard(&dir);
        let infos = SegmentInfos::read_latest(&shard).unwrap();
        let merged = (infos.segments.into_iter())
            .map(|s| SegmentReader::open(&shard, s).unwrap())
            .max_by_key(SegmentReader::max_doc)
            .unwrap();
        let rounds: Vec<i64> = (merged.terms("_id").unwrap().iter())
            .map(|t| decode_id(&t.term).parse().unwrap())
            .collect();
        assert!(rounds.len() > 1 && rounds.iter().any(|&r| r >= 6));
        assert!(merged.field("late").is_err());
        let even = rounds.iter().filter(|&&r| r % 2 == 0).count();
        assert_eq!(merged.points("n").unwrap().len(), even);
        let back = open(&dir).unwrap();
        let disk = back["i"].disk.as_ref().unwrap();
        let late = disk.number_docs("late", |v| v.cmp(&3), false).unwrap();
        assert!(late.contains(&disk.pos("3").unwrap()));
    }

    #[test]
    fn memory_bounds_snapshots_share_documents_instead_of_copying_them() {
        let dir = std::env::temp_dir().join(format!("esm-snapshot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut ix = Index::default();
        ix.put("disk", json!({"n": 1}));
        save(&dir, &Indices::from([("i".to_owned(), ix)])).unwrap();
        let mut ix = open(&dir).unwrap().remove("i").unwrap();
        ix.put("memory", json!({"n": 2}));
        let snap = ix.snapshot();
        assert!(Arc::ptr_eq(&ix.docs["memory"], &snap.docs["memory"]));
        let segments = |ix: &Index| Arc::clone(&ix.disk.as_ref().unwrap().segments);
        assert!(Arc::ptr_eq(&segments(&ix), &segments(&snap)));
        // later writes do not change it, nor deleting the files of its segments
        ix.put("disk", json!({"n": 3}));
        ix.delete("memory");
        std::fs::remove_dir_all(&dir).unwrap();
        let got: Vec<_> = snap
            .docs()
            .map(|(id, d)| (id.to_owned(), d.source.clone()))
            .collect();
        assert_eq!(
            got,
            [
                ("disk".into(), json!({"n": 1})),
                ("memory".into(), json!({"n": 2}))
            ]
        );
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
