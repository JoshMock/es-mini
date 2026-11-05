//! Loads indices from, and saves them to, an Elasticsearch data directory.
//!
//! An opened index serves its documents from the segments: only ids, versions and term
//! statistics are read into memory, and a `_source` is read from disk when a document is.

use crate::{Document, Index, Indices, Mappings, Shard, TermStats};
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

/// Opens every index of `data_dir`, each shard from `indices/<uuid>/<shard>/`.
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
        for (name, a) in meta.aliases {
            let filter = (a.filter.as_deref().map(serde_json::from_str))
                .transpose()
                .map_err(|e| es_codec::Error(e.to_string()))?;
            let alias = crate::aliases::Alias {
                filter,
                index_routing: a.index_routing,
                search_routing: a.search_routing,
                is_write_index: a.is_write_index,
                is_hidden: a.is_hidden,
            };
            index.aliases.insert(name, alias);
        }
        // An index whose shards never started (e.g. a system index made just before shutdown)
        // has no shard directory. Some but not all is damage: loading the missing ones empty
        // would let the next save erase their documents.
        let shards = index.number_of_shards();
        let missing: Vec<usize> = (0..shards)
            .filter(|&n| !shard_dir(data_dir, &meta.uuid, n).exists())
            .collect();
        if missing.len() == shards {
            continue;
        }
        if let Some(n) = missing.first() {
            let name = &meta.name;
            return Err(es_codec::Error(format!(
                "index [{name}]: shard [{n}]: missing"
            )));
        }
        for n in 0..shards {
            let dir = shard_dir(data_dir, &meta.uuid, n);
            let shard = index.shard_mut(n).1;
            load_shard(shard, &dir).map_err(|e| {
                es_codec::Error(format!("index [{}]: shard [{n}]: {}", meta.name, e.0))
            })?;
            replay_translog(shard, &dir.join("../translog"))?;
        }
        out.insert(meta.name, index);
    }
    Ok(out)
}

/// Re-applies the operations in a shard's translog that its commit does not hold: those above
/// the commit's `local_checkpoint`, up to the checkpoint's offset in the current generation.
fn replay_translog(index: &mut Shard, dir: &Path) -> es_codec::Result<()> {
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

/// Where a live document of [`Segments`] is, with its version and sequence number.
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
pub(crate) struct Segments {
    segments: Arc<Vec<SegmentReader>>,
    /// The documents live when the segments were opened, by id; shared by clones.
    ids: Arc<HashMap<String, DiskDoc>>,
    /// Per segment, the documents of `ids` that writes to the index since replaced or deleted,
    /// as Lucene's live docs: a clone shares them until either removes a document.
    removed: Vec<Arc<HashSet<usize>>>,
    /// Per segment, the paths of its nested documents (`_nested_path` terms)
    nested: Vec<Vec<String>>,
}

impl std::fmt::Debug for Segments {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Segments")
            .field("segments", &self.segments.len())
            .field("docs", &self.live().count())
            .finish()
    }
}

/// The `_id`, `_source` bytes and `_routing` of a document's stored fields.
type Stored = (Option<String>, Option<Vec<u8>>, Option<String>);

fn id_and_source(seg: &SegmentReader, fields: Doc) -> es_codec::Result<Stored> {
    let (mut id, mut source, mut routing) = (None, None, None);
    for (n, v) in fields {
        match (seg.field_by_number(n)?.name.as_str(), v) {
            ("_id", StoredValue::Bytes(b)) => id = Some(decode_id(&b)),
            ("_source", StoredValue::Bytes(b)) => source = Some(b),
            ("_routing", StoredValue::String(r)) => routing = Some(r),
            _ => {}
        }
    }
    Ok((id, source, routing))
}

fn document(d: &DiskDoc, (_, source, routing): Stored) -> es_codec::Result<Document> {
    let source = source.ok_or_else(|| es_codec::Error("document has no _source".into()))?;
    Ok(Document {
        source: serde_json::from_slice(&source).map_err(|e| es_codec::Error(e.to_string()))?,
        version: d.version,
        seq_no: d.seq_no,
        routing,
    })
}

impl Segments {
    /// A live document by id, with its id as `ids` holds it.
    fn get(&self, id: &str) -> Option<(&String, &DiskDoc)> {
        (self.ids.get_key_value(id)).filter(|(_, d)| !self.removed[d.segment].contains(&d.doc))
    }

    /// The live documents.
    fn live(&self) -> impl Iterator<Item = (&String, &DiskDoc)> {
        (self.ids.iter()).filter(|(_, d)| !self.removed[d.segment].contains(&d.doc))
    }

    pub(crate) fn meta(&self, id: &str) -> Option<DiskDoc> {
        self.get(id).map(|(_, d)| *d)
    }

    /// Where a live document is.
    pub(crate) fn pos(&self, id: &str) -> Option<DiskPos> {
        self.get(id).map(|(_, d)| (d.segment, d.doc))
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
        let d = self.meta(id)?;
        Arc::make_mut(&mut self.removed[d.segment]).insert(d.doc);
        Some(d)
    }

