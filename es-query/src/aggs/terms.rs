//! The `terms` bucket aggregation: each shard counts the documents of every value, keeps its
//! top `shard_size` buckets, and the reduce merges them like ES's AbstractInternalTerms.

use super::{Agg, Stats};
use crate::hll::mix64;
use crate::{QueryError, int};
use es_index::{FieldMapping, ip_bytes, ip_string, iso_date, postings::text};
use serde_json::{Map, Value};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

/// The default `value_type` of an unmapped field.
pub(super) static KEYWORD: FieldMapping = FieldMapping::Keyword { ignore_above: None };

/// A `terms` request.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Terms {
    size: usize,
    shard_size: Option<usize>,
    pub(super) min_doc_count: u64,
    shard_min_doc_count: u64,
    /// Sort keys in priority order; ties then break by ascending key.
    order: Vec<(By, bool)>,
    include: Option<Include>,
    exclude: Vec<String>,
    /// How an unmapped field's `missing` value is typed.
    pub(super) value_type: Option<FieldMapping>,
}

#[derive(Debug, Clone, PartialEq)]
enum By {
    Count,
    Key,
    /// A metric of the sub-aggregation at `index`.
    Agg {
        index: usize,
        agg: Box<Agg>,
        key: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq)]
enum Include {
    Values(Vec<String>),
    Partition { partition: i64, of: i64 },
}

impl Terms {
    pub(super) fn parse(
        name: &str,
        params: &Map<String, Value>,
        subs: &[Agg],
    ) -> Result<Terms, QueryError> {
        let mut t = Terms {
            size: 10,
            shard_size: None,
            min_doc_count: 1,
            shard_min_doc_count: 0,
            order: vec![(By::Count, false)],
            include: None,
            exclude: vec![],
            value_type: None,
        };
        for (k, v) in params {
            let bad = || QueryError::x_content(format!("[terms] failed to parse field [{k}]"));
            let positive = |what: &str| {
                let n = int(v).ok_or_else(bad)?;
                match usize::try_from(n) {
                    Ok(n) if n > 0 => Ok(n),
                    _ => Err(QueryError::illegal(format!(
                        "[{what}] must be greater than 0. Found [{n}] in [{name}]"
                    ))),
                }
            };
            let at_least_0 = |what: &str| {
                let n = int(v).ok_or_else(bad)?;
                u64::try_from(n).map_err(|_| {
                    QueryError::illegal(format!(
                        "[{what}] must be greater than or equal to 0. Found [{n}] in [{name}]"
                    ))
                })
            };
            match k.as_str() {
                "field" | "missing" => {}
                // ponytail: one collector for every hint and mode; they only change ES's speed
                "execution_hint" | "collect_mode" => {}
                "size" => t.size = positive("size")?,
                "shard_size" => t.shard_size = Some(positive("shardSize")?),
                "min_doc_count" => t.min_doc_count = at_least_0("minDocCount")?,
                "shard_min_doc_count" => t.shard_min_doc_count = at_least_0("shardMinDocCount")?,
                "order" => t.order = parse_order(v, subs)?,
                "include" => {
                    t.include = Some(match v {
                        Value::Object(o) => parse_partition(o)?,
                        v => Include::Values(parse_values(v)?),
                    });
                }
                "exclude" => t.exclude = parse_values(v)?,
                "value_type" => {
                    t.value_type = Some(match v.as_str().unwrap_or_default() {
                        "string" | "keyword" => KEYWORD.clone(),
                        "long" | "integer" | "short" | "byte" => FieldMapping::Long,
                        "double" | "float" | "number" | "numeric" => FieldMapping::Double,
                        "boolean" => FieldMapping::Boolean,
                        "date" => FieldMapping::Date {
                            format: None,
                            locale: None,
                        },
                        "ip" => FieldMapping::Ip,
                        _ => return Err(format!("Unknown value type [{}]", text(v)).into()),
                    });
                }
                _ => return Err(format!("[terms] unknown field [{k}]").into()),
            }
        }
        if matches!(t.include, Some(Include::Partition { .. })) && !t.exclude.is_empty() {
            return Err(QueryError::illegal(
                "Cannot specify any excludes when using a partition-based include",
            ));
        }
        Ok(t)
    }

    fn key_order(&self) -> bool {
        self.order[0].0 == By::Key
    }

    fn count_desc(&self) -> bool {
        matches!(self.order[0], (By::Count, false))
    }

    /// ES's BucketCountThresholds after `adjustBucketCountThresholds` and `ensureValidity`.
    fn shard_size(&self) -> usize {
        let default = match self.key_order() {
            true => self.size,
            false => ((self.size as f64 * 1.5 + 10.0) as usize).min(i32::MAX as usize),
        };
        self.shard_size.unwrap_or(default).max(self.size)
    }

    fn cmp(&self, a: &(Key, Bucket), b: &(Key, Bucket)) -> Ordering {
        let by = |(by, asc): &(By, bool)| {
            let o = match by {
                By::Count => a.1.count.cmp(&b.1.count),
                By::Key => a.0.cmp(&b.0),
                // ponytail: NaN sorts as Java's Double.compare (last ascending, first
                // descending); ES's shards instead always put NaN last
                By::Agg { index, agg, key } => {
                    let v = |b: &Bucket| {
                        super::sort_value(agg, key.as_deref(), &b.subs[*index]).unwrap_or(f64::NAN)
                    };
                    v(&a.1).total_cmp(&v(&b.1))
                }
            };
            if *asc { o } else { o.reverse() }
        };
        (self.order.iter().map(by))
            .find(|o| o.is_ne())
            .unwrap_or_else(|| a.0.cmp(&b.0))
    }

    /// A shard's buckets of a field with `format`; `parse` reads an include/exclude value as
    /// one of the field's doc values; `empty` is a new bucket's sub-aggregation results.
    pub(super) fn buckets(
        &self,
        format: Format,
        parse: impl Fn(&str) -> Option<Value>,
        empty: Vec<Stats>,
    ) -> Buckets {
        let keys = |vals: &[String]| -> BTreeSet<Key> {
            vals.iter()
                .filter_map(|s| Key::of(&parse(s)?, format))
                .collect()
        };
        Buckets {
            format,
            include: match &self.include {
                None => None,
                Some(Include::Values(v)) => Some(Filter::Keys(keys(v))),
                Some(Include::Partition { partition, of }) => Some(Filter::Partition {
                    partition: *partition,
                    of: *of,
                }),
            },
            exclude: keys(&self.exclude),
            empty,
            ..Buckets::default()
        }
    }

    /// Whether a shard must add zero-count buckets for its unmatched values.
    pub(super) fn wants_zero_buckets(&self, b: &Buckets) -> bool {
        self.min_doc_count == 0 && !(self.count_desc() && b.counts.len() >= self.size)
    }

    /// Keeps a shard's top `shard_size` buckets, like ES's terms aggregators' `buildAggregations`.
    pub(super) fn finish_shard(&self, b: &mut Buckets) {
        let shard_size = self.shard_size();
        let min = self.shard_min_doc_count.min(self.min_doc_count);
        let mut v: Vec<(Key, Bucket)> = std::mem::take(&mut b.counts).into_iter().collect();
        let total: u64 = v.iter().map(|b| b.1.count).sum();
        v.retain(|b| b.1.count >= min);
        v.sort_by(|a, b| self.cmp(a, b));
        v.truncate(shard_size);
        b.other += total - v.iter().map(|b| b.1.count).sum::<u64>();
        // AbstractInternalTerms.getDocCountError
        b.error = if v.len() < shard_size || self.key_order() {
            0
        } else if self.count_desc() {
            v.iter().map(|b| b.1.count).min().unwrap_or(0) as i64
        } else {
            -1
        };
        b.shards = 1;
        b.counts = v.into_iter().collect();
    }

    /// The rendered result and its `typed_keys` type; `sub` renders a bucket's
    /// sub-aggregations into it.
    pub(super) fn render(
        &self,
        b: &mut Buckets,
        body: &mut Map<String, Value>,
        mut sub: impl FnMut(&mut [Stats], &mut Map<String, Value>),
    ) -> &'static str {
        let mut v: Vec<(Key, Bucket)> = (std::mem::take(&mut b.counts).into_iter())
            .filter(|b| b.1.count >= self.min_doc_count)
            .collect();
        v.sort_by(|a, b| self.cmp(a, b));
        let rest = v.split_off(self.size.min(v.len()));
        // a single shard's counts are exact
        let error = if b.shards > 1 { b.error } else { 0 };
        body.insert("doc_count_error_upper_bound".into(), error.into());
        let other = b.other + rest.iter().map(|b| b.1.count).sum::<u64>();
        body.insert("sum_other_doc_count".into(), other.into());
        let buckets = v.into_iter().map(|(k, mut bucket)| {
            let mut out = Map::new();
            let key = match (&k, b.format) {
                (Key::Str(s), _) => Value::from(s.as_str()),
                (Key::Ip(ip), _) => Value::from(ip_string(*ip)),
                (k, Format::Double) => Value::from(k.f64()),
                (Key::Long(x), _) => Value::from(*x),
                (Key::Double(x), _) => Value::from(*x),
            };
            out.insert("key".into(), key);
            match (&k, b.format) {
                (Key::Long(x), Format::Bool) => {
                    out.insert("key_as_string".into(), (*x != 0).to_string().into());
                }
                (Key::Long(x), Format::Date) => {
                    out.insert("key_as_string".into(), iso_date(*x as u64).into());
                }
                _ => {}
            }
            out.insert("doc_count".into(), bucket.count.into());
            sub(&mut bucket.subs, &mut out);
            Value::Object(out)
        });
        body.insert("buckets".into(), buckets.collect());
        match b.format {
            Format::Unmapped | Format::Str | Format::Ip => "sterms",
            Format::Double => "dterms",
            Format::Long | Format::Bool | Format::Date => "lterms",
        }
    }
}