    /// Reads a live document from its segment.
    pub(crate) fn load(&self, d: &DiskDoc) -> es_codec::Result<Document> {
        let seg = &self.segments[d.segment];
        document(d, id_and_source(seg, seg.document(d.doc)?)?)
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
        self.live()
            .map(|(id, d)| ((d.segment, d.doc), (id.as_str(), d.seq_no)))
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
                    let stored = id_and_source(seg, fields)?;
                    let Some((id, d)) = (stored.0.as_ref()).and_then(|id| self.get(id)) else {
                        return Ok(None);
                    };
                    if (d.segment, d.doc) != (s, doc) {
                        return Ok(None);
                    }
                    Ok(Some((id.as_str(), document(d, stored)?)))
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

fn load_shard(index: &mut Shard, dir: &Path) -> es_codec::Result<()> {
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
    if let Some(changes) = infos.user_data.get(IGNORE_ABOVE) {
        index.ignore_above =
            serde_json::from_str(changes).map_err(|e| es_codec::Error(e.to_string()))?;
    }
    index.commit = infos.user_data;
    let mut ids = HashMap::new();
    let (mut segments, mut shard) = (
        vec![],
        Segments {
            segments: Arc::default(),
            ids: Arc::default(),
            removed: vec![],
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
            ids.insert(id, d);
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
                    if let (Some(id), Some(_), _) = id_and_source(&seg, fields?)? {
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
    shard.ids = Arc::new(ids);
    shard.removed = vec![Arc::default(); segments.len()];
    shard.segments = Arc::new(segments);
    index.stats = Some(stats);
    index.disk = Some(shard);
    index.persisted = index.next_seq_no;
    Ok(())
}

/// The documents of `ix` that the saved segments do not hold: all but the `current` ids.
fn unsaved<'a>(
    ix: &'a Shard,
    current: &'a HashSet<String>,
) -> es_codec::Result<Vec<(&'a str, Cow<'a, Document>)>> {
    let mut out: Vec<_> = ix
        .docs
        .iter()
        .filter(|(id, _)| !current.contains(*id))
        .map(|(id, d)| (id.as_str(), Cow::Borrowed(&**d)))
        .collect();
    if let Some(disk) = &ix.disk {
        let at: HashSet<DiskPos> = (disk.live())
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
pub(crate) fn integer(v: &Value, min: i64, max: i64) -> Option<Option<i64>> {
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
/// otherwise). `None` before 1970 or after 2262 (see [`date_nanos_rejection`]).
pub fn date_nanos(format: Option<&str>, locale: Option<&str>, v: &Value) -> Option<Option<i64>> {
    let Some(nanos) = nanos(format, locale, v) else {
        return Some(None);
    };
    i64::try_from(nanos).ok().filter(|n| *n >= 0).map(Some)
}

/// Elasticsearch's reason for rejecting a `date_nanos` value that resolves before 1970 or after
/// 2262 (`DateUtils.toLong`); `None` for values it accepts or finds malformed.
pub fn date_nanos_rejection(
    format: Option<&str>,
    locale: Option<&str>,
    v: &Value,
) -> Option<String> {
    if epoch_long_nanos(format, locale, v).is_some() {
        return None;
    }
    let n = nanos(format, locale, v)?;
    let why = match n {
        ..0 => "is before the epoch in 1970",
        0..=0x7fff_ffff_ffff_ffff => return None,
        _ => "is after 2262-04-11T23:47:16.854775807",
    };
    let instant = crate::date_math::instant_string(n);
    Some(format!(
        "date[{instant}] {why} and cannot be stored in nanosecond resolution"
    ))
}

/// Elasticsearch's `DateFieldMapper.isEpochMillis` shortcut: under the default format, a JSON
/// integer outside the int range (Jackson's `LONG`) is epoch millis, converted to nanos with
/// `TimeUnit.toNanos` (saturating) and never range-checked.
fn epoch_long_nanos(format: Option<&str>, locale: Option<&str>, v: &Value) -> Option<i64> {
    let n = v.as_i64().filter(|n| i32::try_from(*n).is_err())?;
    (format.is_none() && locale.is_none()).then(|| n.saturating_mul(1_000_000))
}

/// Epoch nanos of a `date_nanos` value, unbounded; `None` if malformed.
fn nanos(format: Option<&str>, locale: Option<&str>, v: &Value) -> Option<i128> {
    let millis = crate::date_millis(format, locale, v)?;
    let s = crate::postings::text(v);
    let number = s.parse::<f64>().is_ok();
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
    let sub: i128 = format!("{:0<6}", &sub[..sub.len().min(6)]).parse().ok()?;
    // epoch millis truncate toward zero, so a negative one's fraction counts down
    let sub = if epoch_millis && s.starts_with('-') {
        -sub
    } else {
        sub
    };
    Some(i128::from(millis) * 1_000_000 + sub)
}

/// A range key's point bytes in a `width`-byte dimension: a long's sortable bytes, or an
/// address's 16 bytes.
fn key_bytes(k: u128, width: usize) -> Vec<u8> {
    match width {
        16 => k.to_be_bytes().to_vec(),
        _ => es_codec::points::sortable_bytes(crate::mapping::key_long(k), width),
    }
}

/// BinaryRangeUtil's encoding of a doc's sorted ranges of keys: their count, then each bound
/// as the point bytes of `width` (`encodeIPRanges`, `encodeFloatRanges`, `encodeDoubleRanges`),
/// or for longs (`width` 0, `encodeLongRanges`) a sign bit, 4 bits of length and the value's
/// significant bytes (negatives inverted).
fn encode_ranges(ranges: &[(u128, u128)], width: usize) -> Vec<u8> {
    let mut out = es_codec::output::Output::default();
    out.vint(ranges.len() as i32);
    for &(from, to) in ranges {
        for k in [from, to] {
            if width > 0 {
                out.bytes(&key_bytes(k, width));
                continue;
            }
            let n = crate::mapping::key_long(k);
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
    object: Cow<'a, Value>,
}

/// The Lucene documents of `docs`, as Elasticsearch's DocumentParser orders them: each nested
/// object right before the one holding it, after those nested in it, in source order.
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
                    // Like DotExpandingXContentParser, a dotted key crossing into a nested path
                    // (`{"c.a": 1}` with `c` nested) is the object `{"a": 1}` there.
                    let crossing = (k.match_indices('.'))
                        .map(|(i, _)| prefix.len() + i)
                        .find(|&i| nested.contains(&&path[..i]));
                    if let Some(i) = crossing {
                        let object = serde_json::json!({ &path[i + 1..]: x });
                        let path = path[..i].to_owned();
                        let mut inner = vec![];
                        walk(nested, doc, &format!("{path}."), &object, &mut inner);
                        out.extend(inner.into_iter().map(|d| LuceneDoc {
                            doc: d.doc,
                            path: d.path,
                            object: Cow::Owned(d.object.into_owned()),
                        }));
                        let object = Cow::Owned(object);
                        out.push(LuceneDoc { doc, path, object });
                        continue;
                    }
                    if !nested.contains(&path.as_str()) {
                        walk(nested, doc, &format!("{path}."), x, out);
                        continue;
                    }
                    // Elasticsearch rejects documents with anything else at a nested path.
                    let objects = crate::postings::values(x, "").into_iter();
                    for object in objects.filter(|o| o.is_object()) {
                        walk(nested, doc, &format!("{path}."), object, out);
                        let (path, object) = (path.clone(), Cow::Borrowed(object));
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
        let (path, object) = (String::new(), Cow::Borrowed(&d.source));
        out.push(LuceneDoc { doc, path, object });
    }
    out
}

fn nested_paths(mappings: &Mappings) -> Vec<&str> {
    (mappings.properties.iter())
        .filter(|(_, m)| matches!(m, crate::FieldMapping::Nested { .. }))
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

/// The mapped fields of Lucene `docs`, of saved documents with sequence numbers `seq_nos`, whose
/// keyword fields' `ignore_above` went through `changes`.
fn user_fields(
    mappings: &Mappings,
    docs: &[LuceneDoc],
    seq_nos: &[u64],
    changes: &[crate::postings::LimitChange],
    meta: &[es_codec::field_infos::FieldInfo],
) -> UserFields {
    use crate::FieldMapping as M;
    use es_codec::{field_infos::DocValuesType, terms::TermPostings};
    use std::collections::BTreeMap;
    let mut out = UserFields::default();
    let fields = mappings.with_multi_fields();
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
                    true => crate::postings::values(&d.object, relative),
                    false => vec![],
                })
                .collect()
        };
        if values.iter().all(Vec::is_empty) && !mapping.is_object() {
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
                // Each with the limit its document was indexed with.
                let mut values = values;
                for (doc, vs) in values.iter_mut().enumerate() {
                    let seq_no = seq_nos[docs[doc].doc];
                    let limit = crate::postings::limit_at(changes, path, *ignore_above, seq_no);
                    let fits = |v: &&Value| {
                        scalar_text(v).is_none_or(|s| !crate::postings::ignored(limit, &s))
                    };
                    if !vs.iter().all(fits) {
                        ignored[doc].push(Value::from(name.as_str()));
                    }
                    vs.retain(fits);
                }
                let Some(keys) = typed(&values, &|v| scalar_text(v).map(|s| Some(s.into_bytes())))
                else {
                    continue;
                };
                let keys = dedup(keys);
                out.terms.push((number, postings(keys.clone())));
                out.keyword.push((number, keys));
                (IndexOptions::Docs, DocValuesType::SortedSet, true)
            }
            m @ (M::LongRange
            | M::IntegerRange
            | M::FloatRange
            | M::DoubleRange
            | M::IpRange
            | M::DateRange { .. }) => {
                // Like ES: a two-dimensional range point per range, and binary doc values of
                // the doc's distinct ranges. Like RangeFieldMapper.Range.equals, ranges with the
                // same keys but other include flags (`gt 0`, `gte 1`) are distinct.
                let flags = |v: &Value| {
                    let include = |exclusive: &str, inclusive: &str| {
                        (v.as_object().into_iter().flat_map(|o| o.keys()))
                            .rfind(|k| *k == exclusive || *k == inclusive)
                            .is_none_or(|k| k == inclusive)
                    };
                    (include("gt", "gte"), include("lt", "lte"))
                };
                let Some(mut ranges) =
                    typed(&values, &|v| Some(m.range_keys(v)?.map(|r| (r, flags(v)))))
                else {
                    continue;
                };
                let width = match m {
                    M::IntegerRange | M::FloatRange => 4,
                    M::IpRange => 16,
                    _ => 8,
                };
                out.points.push((
                    number,
                    (ranges.iter().enumerate())
                        .flat_map(|(doc, rs)| {
                            rs.iter().map(move |(r, _)| {
                                (
                                    doc as u32,
                                    [key_bytes(r.0, width), key_bytes(r.1, width)].concat(),
                                )
                            })
                        })
                        .collect(),
                ));
                (point_width, point_dims) = (width as i32, 2);
                // ponytail: ip ranges are sorted where ES writes its HashSet's order; readers
                // do not depend on it
                ranges.iter_mut().for_each(|rs| {
                    rs.sort();
                    rs.dedup();
                });
                let long = matches!(m, M::LongRange | M::IntegerRange | M::DateRange { .. });
                out.binary.push((
                    number,
                    (ranges.iter())
                        .map(|rs| {
                            let keys: Vec<_> = rs.iter().map(|r| r.0).collect();
                            (!rs.is_empty())
                                .then(|| encode_ranges(&keys, if long { 0 } else { width }))
                        })
                        .collect(),
                ));
                (IndexOptions::None, DocValuesType::Binary, false)
            }
            // The path of nested documents (see `lucene_docs`), not a field
            M::Nested { .. } | M::Object { .. } => continue,
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
                let sortable_long = crate::mapping::sortable_long;
                let sortable_int = |f| i64::from(crate::mapping::sortable_int(f));
                // Like ES, numeric strings are numbers, and values that do not fit the type
                // finitely are malformed.
                let float = |v: &Value| crate::json_num(v).map(|f| f as f32);
                // integers truncate fractions, as es-mini reads them (see `json_int`)
                let int = |min, max| {
                    typed(&values, &move |v| match crate::json_int(v) {
                        Some(n) => (min..=max).contains(&n).then_some(Some(n)),
                        None => Some(None),
                    })
                };
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
                            let (format, locale) = (format.as_deref(), locale.as_deref());
                            (epoch_long_nanos(format, locale, v).map(Some))
                                .or_else(|| date_nanos(format, locale, v))
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
        'read: for (meta, ix) in metas.iter().zip(indices.values()) {
            for n in 0..ix.number_of_shards() {
                let dir = shard_dir(data_dir, &meta.uuid, n).join("../translog");
                let Ok(cp) = std::fs::read(dir.join("translog.ckp"))
                    .map_err(io)
                    .and_then(|b| es_codec::translog::read_checkpoint(&b))
                else {
                    logs.clear();
                    break 'read;
                };
                logs.push((dir, cp));
            }
        }
    }
    let shard_count: usize = indices.values().map(Index::number_of_shards).sum();
    let mut flush =
        !unchanged || logs.len() != shard_count || indices.values().any(|ix| ix.flush_requested);
    if flush && all {
        return Ok(pending);
    }
    if !flush {
        let mut logs = logs.into_iter();
        for ix in indices.values_mut() {
            let deferred =
                ix.settings.get(crate::index_settings::TRANSLOG_DURABILITY) == Some("async");
            let threshold = flush_threshold(ix);
            for n in 0..ix.number_of_shards() {
                let (dir, cp) = logs.next().expect("a checkpoint per shard");
                let mut ops = pending.saving.remove(&dir).unwrap_or_default();
                if all || !deferred {
                    ops.extend(ix.shard_mut(n).1.take_unlogged());
                }
                // The checkpoint is rewritten on every append, so its age is the shard's idle
                // time.
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
                flush |= (cp.offset as u64 + bytes.len() as u64) > threshold;
                pending
                    .appends
                    .push((dir, advance(cp, &ops, bytes.len()), bytes, ops));
            }
        }
    }
    if flush {
        commit(data_dir, indices)?;
        pending.appends.clear();
        forget_retries(&mut pending.saving, data_dir);
        // ponytail: rereads every term of every index; cache per segment if commits get slow
        for (meta, ix) in metas.iter().zip(indices.values_mut()) {
            ix.flush_requested = false;
            for n in 0..ix.number_of_shards() {
                let shard = ix.shard_mut(n).1;
                drop(shard.take_unlogged());
                shard.committed(commit_stats(&shard_dir(data_dir, &meta.uuid, n))?);
            }
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

/// Marks the segments es-mini writes, the only ones a later save reuses: an Elasticsearch-written
/// index is rewritten whole on its first save. Version 2 added text positions, version 3 read
/// dates with their mapping's format, version 4 wrote nested documents, version 5 lists
/// [`EMPTY`] fields and version 6 indexes `_seq_no` points (Lucene rejects a field with two
/// schemas), so older segments are rewritten too.
const WRITER: (&str, &str) = ("es-mini.writer", "6");

/// The commit user data listing (as a JSON array) the shard's keyword `ignore_above` changes,
/// which its documents' terms depend on (see [`crate::Shard::ignore_above`]).
const IGNORE_ABOVE: &str = "es-mini.ignore_above";

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

/// Lucene 10.5.1 `TieredMergePolicy.findMerges` (natural merges, search concurrency 1) of
/// segments given as `(name, bytes, max_doc, del_count)`: per merge, the positions of a few
/// similar-size segments to merge into at most [`MAX_MERGED_SEGMENT`] bytes, picked until the
/// rest are within the index's segment and deleted-doc budgets (at most one of the maximum size).
fn find_merges(segments: &[(&str, u64, i32, i32)]) -> Vec<Vec<usize>> {
    struct Seg<'a> {
        at: usize,
        name: &'a str,
        total: u64,
        bytes: u64,
        max_doc: f64,
        del: f64,
    }
    let mut segs: Vec<Seg> = (segments.iter().enumerate())
        .map(|(at, &(name, total, max_doc, del))| {
            let (max_doc, del) = (f64::from(max_doc), f64::from(del));
            let live = if max_doc <= 0.0 {
                1.0
            } else {
                1.0 - del / max_doc
            };
            let bytes = (total as f64 * live) as u64;
            Seg {
                at,
                name,
                total,
                bytes,
                max_doc,
                del,
            }
        })
        .collect();
    segs.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.name.cmp(b.name)));
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
    let (allowed, allowed_del) = (allowed.max(SEGS_PER_TIER), allowed_del.max(0.0).floor());
    let floor = |b: u64| b.max(FLOOR_SEGMENT) as f64;
    let (mut merges, mut have_large) = (vec![], false);
    while !segs.is_empty()
        && (segs.len() as f64 > allowed || segs.iter().map(|s| s.del).sum::<f64>() > allowed_del)
    {
        let mut best: Option<(f64, Vec<&Seg>, bool)> = None;
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
            if best.as_ref().is_none_or(|(b, ..)| score < *b) {
                best = Some((score, merge, too_large));
            }
        }
        let Some((_, merge, too_large)) = best else {
            break;
        };
        let picked: Vec<usize> = merge.iter().map(|s| s.at).collect();
        // Like Lucene, a second merge of the maximum size waits for a later commit.
        if !have_large || !too_large {
            have_large |= too_large;
            merges.push(picked.clone());
        }
        segs.retain(|s| !picked.contains(&s.at));
    }
    merges
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
    ix: &Shard,
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

/// Elasticsearch's parent field (`Engine.ROOT_DOC_FIELD_NAME`) in a sorted index: a numeric doc
/// value of -1 on every root, which tells Lucene's sorter where each block of nested docs ends.
const ROOT_DOC_FIELD: &str = "__root_doc_for_nested";

/// A list setting of `ix` (`index.sort.*`), given as an array or a comma-separated string.
fn sort_setting(ix: &Index, key: &str) -> Option<Vec<String>> {
    Some(match ix.settings.flat().get(key)? {
        Value::Array(items) => items.iter().map(crate::setting_string).collect(),
        v => (crate::setting_string(v).split(',').map(str::trim))
            .filter(|e| !e.is_empty())
            .map(str::to_owned)
            .collect(),
    })
}

/// Why Elasticsearch 9.5.5 refuses to create the index `name` that `ix` describes for its index
/// sort (`DocumentMapper.validate`, then `IndexSortConfig.buildIndexSort`): the reason, and the
/// cause it reports for a text field, whose fielddata is disabled.
pub fn index_sort_error(ix: &Index, name: &str) -> Option<(String, Option<String>)> {
    let nested = nested_paths(&ix.mappings);
    for f in sort_setting(ix, "index.sort.field").unwrap_or_default() {
        let parent = match nested.contains(&f.as_str()) {
            true => f.as_str(),
            false => nested_scope(&nested, &f),
        };
        if !parent.is_empty() {
            let reason =
                format!("cannot apply index sort to field [{f}] under nested object [{parent}]");
            return Some((reason, None));
        }
    }
    let reason = index_sort(ix).err()?.0;
    let text = (reason.strip_prefix("docvalues not found for index sort field:["))
        .and_then(|r| r.strip_suffix(']'));
    let cause = text.map(|f| {
        format!(
            "Fielddata is disabled on [{f}] in [{name}]. Text fields are not optimised for \
             operations that require per-document field data like aggregations and sorting, so \
             these operations are disabled by default. Please use a keyword field instead. \
             Alternatively, set fielddata=true on [{f}] in order to load field data by \
             uninverting the inverted index. Note that this can use significant memory."
        )
    });
    Some((reason, cause))
}

/// The index sort of `ix`, as Elasticsearch 9.5.5's `IndexSortConfig.buildIndexSort` builds it
/// from the `index.sort.*` settings (order `asc`, mode `min` or `max` for `desc`, missing
/// `_last` by default) and the fields' doc values; empty for an unsorted index.
fn index_sort(ix: &Index) -> es_codec::Result<Vec<es_codec::segment_infos::SortField>> {
    use crate::FieldMapping as M;
    use es_codec::segment_infos::SortField;
    let list = |key: &str| sort_setting(ix, key);
    let fields = list("index.sort.field").unwrap_or_default();
    let (order, mode, missing) = (
        list("index.sort.order"),
        list("index.sort.mode"),
        list("index.sort.missing"),
    );
    let err = |what: &str, f: &str| es_codec::Error(format!("{what} index sort field:[{f}]"));
    let at = |l: &Option<Vec<String>>, i: usize| l.as_ref().and_then(|l| l.get(i)).cloned();
    (fields.iter().enumerate())
        .map(|(i, f)| {
            let order = at(&order, i);
            let reverse = order
                .as_deref()
                .is_some_and(|o| o.eq_ignore_ascii_case("desc"));
            // getDefaultSortMode compares the order exactly
            let max = at(&mode, i).map_or(order.as_deref() == Some("desc"), |m| {
                m.eq_ignore_ascii_case("max")
            });
            let first = at(&missing, i).as_deref() == Some("_first");
            // IndexFieldData.XFieldComparatorSource.missingObject
            let min = first ^ reverse;
            let numeric = |sort_type: &str, (lo, hi): (i64, i64)| SortField::SortedNumeric {
                field: f.clone(),
                sort_type: sort_type.into(),
                reverse,
                selector: max.into(),
                missing: Some(if min { lo } else { hi }),
            };
            let float = |f: f32| i64::from(crate::mapping::sortable_int(f));
            let infinities = |f: fn(f64) -> i64| (f(f64::NEG_INFINITY), f(f64::INFINITY));
            Ok(
                match (ix.mappings.field(f))
                    .filter(|m| !m.is_object())
                    .ok_or_else(|| err("unknown", f))?
                {
                    M::Keyword { .. } | M::Ip => SortField::SortedSet {
                        field: f.clone(),
                        reverse,
                        selector: max.into(),
                        missing: if min { 1 } else { 2 }, // STRING_FIRST, STRING_LAST
                    },
                    M::Long | M::Boolean | M::Date { .. } => numeric("LONG", (i64::MIN, i64::MAX)),
                    // LongValuesComparatorSource keeps date_nanos missing values non-negative.
                    M::DateNanos { .. } => numeric("LONG", (0, i64::MAX)),
                    M::Integer | M::Short | M::Byte => {
                        numeric("INT", (i32::MIN.into(), i32::MAX.into()))
                    }
                    M::Float => numeric("FLOAT", (float(f32::NEG_INFINITY), float(f32::INFINITY))),
                    M::Double => numeric("DOUBLE", infinities(crate::mapping::sortable_long)),
                    // Its comparator is a custom one, which Lucene cannot sort an index by.
                    M::HalfFloat => return Err(err("invalid", f)),
                    m if m.is_range() => {
                        let reason = format!("Sorting by range field [{f}] is not supported");
                        return Err(es_codec::Error(reason));
                    }
                    _ => return Err(err("docvalues not found for", f)),
                },
            )
        })
        .collect()
}

/// A document's value of one index sort field, ordered as Lucene's `IndexSorter` compares it
/// before a reverse sort flips it. Terms rank 0 when missing first, 1 when present, 2 when
/// missing last.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum SortKey {
    Number(i64),
    Term(u8, Vec<u8>),
}

/// The key of a document holding `numbers` or `terms` (as doc values) for `sort`.
fn sort_key(
    sort: &es_codec::segment_infos::SortField,
    numbers: &[i64],
    terms: &[&[u8]],
) -> SortKey {
    use es_codec::segment_infos::SortField as S;
    let (max, sort_type, missing) = match sort {
        S::SortedSet {
            selector, missing, ..
        } => {
            let t = if *selector == 1 {
                terms.iter().max()
            } else {
                terms.iter().min()
            };
            return match t {
                Some(t) => SortKey::Term(1, t.to_vec()),
                None => SortKey::Term(if *missing == 2 { 2 } else { 0 }, vec![]),
            };
        }
        S::SortedNumeric {
            sort_type,
            selector,
            missing,
            ..
        } => (*selector == 1, sort_type, missing),
        S::Plain {
            sort_type, missing, ..
        } => (false, sort_type, missing),
    };
    let v = if max {
        numbers.iter().max()
    } else {
        numbers.iter().min()
    };
    let v = v.copied().unwrap_or(missing.unwrap_or(0));
    // IntSorter and FloatSorter read ints (floats as their sortable bits).
    SortKey::Number(match sort_type.as_str() {
        "INT" | "FLOAT" => i64::from(v as i32),
        _ => v,
    })
}

fn cmp_keys(sort: &[es_codec::segment_infos::SortField], a: &[SortKey], b: &[SortKey]) -> Ordering {
    (sort.iter().zip(a.iter().zip(b)))
        .map(|(s, (a, b))| if s.reverse() { b.cmp(a) } else { a.cmp(b) })
        .find(|o| o.is_ne())
        .unwrap_or(Ordering::Equal)
}

/// Writes segment number `counter` of `dir` holding `docs`' `_id`, `_source`, `_version`,
/// `_seq_no` and mapped fields, keyword ones with the `ignore_above` of their document (see
/// [`crate::postings::limit_at`]). In a sorted index, docs go in `sort` order like Lucene's
/// `Sorter` puts them at a flush (each root after its nested docs, ties in indexing order).
fn write_docs(
    dir: &Path,
    counter: i64,
    docs: &[(&str, &Document)],
    mappings: &Mappings,
    changes: &[crate::postings::LimitChange],
    fields: &[es_codec::field_infos::FieldInfo],
    sort: &[es_codec::segment_infos::SortField],
) -> es_codec::Result<es_codec::SegmentCommitInfo> {
    use es_codec::{
        segment_writer::{NewSegment, write_segment},
        terms::TermPostings,
        uid::encode_id,
    };
    let index = |docs| {
        let lucene = lucene_docs(mappings, docs);
        let seq_nos: Vec<u64> = docs.iter().map(|(_, d)| d.seq_no).collect();
        let user = user_fields(mappings, &lucene, &seq_nos, changes, fields);
        (lucene, user)
    };
    let (mut lucene, mut user) = index(docs);
    let sorted: Vec<_>;
    let mut docs = docs;
    if !sort.is_empty() {
        // The keys come from the doc values the segment holds.
        let mut keys: Vec<Vec<SortKey>> = docs.iter().map(|_| vec![]).collect();
        for (at, l) in lucene.iter().enumerate().filter(|(_, l)| l.path.is_empty()) {
            for s in sort {
                let number = (user.fields.iter())
                    .find(|f| f.name == s.field())
                    .map(|f| f.number);
                let numbers = number
                    .and_then(|n| user.sorted_numeric.iter().find(|e| e.0 == n))
                    .map_or(&[][..], |(_, v)| &v[at]);
                let terms: Vec<&[u8]> = number
                    .and_then(|n| user.keyword.iter().find(|e| e.0 == n))
                    .map_or(vec![], |(_, v)| v[at].iter().map(Vec::as_slice).collect());
                keys[l.doc].push(sort_key(s, numbers, &terms));
            }
        }
        let mut order: Vec<usize> = (0..docs.len()).collect();
        order.sort_by(|&a, &b| cmp_keys(sort, &keys[a], &keys[b]));
        if !order.is_sorted() {
            // ponytail: indexes the docs twice, once for their keys; sort them by `_source`
            // values if flushes of sorted indices get slow
            sorted = order.iter().map(|&i| docs[i]).collect();
            docs = &sorted;
            (lucene, user) = index(docs);
        }
    }
    let nested_path = (fields.len() + user.fields.len()) as i32;
    let routing_number = nested_path + i32::from(lucene.iter().any(|l| !l.path.is_empty()));
    // Built a doc at a time as the stored fields writer asks for them.
    let stored = (lucene.iter().enumerate()).map(|(doc, l)| {
        let (id, d) = docs[l.doc];
        let mut stored = vec![];
        if l.path.is_empty() {
            stored.push((0, StoredValue::Bytes(encode_id(id))));
            stored.push((1, StoredValue::Bytes(d.source.to_string().into_bytes())));
            if let Some(r) = &d.routing {
                stored.push((routing_number, StoredValue::String(r.clone())));
            }
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
    // SeqNoFieldMapper's default: a long point on every doc, nested ones too
    let mut points = user.points;
    let seq_nos = (lucene.iter().zip(0..)).map(|(l, doc)| {
        (
            doc,
            es_codec::points::sortable_bytes(docs[l.doc].1.seq_no as i64, 8),
        )
    });
    points.push((3, seq_nos.collect()));
    let mut numeric = vec![
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
    // Like RoutingFieldMapper: a stored StringField, on roots only
    let routings = postings(&|l| {
        let routing = docs[l.doc].1.routing.as_ref();
        routing
            .filter(|_| l.path.is_empty())
            .map(|r| r.clone().into_bytes())
    });
    if !routings.is_empty() {
        all_fields.push(es_codec::field_infos::FieldInfo {
            name: "_routing".into(),
            number: routing_number,
            ..fields[0].clone()
        });
        terms.push((routing_number, routings));
    }
    if !sort.is_empty() {
        let number = all_fields.len() as i32;
        all_fields.push(es_codec::field_infos::FieldInfo {
            name: ROOT_DOC_FIELD.into(),
            number,
            parent: true,
            ..fields[2].clone() // a numeric doc value like `_version`
        });
        let parents = lucene.iter().map(|l| match l.path.is_empty() {
            true => vec![-1],
            false => vec![],
        });
        numeric.push((number, parents.collect()));
    }
    let (name, id) = segment_name(counter);
    let mut seg = write_segment(
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
            points: &points,
        },
        stored,
    )?;
    seg.info.index_sort = sort.to_vec();
    // Lucene marks every segment with nested docs; only a sorter reads it, to find blocks.
    seg.info.has_blocks = !sort.is_empty() && lucene.len() > docs.len();
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

/// Merges the segments of `infos` as [`find_merges`] picks, each merge into one new segment
/// of its live documents in the first one's place, as Lucene's `SegmentInfos.applyMergeChanges`
/// does. Like Lucene's `SegmentMerger`, it puts the segments' live docs in [`merge_order`],
/// copying each format from them a field at a time, postings a term at a time, and stored fields
/// a doc at a time. Fields a segment leaves out without listing them [`EMPTY`] are left out of
/// the merge.
fn merge(
    dir: &Path,
    infos: &mut SegmentInfos,
    compound_format: Option<&str>,
    sort: &[es_codec::segment_infos::SortField],
) -> es_codec::Result<()> {
    let sizes: Vec<_> = (infos.segments.iter())
        .map(|s| {
            let (bytes, max_doc, del) = segment_size(dir, s);
            (s.name.clone(), bytes, max_doc, del)
        })
        .collect();
    let named: Vec<_> = (sizes.iter())
        .map(|(name, b, m, d)| (name.as_str(), *b, *m, *d))
        .collect();
    for picked in find_merges(&named) {
        // Positions move as earlier merges replace their segments.
        let picked: Vec<usize> = (picked.iter())
            .filter_map(|&at| (infos.segments.iter()).position(|s| s.name == sizes[at].0))
            .collect();
        merge_segments(dir, infos, &picked, compound_format, sort)?;
    }
    Ok(())
}

/// A segment's `(bytes, max_doc, del_count)`.
fn segment_size(dir: &Path, s: &es_codec::SegmentCommitInfo) -> (u64, i32, i32) {
    let files = s.info.files.iter();
    let bytes = files.filter_map(|f| std::fs::metadata(dir.join(f)).ok());
    (bytes.map(|m| m.len()).sum(), s.info.max_doc, s.del_count)
}

/// Merges the segments of `infos` at `picked` (see [`merge`]).
// ponytail: holds one field's merged doc values, norms and points at a time, plus the new
// segment's encoded index files until it is written. Lucene streams both (doc by doc, to its
// outputs); do that if one field or a segment's index outgrows memory.
fn merge_segments(
    dir: &Path,
    infos: &mut SegmentInfos,
    picked: &[usize],
    compound_format: Option<&str>,
    sort: &[es_codec::segment_infos::SortField],
) -> es_codec::Result<()> {
    use es_codec::{
        field_infos::DocValuesType as Dv,
        segment_writer::{DocValuesData, FieldData, SegmentWriter},
    };
    let sizes: Vec<_> = (infos.segments.iter())
        .map(|s| segment_size(dir, s))
        .collect();
    let Some(&first) = picked.iter().min() else {
        return Ok(());
    };
    let readers = (picked.iter())
        .map(|&at| SegmentReader::open(dir, infos.segments[at].clone()))
        .collect::<es_codec::Result<Vec<_>>>()?;
    // Per segment, each doc's id in the merged one, `None` for deleted docs.
    let order = merge_order(&readers, sort)?;
    let max_doc = order.len() as u32;
    let mut maps: Vec<Vec<Option<u32>>> =
        (readers.iter()).map(|r| vec![None; r.max_doc()]).collect();
    for (new, &(s, doc)) in (0..).zip(&order) {
        maps[s][doc] = Some(new);
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
        // A segment without nested or routed documents has no `_nested_path` or `_routing`.
        name == "_nested_path"
            || name == "_routing"
            || (readers.iter().zip(&empties))
                .all(|(r, e)| r.field(name).is_ok() || e.contains(name))
    };
    let meta = es_codec::segment_writer::es_meta_fields();
    let (name, id) = segment_name(infos.counter);
    let mut segment = SegmentWriter::new(&name, id);
    let (mut fields, mut empty) = (Vec::<&str>::new(), Vec::<String>::new());
    for f in readers.iter().flat_map(|r| &r.fields) {
        let name = f.name.as_str();
        if fields.contains(&name) || empty.iter().any(|e| e == name) || !covered(name) {
            continue;
        }
        let terms = (f.index_options != IndexOptions::None)
            .then(|| merge_terms(&readers, &maps, name))
            .transpose()?;
        let mut terms = terms.into_iter().flatten().peekable();
        let mut points = vec![];
        if f.point_dimensions > 0 {
            for (r, map) in readers.iter().zip(&maps) {
                if r.field(name).is_ok() {
                    let live = r.points(name)?.into_iter();
                    points.extend(live.filter_map(|(d, v)| Some((new_doc(map, d as usize)?, v))));
                }
            }
            points.sort_by_key(|p| p.0);
        }
        let wrong = || es_codec::Error(format!("unexpected doc values type for {name}"));
        let (mut numbers, mut bytes) = (vec![], vec![]);
        let mut keywords: Vec<Vec<Vec<u8>>> = vec![];
        let has_dv = match f.doc_values {
            Dv::None => false,
            Dv::Numeric | Dv::SortedNumeric => {
                numbers = by_merged_doc(&readers, &maps, name, |r| match r.doc_values(name)? {
                    DocValues::Numbers(n) => Ok(n),
                    _ => Err(wrong()),
                })?;
                numbers.iter().any(|v| !v.is_empty())
            }
            Dv::SortedSet => {
                keywords = by_merged_doc(&readers, &maps, name, |r| match r.doc_values(name)? {
                    DocValues::Ordinals { ords, terms } => (ords.into_iter())
                        .map(|o| {
                            (o.into_iter())
                                .map(|o| terms.get(o).cloned().ok_or_else(wrong))
                                .collect()
                        })
                        .collect(),
                    _ => Err(wrong()),
                })?;
                keywords.iter().any(|v| !v.is_empty())
            }
            Dv::Binary => {
                bytes = by_merged_doc(&readers, &maps, name, |r| match r.doc_values(name)? {
                    DocValues::Bytes(b) => Ok(b),
                    _ => Err(wrong()),
                })?;
                bytes.iter().any(Option::is_some)
            }
            Dv::Sorted => return Err(wrong()),
        };
        if !has_dv
            && terms.peek().is_none()
            && points.is_empty()
            && !meta.iter().any(|m| m.name == name)
        {
            empty.push(f.name.clone());
            continue;
        }
        let doc_values = match f.doc_values {
            Dv::Numeric if !has_dv => {
                Some(DocValuesData::Numeric(&[])) // as a flush writes `__soft_deletes`
            }
            Dv::Numeric => Some(DocValuesData::Numeric(&numbers)),
            Dv::SortedNumeric => Some(DocValuesData::SortedNumeric(&numbers)),
            Dv::SortedSet => Some(DocValuesData::Keyword(&keywords)),
            Dv::Binary => Some(DocValuesData::Binary(&bytes)),
            _ => None,
        };
        let mut norms = vec![];
        let has_norms = !f.omit_norms && f.index_options != IndexOptions::None;
        if has_norms {
            norms = by_merged_doc(&readers, &maps, name, |r| {
                let norms = r.norms(name)?.unwrap_or_default();
                Ok(norms
                    .into_iter()
                    .map(|n| (n != 0).then_some(n as i8))
                    .collect())
            })?;
        }
        let info = es_codec::field_infos::FieldInfo {
            number: fields.len() as i32,
            dv_gen: -1,
            ..f.clone()
        };
        let data = FieldData {
            doc_values,
            norms: has_norms.then_some(&norms[..]),
            terms: &[],
            points: (!points.is_empty()).then_some(&points[..]),
        };
        segment.add_streamed(info, data, terms)?;
        fields.push(name);
    }
    // Fields no merged segment has values of
    for name in empties.first().into_iter().flatten() {
        if empties.iter().all(|e| e.contains(name)) && !empty.contains(name) {
            empty.push(name.clone());
        }
    }
    let numbers: HashMap<&str, i32> = (fields.iter().enumerate())
        .map(|(n, &f)| (f, n as i32))
        .collect();
    // Each segment's docs come in doc order, read a chunk at a time.
    let mut streams: Vec<Box<dyn Iterator<Item = _>>> = vec![];
    for (r, map) in readers.iter().zip(&maps) {
        match r.documents_where(|d| map[d].is_some()) {
            Ok(docs) => streams.push(Box::new(docs)),
            Err(e) => streams.push(Box::new(std::iter::once(Err(e)))),
        }
    }
    let stored = order.iter().map(|&(s, doc)| {
        let (read, fields) = streams[s]
            .next()
            .unwrap_or_else(|| Ok((usize::MAX, vec![])))?;
        if read != doc {
            return Err(es_codec::Error(format!("merge read doc {read}, not {doc}")));
        }
        let mut out = vec![];
        for (n, v) in fields {
            if let Some(&n) = numbers.get(readers[s].field_by_number(n)?.name.as_str()) {
                out.push((n, v));
            }
        }
        Ok(out)
    });
    // `MergePolicy.size`: bytes prorated by the share of live docs.
    let live_bytes = |&(bytes, max_doc, del): &(u64, i32, i32)| match max_doc {
        ..=0 => bytes,
        _ => (bytes as f64 * (1.0 - f64::from(del) / f64::from(max_doc))) as u64,
    };
    let total = sizes.iter().map(live_bytes).sum();
    let mut seg = segment.finish(dir, max_doc as i32, stored, |bytes| {
        use_compound_file(compound_format, bytes, total)
    })?;
    seg.info.index_sort = sort.to_vec();
    seg.info.has_blocks = readers.iter().any(|r| r.seg.info.has_blocks);
    infos.counter += 1;
    let mut at = 0..;
    infos
        .segments
        .retain(|_| !picked.contains(&at.next().unwrap()));
    infos.segments.insert(first, ours(seg, &empty));
    Ok(())
}

/// The live docs of `readers` as `(reader, doc)`, in their merged segment's order: one segment
/// after another, or for an index `sort` Lucene's `MultiSorter` order, which merges the
/// segments' blocks (a root after its nested docs) by the roots' keys, ties to the earlier
/// segment.
fn merge_order(
    readers: &[SegmentReader],
    sort: &[es_codec::segment_infos::SortField],
) -> es_codec::Result<Vec<(usize, usize)>> {
    let mut order = vec![];
    let mut blocks = vec![];
    for (s, r) in readers.iter().enumerate() {
        let live = r.live_docs()?;
        if sort.is_empty() {
            order.extend((0..live.len()).filter(|&d| live[d]).map(|d| (s, d)));
            continue;
        }
        let numbers = |field: &str| match r.doc_values(field) {
            Ok(DocValues::Numbers(n)) => Ok(n),
            _ => Err(es_codec::Error(format!(
                "{}: no numbers in {field}",
                r.seg.name
            ))),
        };
        // Only roots have a `_primary_term`.
        let roots = numbers("_primary_term")?;
        let columns = (sort.iter())
            .map(|f| {
                r.field(f.field())
                    .ok()
                    .map(|_| r.doc_values(f.field()))
                    .transpose()
            })
            .collect::<es_codec::Result<Vec<_>>>()?;
        let mut list = std::collections::VecDeque::new();
        let mut start = 0;
        for doc in (0..r.max_doc()).filter(|&d| roots.get(d).is_some_and(|v| !v.is_empty())) {
            if live[doc] {
                let keys = (sort.iter().zip(&columns))
                    .map(|(f, c)| match c {
                        Some(DocValues::Numbers(n)) => sort_key(f, &n[doc], &[]),
                        Some(DocValues::Ordinals { ords, terms }) => {
                            let held: Vec<&[u8]> =
                                ords[doc].iter().map(|&o| &terms[o][..]).collect();
                            sort_key(f, &[], &held)
                        }
                        _ => sort_key(f, &[], &[]),
                    })
                    .collect::<Vec<_>>();
                list.push_back((keys, start..doc + 1));
            }
            start = doc + 1;
        }
        blocks.push(list);
    }
    // `min_by` keeps the first of equal keys: the earlier segment's.
    while let Some(s) = (0..blocks.len())
        .filter(|&s| !blocks[s].is_empty())
        .min_by(|&a, &b| cmp_keys(sort, &blocks[a][0].0, &blocks[b][0].0))
    {
        let Some((_, docs)) = blocks[s].pop_front() else {
            break;
        };
        order.extend(docs.map(|d| (s, d)));
    }
    Ok(order)
}

/// The terms of `field` in the segments of `readers` (whose docs `maps` gives merged ids), merged
/// in term order a term at a time as Lucene's `MultiTermsEnum` does, without deleted docs or
/// the terms left with none.
// ponytail: picks the next term by scanning the segments; a priority queue (as Lucene's) if a
// merge ever takes more than MAX_MERGE_AT_ONCE.
fn merge_terms<'a>(
    readers: &'a [SegmentReader],
    maps: &'a [Vec<Option<u32>>],
    field: &str,
) -> es_codec::Result<impl Iterator<Item = es_codec::Result<es_codec::terms::TermPostings>> + 'a> {
    let mut streams = vec![];
    for (r, map) in readers.iter().zip(maps) {
        if r.field(field).is_ok() {
            streams.push((r.term_stream(field)?.peekable(), map));
        }
    }
    Ok(std::iter::from_fn(move || {
        loop {
            let mut min: Option<Vec<u8>> = None;
            for (s, _) in &mut streams {
                match s.peek() {
                    Some(Err(_)) => return s.next(),
                    Some(Ok(t)) if min.as_ref().is_none_or(|m| t.term < *m) => {
                        min = Some(t.term.clone());
                    }
                    _ => {}
                }
            }
            let mut out = es_codec::terms::TermPostings::new(min?, vec![]);
            for (s, map) in &mut streams {
                let Some(Ok(t)) = s.next_if(|t| t.as_ref().is_ok_and(|t| t.term == out.term))
                else {
                    continue;
                };
                let mut positions = t.positions.into_iter();
                for (doc, freq) in t.docs {
                    let at = positions.next();
                    if let Some(doc) = map.get(doc as usize).copied().flatten() {
                        out.docs.push((doc, freq));
                        out.total_term_freq += u64::from(freq);
                        out.positions.extend(at);
                    }
                }
            }
            if !out.docs.is_sorted() {
                // a sorted merge interleaves the segments' docs
                let mut at: Vec<usize> = (0..out.docs.len()).collect();
                at.sort_by_key(|&i| out.docs[i].0);
                out.docs = at.iter().map(|&i| out.docs[i]).collect();
                if !out.positions.is_empty() {
                    let mut positions = std::mem::take(&mut out.positions);
                    out.positions = at
                        .iter()
                        .map(|&i| std::mem::take(&mut positions[i]))
                        .collect();
                }
            }
            if !out.docs.is_empty() {
                return Some(Ok(out));
            }
        }
    }))
}

/// Lucene 10.5.1's `MergePolicy.useCompoundFile` as Elasticsearch 9.5.5's `MergePolicyConfig`
/// configures it from `index.compound_format` (default `1gb`): whether a merged segment of
/// `merged` bytes goes into a compound file, `total` being the (live) bytes of the index's
/// segments. `true`/`false` always/never, a ratio for at most that share of `total`, a byte
/// size for at most that many bytes. Flushed segments are always compound.
fn use_compound_file(compound_format: Option<&str>, merged: u64, total: u64) -> bool {
    let setting = compound_format.unwrap_or("1gb").trim();
    let (ratio, max) = if setting.eq_ignore_ascii_case("true") {
        (1.0, u64::MAX)
    } else if setting.eq_ignore_ascii_case("false") {
        (0.0, u64::MAX)
    } else if let Some(bytes) = setting.ends_with('b').then(|| crate::parse_bytes(setting)) {
        // Elasticsearch refuses invalid values when the setting is made; fall back to the default.
        (1.0, bytes.unwrap_or(1 << 30))
    } else {
        (setting.parse().unwrap_or(1.0), u64::MAX)
    };
    ratio > 0.0 && merged <= max && (ratio >= 1.0 || merged as f64 <= ratio * total as f64)
}

/// Per doc of a merge of `readers` (where `maps` gives their live docs merged ids), what `read`
/// gives for it in its segment, or the default where the segment lacks `field`.
fn by_merged_doc<T: Default>(
    readers: &[SegmentReader],
    maps: &[Vec<Option<u32>>],
    field: &str,
    read: impl Fn(&SegmentReader) -> es_codec::Result<Vec<T>>,
) -> es_codec::Result<Vec<T>> {
    let mut out: Vec<T> = maps
        .iter()
        .flatten()
        .flatten()
        .map(|_| T::default())
        .collect();
    for (r, map) in readers.iter().zip(maps) {
        let mut values = match r.field(field) {
            Ok(_) => read(r)?,
            Err(_) => vec![],
        };
        for (d, new) in map.iter().enumerate() {
            if let (Some(new), Some(v)) = (new, values.get_mut(d)) {
                out[*new as usize] = std::mem::take(v);
            }
        }
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
                aliases: (ix.aliases.iter())
                    .map(|(name, a)| {
                        let alias = es_codec::cluster_state::AliasMetadata {
                            filter: a.filter.as_ref().map(Value::to_string),
                            index_routing: a.index_routing.clone(),
                            search_routing: a.search_routing.clone(),
                            is_write_index: a.is_write_index,
                            is_hidden: a.is_hidden,
                        };
                        (name.clone(), alias)
                    })
                    .collect(),
            }
        })
        .collect()
}

/// The Lucene directory of shard `n` of the index `uuid`.
fn shard_dir(root: &Path, uuid: &str, n: usize) -> std::path::PathBuf {
    root.join("indices")
        .join(uuid)
        .join(n.to_string())
        .join("index")
}

/// Every shard of `ix` up to `index.number_of_shards`, with its number: those not written yet
/// are empty.
fn shards(ix: &Index) -> impl Iterator<Item = (usize, &Shard)> {
    static EMPTY: std::sync::LazyLock<Shard> = std::sync::LazyLock::new(Shard::default);
    (0..ix.number_of_shards()).map(|n| (n, ix.shards().get(n).unwrap_or(&EMPTY)))
}

/// Commits `shard` of `ix` into the shard directory `dir`, reusing the commit in `prev` (the same
/// directory for an in-place commit). An in-place commit of an unchanged index writes nothing.
/// A commit afterwards deletes the files in `dir` no longer referenced. Returns whether it
/// committed.
fn write_shard(prev: &Path, dir: &Path, ix: &Index, shard: &Shard) -> es_codec::Result<bool> {
    use es_codec::segment_infos::{radix36, shard_user_data};
    let in_place = prev == dir;
    let (mut infos, current, changed) = match reuse_segments(prev, dir, shard)? {
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
    let max_seq = shard.max_seq_no();
    let mut owned = unsaved(shard, &current)?;
    let saved_max = infos
        .user_data
        .get("max_seq_no")
        .and_then(|v| v.parse::<i64>().ok());
    if in_place && !changed && owned.is_empty() && saved_max == Some(max_seq) {
        return Ok(false);
    }
    owned.sort_by_key(|(_, d)| d.seq_no);
    let docs: Vec<_> = owned.iter().map(|(id, d)| (*id, &**d)).collect();
    let sort = index_sort(ix)?;
    if !docs.is_empty() {
        infos.segments.push(write_docs(
            dir,
            infos.counter,
            &docs,
            &ix.mappings,
            &shard.ignore_above,
            &es_codec::segment_writer::es_meta_fields(),
            &sort,
        )?);
        infos.counter += 1;
    }
    merge(
        dir,
        &mut infos,
        ix.settings.get("index.compound_format"),
        &sort,
    )?;
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
        if let Some(v) = shard.commit.get(key) {
            infos.user_data.insert(key.into(), v.clone());
        }
    }
    if !shard.ignore_above.is_empty() {
        let changes = serde_json::to_string(&shard.ignore_above)
            .map_err(|e| es_codec::Error(e.to_string()))?;
        infos.user_data.insert(IGNORE_ABOVE.into(), changes);
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
    let mut writes = vec![];
    for (meta, ix) in metas.iter().zip(indices.values()) {
        for (n, shard) in shards(ix) {
            let dir = shard_dir(data_dir, &meta.uuid, n);
            let ours = SegmentInfos::read_latest(&dir).is_ok_and(|infos| {
                infos
                    .segments
                    .iter()
                    .all(|s| s.info.diagnostics.get(WRITER.0).map(String::as_str) == Some(WRITER.1))
            });
            if !ours {
                return Ok(false);
            }
            writes.push((dir, ix, shard));
        }
    }
    for (dir, ix, shard) in writes {
        write_shard(&dir, &dir, ix, shard)?;
    }
    Ok(true)
}

/// Writes cluster metadata plus every index's shards into the empty directory `data_dir`,
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
        for (n, shard) in shards(ix) {
            write_shard(
                &shard_dir(prev, &meta.uuid, n),
                &shard_dir(data_dir, &meta.uuid, n),
                ix,
                shard,
            )?;
        }
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
        assert!(find_merges(&[("_0", gb, 100, 0); 9]).is_empty());
        // Twelve 1.1gb segments exceed the 9 allowed; only four fit under 5gb.
        let names: Vec<String> = (0..12).map(|i| format!("_{i:02}")).collect();
        let segments: Vec<_> = (names.iter())
            .map(|n| (n.as_str(), gb * 11 / 10, 100, 0))
            .collect();
        assert_eq!(find_merges(&segments), [[0, 1, 2, 3]]);
    }

    #[test]
    fn disk_fidelity_merges_pick_several_and_break_ties_by_name() {
        // Thirty 1mb segments, whose names sort apart from their positions (`_10` before `_2`):
        // like Lucene, two merges of the first sixteen and the other fourteen by name.
        let names: Vec<String> = (0..30).map(|i| format!("_{i}")).collect();
        let segments: Vec<_> = (names.iter())
            .map(|n| (n.as_str(), 1 << 20, 10, 0))
            .collect();
        let first = [0, 1, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 2, 20, 21, 22];
        let second = [23, 24, 25, 26, 27, 28, 29, 3, 4, 5, 6, 7, 8, 9];
        assert_eq!(find_merges(&segments), [&first[..], &second[..]]);
    }

    #[test]
    fn merged_segments_are_compound_as_index_compound_format_says() {
        let (gb, mb) = (1u64 << 30, 1u64 << 20);
        // The default, 1gb: compound up to 1gb, whatever the index's size.
        assert!(use_compound_file(None, gb, gb));
        assert!(!use_compound_file(None, gb + 1, 100 * gb));
        assert!(use_compound_file(Some("true"), 100 * gb, gb));
        assert!(!use_compound_file(Some("FALSE"), 1, gb));
        assert!(use_compound_file(Some("10mb"), 10 * mb, gb));
        assert!(!use_compound_file(Some("10mb"), 10 * mb + 1, gb));
        // A ratio: compound while at most that share of the index's bytes.
        assert!(use_compound_file(Some("0.1"), 100, 1000));
        assert!(!use_compound_file(Some("0.1"), 101, 1000));
        assert!(!use_compound_file(Some("0"), 1, 1000));
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
        shard_dir(dir, &read_indices(dir).unwrap()[0].uuid, 0)
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
        let disk = back["i"].shards[0].disk.as_ref().unwrap();
        let late = disk.number_docs("late", |v| v.cmp(&3), false).unwrap();
        assert!(late.contains(&disk.pos("3").unwrap()));
    }

    /// Like Lucene under Elasticsearch's IndexSortConfig, a sorted index's segments, flushed or
    /// merged, record the index sort and hold their documents in its order, each root right
    /// after its nested docs and marked with the parent field Elasticsearch configures.
    #[test]
    fn disk_fidelity_sorted_indices_write_segments_in_sort_order() {
        use es_codec::segment_infos::SortField;
        let mappings: Mappings = serde_json::from_value(json!({"properties": {
            "k": {"type": "keyword"},
            "n": {"type": "long"},
            "c": {"type": "nested", "properties": {"a": {"type": "keyword"}}},
        }}))
        .unwrap();
        let index = || {
            let mut ix = Index {
                mappings: mappings.clone(),
                ..Index::default()
            };
            let sort = json!({"field": ["k", "n"], "order": ["desc", "asc"]});
            ix.settings.merge(&json!({"index": {"sort": sort}}));
            Indices::from([("i".to_owned(), ix)])
        };
        let (merged, flushed) = (scratch("sorted-merge"), scratch("sorted-flush"));
        let (mut each, mut all) = (index(), index());
        for round in 0..12 {
            for i in 0..3 {
                let n = (round * 7 + i * 5) % 11;
                let mut doc = json!({"n": [n, 50 - n], "c": [{"a": i.to_string()}]});
                if (round + i) % 4 != 0 {
                    doc["k"] = json!(["a", format!("k{}", (round + i) % 3)]);
                }
                for ix in [&mut each, &mut all].map(|i| i.get_mut("i").unwrap()) {
                    ix.put(&format!("{round}-{i}"), doc.clone());
                    if round > 0 && i == 0 {
                        ix.delete(&format!("{}-1", round - 1));
                    }
                }
            }
            save(&merged, &each).unwrap();
        }
        save(&flushed, &all).unwrap();
        let want = [
            SortField::SortedSet {
                field: "k".into(),
                reverse: true,
                selector: 1,
                missing: 1, // STRING_FIRST, which a reverse sort puts last
            },
            SortField::SortedNumeric {
                field: "n".into(),
                sort_type: "LONG".into(),
                reverse: false,
                selector: 0,
                missing: Some(i64::MAX),
            },
        ];
        for dir in [shard(&merged), shard(&flushed)] {
            let infos = SegmentInfos::read_latest(&dir).unwrap();
            for seg in infos.segments {
                assert_eq!(seg.info.index_sort, want);
                assert!(seg.info.has_blocks);
                let r = SegmentReader::open(&dir, seg).unwrap();
                let column = |f: &str| match r.doc_values(f).unwrap() {
                    DocValues::Numbers(n) => n,
                    _ => panic!("{f}"),
                };
                let (roots, parents, n) = (
                    column("_primary_term"),
                    column("__root_doc_for_nested"),
                    column("n"),
                );
                assert!(r.field("__root_doc_for_nested").unwrap().parent);
                let DocValues::Ordinals { ords, terms } = r.doc_values("k").unwrap() else {
                    panic!("k");
                };
                // each doc has one nested object: nested, root, nested, root, ...
                for doc in 0..r.max_doc() {
                    let root = doc % 2 == 1;
                    assert_eq!(roots[doc].is_empty(), !root);
                    assert_eq!(parents[doc], if root { vec![-1] } else { vec![] });
                }
                let keys: Vec<_> = (0..r.max_doc())
                    .filter(|d| d % 2 == 1)
                    .map(|d| {
                        let k = ords[d].iter().map(|&o| &terms[o]).max();
                        (std::cmp::Reverse(k), n[d].iter().min().copied())
                    })
                    .collect();
                assert!(keys.is_sorted(), "{keys:?}");
            }
        }
        assert!(
            SegmentInfos::read_latest(&shard(&merged))
                .unwrap()
                .segments
                .len()
                < 12
        );
        let want = index_data(&shard(&flushed));
        assert_eq!(want.len(), 25);
        assert_eq!(index_data(&shard(&merged)), want);
        assert_eq!(open(&merged).unwrap()["i"].docs().count(), 25);
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
        assert!(Arc::ptr_eq(
            ix.shards[0].docs.get("memory").unwrap(),
            snap.shards[0].docs.get("memory").unwrap()
        ));
        let segments = |ix: &Index| Arc::clone(&ix.shards[0].disk.as_ref().unwrap().segments);
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