fn parse_order(v: &Value, subs: &[Agg]) -> Result<Vec<(By, bool)>, QueryError> {
    let objects = match v {
        Value::Array(a) => a.iter().collect(),
        v => vec![v],
    };
    let mut order = vec![];
    for o in objects {
        let o = o
            .as_object()
            .ok_or("Expected [START_OBJECT] under [order]")?;
        let (key, dir) = o
            .iter()
            .next_back()
            .ok_or("Must specify at least one field for [order]")?;
        let asc = match dir.as_str().map(str::to_ascii_lowercase).as_deref() {
            Some("asc") => true,
            Some("desc") => false,
            Some(_) => return Err(format!("Unknown order direction [{}]", text(dir)).into()),
            None => return Err(format!("Unexpected token [{dir}] for [order]").into()),
        };
        let by = match key.as_str() {
            "_count" => By::Count,
            "_key" => By::Key,
            // ponytail: paths name a direct sub-aggregation (`agg` or `agg.metric`); no
            // single-bucket aggregations exist to path through with `>`
            path => {
                let (agg, key) = match path.rsplit_once('.') {
                    Some((a, k)) if !path.contains(['>', '[']) => (a, Some(k.to_owned())),
                    _ => (path.split(['>', '.', '[']).next().unwrap_or(path), None),
                };
                let invalid = |e: String| {
                    QueryError::illegal(format!("Invalid aggregation order path [{path}]. {e}"))
                };
                let index = (subs.iter().position(|s| s.name == agg))
                    .filter(|_| !path.contains(['>', '[']))
                    .ok_or_else(|| invalid(format!("The provided aggregation [{agg}] either does not exist, or is a pipeline aggregation and cannot be used to sort the buckets.")))?;
                let agg = Box::new(subs[index].clone());
                super::sort_value(&agg, key.as_deref(), &Stats::default()).map_err(invalid)?;
                By::Agg { index, agg, key }
            }
        };
        order.push((by, asc));
    }
    match order.is_empty() {
        true => Err("Must specify at least one field for [order]".into()),
        false => Ok(order),
    }
}

fn parse_values(v: &Value) -> Result<Vec<String>, QueryError> {
    match v {
        // ponytail: no Lucene regexp engine yet; add one when a test needs regex include/exclude
        Value::String(_) => Err(QueryError::illegal(
            "regular expressions in [include] and [exclude] are not supported",
        )),
        Value::Array(a) => a
            .iter()
            .map(|v| match v {
                Value::Array(_) | Value::Object(_) | Value::Null => {
                    Err("Array elements in include/exclude clauses should be string values".into())
                }
                v => Ok(text(v)),
            })
            .collect(),
        _ => Err(QueryError::illegal(
            "Unrecognized token for an include/exclude",
        )),
    }
}

fn parse_partition(o: &Map<String, Value>) -> Result<Include, QueryError> {
    let (mut partition, mut of) = (None, None);
    for (k, v) in o {
        let n = int(v).ok_or_else(|| format!("[{k}] must be an integer"))?;
        match k.as_str() {
            "partition" => partition = Some(n),
            "num_partitions" => of = Some(n),
            _ => {
                return Err(format!("Unknown parameter in Include/Exclude clause: {k}").into());
            }
        }
    }
    let missing = |p: &str| {
        QueryError::illegal(format!(
            "Missing [{p}] parameter for partition-based include"
        ))
    };
    let partition = partition.ok_or_else(|| missing("partition"))?;
    let of = of.ok_or_else(|| missing("num_partitions"))?;
    if of < 1 || !(0..of).contains(&partition) {
        return Err(QueryError::illegal(format!(
            "Partition must be >=0 and < numPartition which is {of}"
        )));
    }
    Ok(Include::Partition { partition, of })
}

/// How a field's terms render.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub(super) enum Format {
    /// An unmapped field without `missing`: no buckets.
    #[default]
    Unmapped,
    Str,
    Long,
    Double,
    Bool,
    Date,
    Ip,
}

impl Format {
    pub(super) fn of(m: Option<&FieldMapping>) -> Format {
        use FieldMapping as M;
        match m {
            None => Format::Unmapped,
            Some(M::Long | M::Integer | M::Short | M::Byte) => Format::Long,
            Some(M::Double | M::Float) => Format::Double,
            Some(M::Boolean) => Format::Bool,
            Some(M::Date { .. }) => Format::Date,
            Some(M::Ip) => Format::Ip,
            Some(M::Keyword { .. } | M::Text | M::TextKeyword { .. }) => Format::Str,
        }
    }
}

/// A bucket key: a keyword's term, an ip's 16 bytes, or a numeric doc value (booleans and
/// dates are longs).
#[derive(Debug, Clone)]
pub(super) enum Key {
    Long(i64),
    Double(f64),
    Str(String),
    Ip([u8; 16]),
}

impl Key {
    fn of(v: &Value, format: Format) -> Option<Key> {
        match v {
            Value::String(_) if format == Format::Ip => ip_bytes(v).map(Key::Ip),
            Value::Bool(b) => Some(Key::Long(i64::from(*b))),
            Value::String(s) => Some(Key::Str(s.clone())),
            Value::Number(n) => (n.as_i64().map(Key::Long)).or_else(|| n.as_f64().map(Key::Double)),
            _ => None,
        }
    }

    fn f64(&self) -> f64 {
        match self {
            Key::Long(x) => *x as f64,
            Key::Double(x) => *x,
            Key::Str(_) | Key::Ip(_) => f64::NAN,
        }
    }
}

/// Longs and doubles compare as numbers, so a long and a double key merge like ES promotes
/// long terms to double terms; strings compare as bytes.
impl Ord for Key {
    fn cmp(&self, o: &Self) -> Ordering {
        match (self, o) {
            (Key::Long(a), Key::Long(b)) => a.cmp(b),
            (Key::Str(a), Key::Str(b)) => a.cmp(b),
            (Key::Str(_), _) => Ordering::Greater,
            (_, Key::Str(_)) => Ordering::Less,
            (Key::Ip(a), Key::Ip(b)) => a.cmp(b),
            (Key::Ip(_), _) => Ordering::Greater,
            (_, Key::Ip(_)) => Ordering::Less,
            _ => self.f64().total_cmp(&o.f64()),
        }
    }
}

impl PartialOrd for Key {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}

impl PartialEq for Key {
    fn eq(&self, o: &Self) -> bool {
        self.cmp(o).is_eq()
    }
}

impl Eq for Key {}

#[derive(Debug, Clone)]
enum Filter {
    Keys(BTreeSet<Key>),
    Partition { partition: i64, of: i64 },
}

/// Lucene's `StringHelper.murmurhash3_x86_32`.
fn murmur3_32(data: &[u8], seed: u32) -> u32 {
    let mix = |k: u32| {
        k.wrapping_mul(0xcc9e_2d51)
            .rotate_left(15)
            .wrapping_mul(0x1b87_3593)
    };
    let mut h = seed;
    let (chunks, tail) = data.as_chunks::<4>();
    for c in chunks {
        h ^= mix(u32::from_le_bytes(*c));
        h = h.rotate_left(13).wrapping_mul(5).wrapping_add(0xe654_6b64);
    }
    if !tail.is_empty() {
        let k = (tail.iter().enumerate()).fold(0, |k, (i, b)| k | u32::from(*b) << (8 * i));
        h ^= mix(k);
    }
    h ^= data.len() as u32;
    h ^= h >> 16;
    h = h.wrapping_mul(0x85eb_ca6b);
    h ^= h >> 13;
    h = h.wrapping_mul(0xc2b2_ae35);
    h ^ (h >> 16)
}

impl Filter {
    /// ES's IncludeExclude filters; partitions hash like its Partitioned*Filter.
    fn accepts(&self, k: &Key) -> bool {
        match self {
            Filter::Keys(keys) => keys.contains(k),
            &Filter::Partition { partition, of } => {
                let hash = match k {
                    Key::Str(s) => i64::from(murmur3_32(s.as_bytes(), 31) as i32),
                    Key::Ip(ip) => i64::from(murmur3_32(ip, 31) as i32),
                    Key::Long(x) => mix64(*x as u64) as i64,
                    // NumericUtils.doubleToSortableLong
                    Key::Double(x) => {
                        let bits = x.to_bits() as i64;
                        mix64((bits ^ ((bits >> 63) & i64::MAX)) as u64) as i64
                    }
                };
                hash.rem_euclid(of) == partition
            }
        }
    }
}

/// One bucket: its document count and its sub-aggregations' results.
#[derive(Debug, Clone, Default)]
pub(super) struct Bucket {
    count: u64,
    pub(super) subs: Vec<Stats>,
}

/// One shard's buckets, or several shards' merged.
#[derive(Debug, Clone, Default)]
pub(super) struct Buckets {
    pub(super) counts: BTreeMap<Key, Bucket>,
    /// Documents counted in buckets left out.
    other: u64,
    /// The upper bound of counts missed by shards' truncation; -1 when unbounded.
    error: i64,
    shards: u32,
    format: Format,
    include: Option<Filter>,
    exclude: BTreeSet<Key>,
    /// A new bucket's sub-aggregation results.
    empty: Vec<Stats>,
}

impl Buckets {
    fn keys(&self, vals: Vec<Value>) -> BTreeSet<Key> {
        (vals.iter().filter_map(|v| Key::of(v, self.format)))
            .filter(|k| self.include.as_ref().is_none_or(|f| f.accepts(k)))
            .filter(|k| !self.exclude.contains(k))
            .collect()
    }

    fn bucket(&mut self, k: Key) -> &mut Bucket {
        let empty = &self.empty;
        (self.counts.entry(k)).or_insert_with(|| Bucket {
            count: 0,
            subs: empty.clone(),
        })
    }

    /// Counts one document with doc values `vals`; `sub` collects it into each of its buckets'
    /// sub-aggregations.
    pub(super) fn add(&mut self, vals: Vec<Value>, mut sub: impl FnMut(&mut Vec<Stats>)) {
        for k in self.keys(vals) {
            let b = self.bucket(k);
            b.count += 1;
            sub(&mut b.subs);
        }
    }

    /// Adds empty buckets for the values of a document the query did not match.
    pub(super) fn add_zero(&mut self, vals: Vec<Value>) {
        for k in self.keys(vals) {
            self.bucket(k);
        }
    }

    pub(super) fn merge(&mut self, o: Buckets) {
        for (k, b) in o.counts {
            match self.counts.get_mut(&k) {
                Some(mine) => {
                    mine.count += b.count;
                    super::reduce(&mut mine.subs, b.subs);
                }
                None => drop(self.counts.insert(k, b)),
            }
        }
        self.other += o.other;
        self.error = match (self.error, o.error) {
            (-1, _) | (_, -1) => -1,
            (a, b) => a + b,
        };
        self.shards += o.shards;
        self.format = match (self.format, o.format) {
            (Format::Unmapped, f) | (f, Format::Unmapped) => f,
            (Format::Long, Format::Double) => Format::Double,
            (f, _) => f,
        };
    }
}

#[cfg(test)]
mod tests {
    use crate::{SearchRequest, search_many};
    use es_index::{FieldMapping, Index};
    use serde_json::{Value, json};

    #[test]
    fn murmur3_32_matches_reference_vectors() {
        assert_eq!(super::murmur3_32(b"", 0), 0);
        assert_eq!(super::murmur3_32(b"hello", 0), 0x248b_fa47);
        let fox = b"The quick brown fox jumps over the lazy dog";
        assert_eq!(super::murmur3_32(fox, 0), 0x2e4f_f723);
    }

    fn index() -> Index {
        let mut ix = Index::default();
        let props = &mut ix.mappings.properties;
        props.insert("k".into(), FieldMapping::Keyword { ignore_above: None });
        props.insert("n".into(), FieldMapping::Long);
        props.insert("d".into(), FieldMapping::Double);
        props.insert("b".into(), FieldMapping::Boolean);
        let date = FieldMapping::Date {
            format: None,
            locale: None,
        };
        props.insert("at".into(), date);
        props.insert("ip".into(), FieldMapping::Ip);
        ix.put(
            "1",
            json!({"k": "a", "n": 1, "d": 1.5, "b": true, "at": "2016-05-03", "ip": "::1"}),
        );
        ix.put(
            "2",
            json!({"k": "b", "n": 2, "d": 1.5, "b": false, "at": "2014-09-01", "ip": "127.0.0.1"}),
        );
        ix.put(
            "3",
            json!({"k": ["b", "b"], "n": [2, 2], "d": 2.5, "b": true, "at": "2016-05-03"}),
        );
        ix.put("4", json!({"k": "c", "n": 3, "b": true, "ip": "0::1"}));
        ix.put("5", json!({}));
        ix
    }

    fn run(targets: &[&Index], body: Value) -> Result<Value, String> {
        let req = SearchRequest::parse(&body).map_err(|e| e.msg)?;
        let targets: Vec<_> = targets.iter().map(|ix| ("i", *ix, None)).collect();
        let res = search_many(&targets, &req).map_err(|e| e.msg)?;
        Ok(res.aggregations.unwrap())
    }

    /// The `t` terms aggregation's result with `params` over `index()`, optionally queried.
    fn terms(params: Value) -> Value {
        terms_q(params, json!({"match_all": {}}))
    }

    fn terms_q(params: Value, query: Value) -> Value {
        let body = json!({"size": 0, "query": query, "aggs": {"t": {"terms": params}}});
        run(&[&index()], body).unwrap()["t"].take()
    }

    fn buckets(params: Value) -> Value {
        terms(params)["buckets"].take()
    }

    #[test]
    fn terms_count_each_value_once_per_document() {
        assert_eq!(
            terms(json!({"field": "k"})),
            json!({"doc_count_error_upper_bound": 0, "sum_other_doc_count": 0, "buckets": [
                {"key": "b", "doc_count": 2},
                {"key": "a", "doc_count": 1},
                {"key": "c", "doc_count": 1},
            ]})
        );
        assert_eq!(
            buckets(json!({"field": "n"})),
            json!([
                {"key": 2, "doc_count": 2},
                {"key": 1, "doc_count": 1},
                {"key": 3, "doc_count": 1},
            ])
        );
        assert_eq!(
            buckets(json!({"field": "d"})),
            json!([{"key": 1.5, "doc_count": 2}, {"key": 2.5, "doc_count": 1}])
        );
    }

    #[test]
    fn terms_on_an_opened_index_read_doc_values_like_memory() {
        let dir = std::env::temp_dir().join(format!("esm-terms-{}", std::process::id()));
        let indices = es_index::Indices::from([("i".to_owned(), index())]);
        es_index::disk::save(&dir, &indices).unwrap();
        let opened = es_index::disk::open(&dir).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        for field in ["k", "n", "d", "b", "at", "ip"] {
            let body = json!({"aggs": {"t": {"terms": {"field": field, "min_doc_count": 0}}}});
            let want = run(&[&index()], body.clone()).unwrap();
            assert_eq!(run(&[&opened["i"]], body).unwrap(), want, "{field}");
        }
    }

    #[test]
    fn terms_on_booleans_and_dates_have_key_as_string() {
        assert_eq!(
            buckets(json!({"field": "b"})),
            json!([
                {"key": 1, "key_as_string": "true", "doc_count": 3},
                {"key": 0, "key_as_string": "false", "doc_count": 1},
            ])
        );
        assert_eq!(
            buckets(json!({"field": "at"})),
            json!([
                {"key": 1_462_233_600_000_i64, "key_as_string": "2016-05-03T00:00:00.000Z", "doc_count": 2},
                {"key": 1_409_529_600_000_i64, "key_as_string": "2014-09-01T00:00:00.000Z", "doc_count": 1},
            ])
        );
    }

    #[test]
    fn terms_on_ips_key_canonical_addresses_in_byte_order() {
        assert_eq!(
            buckets(json!({"field": "ip", "order": {"_key": "desc"}})),
            json!([{"key": "127.0.0.1", "doc_count": 1}, {"key": "::1", "doc_count": 2}])
        );
        assert_eq!(
            buckets(json!({"field": "ip", "include": ["0:0::1"]})),
            json!([{"key": "::1", "doc_count": 2}])
        );
    }

    #[test]
    fn terms_size_and_order() {
        let t = terms(json!({"field": "k", "size": 1}));
        assert_eq!(t["sum_other_doc_count"], 2);
        assert_eq!(t["buckets"], json!([{"key": "b", "doc_count": 2}]));
        let keys = |params: Value| -> Vec<Value> {
            let b = buckets(params);
            b.as_array()
                .unwrap()
                .iter()
                .map(|b| b["key"].clone())
                .collect()
        };
        assert_eq!(
            keys(json!({"field": "k", "order": {"_key": "desc"}})),
            ["c", "b", "a"]
        );
        // ties break by ascending key
        assert_eq!(
            keys(json!({"field": "k", "order": [{"_count": "asc"}]})),
            ["a", "c", "b"]
        );
        assert_eq!(
            keys(json!({"field": "n", "order": {"_key": "asc"}, "size": 2})),
            [1, 2]
        );
    }

    #[test]
    fn terms_min_doc_count() {
        // buckets under min_doc_count are not counted as other docs
        let t = terms(json!({"field": "k", "min_doc_count": 2}));
        assert_eq!(t["sum_other_doc_count"], 0);
        assert_eq!(t["buckets"], json!([{"key": "b", "doc_count": 2}]));
        // 0 adds the shard's unmatched values
        let t = terms_q(
            json!({"field": "k", "min_doc_count": 0}),
            json!({"term": {"k": "b"}}),
        );
        assert_eq!(
            t["buckets"],
            json!([
                {"key": "b", "doc_count": 2},
                {"key": "a", "doc_count": 0},
                {"key": "c", "doc_count": 0},
            ])
        );
    }

    #[test]
    fn terms_include_exclude_and_missing() {
        assert_eq!(
            buckets(json!({"field": "k", "include": ["a", "b"], "exclude": ["b"]})),
            json!([{"key": "a", "doc_count": 1}])
        );
        assert_eq!(
            buckets(json!({"field": "at", "exclude": ["2016-05-03"]}))[0]["key_as_string"],
            "2014-09-01T00:00:00.000Z"
        );
        assert_eq!(
            buckets(json!({"field": "n", "include": ["3"]})),
            json!([{"key": 3, "doc_count": 1}])
        );
        assert_eq!(
            buckets(json!({"field": "n", "missing": 7}))[3],
            json!({"key": 7, "doc_count": 1})
        );
    }

    #[test]
    fn terms_partitions_hash_like_elasticsearch() {
        // from Elasticsearch's terms.yml "Partitioned string/integer test"
        let mut ix = Index::default();
        let props = &mut ix.mappings.properties;
        props.insert("k".into(), FieldMapping::Keyword { ignore_above: None });
        props.insert("n".into(), FieldMapping::Long);
        ix.put("1", json!({"k": "abc", "n": 1234}));
        ix.put("2", json!({"k": "bcd", "n": 5678}));
        let keys = |field: &str, partition: u32| {
            let include = json!({"partition": partition, "num_partitions": 2});
            let body = json!({"aggs": {"t": {"terms": {"field": field, "include": include}}}});
            let b = run(&[&ix], body).unwrap()["t"]["buckets"].take();
            b.as_array()
                .unwrap()
                .iter()
                .map(|b| b["key"].clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(keys("k", 0), ["bcd"]);
        assert_eq!(keys("k", 1), ["abc"]);
        assert_eq!(keys("n", 0), [5678]);
        assert_eq!(keys("n", 1), [1234]);
    }

    #[test]
    fn terms_on_unmapped_fields() {
        let date = json!({"field": "nope", "value_type": "date", "missing": "2016-05-11"});
        assert_eq!(
            buckets(date),
            json!([{"key": 1_462_924_800_000_i64, "key_as_string": "2016-05-11T00:00:00.000Z", "doc_count": 5}])
        );
        assert_eq!(
            buckets(json!({"field": "nope", "value_type": "long", "missing": 3})),
            json!([{"key": 3, "doc_count": 5}])
        );
        assert_eq!(
            buckets(json!({"field": "nope", "missing": "abc"})),
            json!([{"key": "abc", "doc_count": 5}])
        );
        assert_eq!(
            terms(json!({"field": "nope"})),
            json!({"doc_count_error_upper_bound": 0, "sum_other_doc_count": 0, "buckets": []})
        );
    }

    #[test]
    fn terms_reduce_across_indices() {
        // longs and doubles merge as doubles (terms.yml "Mixing longs and doubles")
        let (mut longs, mut doubles) = (Index::default(), Index::default());
        longs
            .mappings
            .properties
            .insert("n".into(), FieldMapping::Long);
        doubles
            .mappings
            .properties
            .insert("n".into(), FieldMapping::Double);
        longs.put("1", json!({"n": 100}));
        longs.put("2", json!({"n": 10}));
        doubles.put("1", json!({"n": 100.0}));
        doubles.put("2", json!({"n": 10.0}));
        doubles.put("3", json!({"n": 14.6}));
        let body = json!({"aggs": {"t": {"terms": {"field": "n"}}}});
        let got = run(&[&longs, &doubles], body).unwrap();
        assert_eq!(
            got["t"]["buckets"],
            json!([
                {"key": 10.0, "doc_count": 2},
                {"key": 100.0, "doc_count": 2},
                {"key": 14.6, "doc_count": 1},
            ])
        );
        // each shard returns its top shard_size terms; the error is their last counts' sum
        let (mut one, mut two) = (Index::default(), Index::default());
        for (ix, ks) in [(&mut one, ["a", "a", "b"]), (&mut two, ["b", "b", "a"])] {
            let kw = FieldMapping::Keyword { ignore_above: None };
            ix.mappings.properties.insert("k".into(), kw);
            for (i, k) in ks.iter().enumerate() {
                ix.put(&i.to_string(), json!({ "k": k }));
            }
        }
        let body = json!({"aggs": {"t": {"terms": {"field": "k", "size": 1, "shard_size": 1}}}});
        assert_eq!(
            run(&[&one, &two], body).unwrap()["t"],
            json!({"doc_count_error_upper_bound": 4, "sum_other_doc_count": 4, "buckets": [
                {"key": "a", "doc_count": 2},
            ]})
        );
    }

    #[test]
    fn terms_buckets_hold_sub_aggregations() {
        let body = json!({"query": {"term": {"k": "b"}}, "aggs": {"t": {
            "terms": {"field": "k", "min_doc_count": 0, "size": 2},
            "aggs": {"m": {"max": {"field": "n"}}, "in": {"terms": {"field": "b"}}},
        }}});
        let mut req = SearchRequest::parse(&body).unwrap();
        req.typed_keys = true;
        let ix = index();
        let got = search_many(&[("i", &ix, None)], &req).unwrap();
        let no_in =
            json!({"doc_count_error_upper_bound": 0, "sum_other_doc_count": 0, "buckets": []});
        assert_eq!(
            got.aggregations.unwrap()["sterms#t"]["buckets"],
            json!([
                {"key": "b", "doc_count": 2, "max#m": {"value": 2.0}, "lterms#in": {
                    "doc_count_error_upper_bound": 0, "sum_other_doc_count": 0, "buckets": [
                        {"key": 0, "key_as_string": "false", "doc_count": 1},
                        {"key": 1, "key_as_string": "true", "doc_count": 1},
                    ]}},
                {"key": "a", "doc_count": 0, "max#m": {"value": null}, "lterms#in": no_in},
            ])
        );
    }

    #[test]
    fn terms_order_by_sub_aggregation() {
        let keys = |order: Value, subs: Value| {
            let body =
                json!({"aggs": {"t": {"terms": {"field": "k", "order": order}, "aggs": subs}}});
            let b = run(&[&index()], body).unwrap()["t"]["buckets"].take();
            (b.as_array().unwrap().iter())
                .map(|b| b["key"].clone())
                .collect::<Vec<_>>()
        };
        let max = json!({"m": {"max": {"field": "d"}}});
        // c has no d: an empty max is -Infinity
        assert_eq!(keys(json!({"m": "desc"}), max.clone()), ["b", "a", "c"]);
        assert_eq!(keys(json!({"m.value": "asc"}), max), ["c", "a", "b"]);
        let stats = json!({"s": {"stats": {"field": "n"}}});
        assert_eq!(keys(json!([{"s.sum": "desc"}]), stats), ["b", "c", "a"]);
        let body = json!({"aggs": {"t": {"terms": {"field": "k", "order": {"m.x": "asc"}},
            "aggs": {"m": {"max": {"field": "n"}}}}}});
        assert_eq!(
            run(&[&index()], body).unwrap_err(),
            "Invalid aggregation order path [m.x]. Ordering on a single-value metrics aggregation can only be done on its value. Either drop the key (a la \"m\") or change it to \"value\" (a la \"m.value\")"
        );
    }

    #[test]
    fn terms_typed_keys() {
        let body = json!({"aggs": {
            "s": {"terms": {"field": "k"}},
            "l": {"terms": {"field": "b"}},
            "d": {"terms": {"field": "d"}},
            "ip": {"terms": {"field": "ip"}},
        }});
        let mut req = SearchRequest::parse(&body).unwrap();
        req.typed_keys = true;
        let ix = index();
        let got = search_many(&[("i", &ix, None)], &req).unwrap();
        let mut names: Vec<_> = got
            .aggregations
            .unwrap()
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        names.sort();
        assert_eq!(names, ["dterms#d", "lterms#l", "sterms#ip", "sterms#s"]);
    }

    #[test]
    fn terms_reject_bad_requests() {
        let err = |params: Value| {
            run(&[&index()], json!({"aggs": {"t": {"terms": params}}})).unwrap_err()
        };
        assert_eq!(
            err(json!({"size": 1})),
            "Required one of fields [field, script], but none were specified."
        );
        assert_eq!(
            err(json!({"field": "k", "size": 0})),
            "[size] must be greater than 0. Found [0] in [t]"
        );
        assert_eq!(
            err(json!({"field": "k", "shard_size": 0})),
            "[shardSize] must be greater than 0. Found [0] in [t]"
        );
        assert_eq!(
            err(json!({"field": "k", "min_doc_count": -1})),
            "[minDocCount] must be greater than or equal to 0. Found [-1] in [t]"
        );
        assert_eq!(
            err(json!({"field": "k", "order": {"_key": "up"}})),
            "Unknown order direction [up]"
        );
        assert_eq!(
            err(json!({"field": "k", "order": {"x": "desc"}})),
            "Invalid aggregation order path [x]. The provided aggregation [x] either does not exist, or is a pipeline aggregation and cannot be used to sort the buckets."
        );
        assert_eq!(
            err(
                json!({"field": "k", "include": {"partition": 0, "num_partitions": 2}, "exclude": ["a"]})
            ),
            "Cannot specify any excludes when using a partition-based include"
        );
    }
}
