//! Field mappings and dynamic mapping.

use crate::{Index, parse_date_millis};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _, ser::Error as _};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

/// MVP leaf field types; objects live in [`Mappings::properties`] as dotted paths (`nested`
/// objects as a [`FieldMapping::Nested`] entry at their path, objects without subfields as a
/// [`FieldMapping::Object`] entry).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FieldMapping {
    Text {
        /// Kept as a stored field: disk saves write it for Elasticsearch, while es-mini answers
        /// `stored_fields` from `_source`.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        store: bool,
    },
    /// Dynamically mapped string: `text` with a `keyword` subfield.
    #[serde(rename = "text", skip_deserializing)]
    TextKeyword {
        fields: BTreeMap<String, FieldMapping>,
    },
    Keyword {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ignore_above: Option<u32>,
        /// Match queries on this field split their text on whitespace into terms.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        split_queries_on_whitespace: bool,
    },
    Long,
    Integer,
    Short,
    Byte,
    Double,
    Float,
    HalfFloat,
    Boolean,
    Date {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        format: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        locale: Option<String>,
        /// Multi-fields, indexed from the same source value.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        fields: BTreeMap<String, FieldMapping>,
    },
    /// Read and aggregated as epoch millis like `date`.
    // ponytail: sub-millisecond precision only lives in `_source`
    DateNanos {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        format: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        locale: Option<String>,
    },
    Ip,
    /// A `{gte, gt, lte, lt}` object of longs; aggregations count it in every bucket it overlaps.
    /// Range fields read as intervals of sortable keys (see [`FieldMapping::range_keys`]).
    LongRange,
    IntegerRange,
    FloatRange,
    DoubleRange,
    /// A `{gte, gt, lte, lt}` object of addresses, or a CIDR block.
    IpRange,
    DateRange {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        format: Option<String>,
    },
    /// Marks an object path as `nested`; its subfields are the entries under `path.`.
    // ponytail: es-mini searches nested objects as part of their root document (only segments
    // on disk hold them as separate documents, for Elasticsearch), so nested queries and
    // aggregations are unsupported; the mapping only round-trips and keeps nested objects out of hit counts
    Nested {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dynamic: Option<Dynamic>,
    },
    /// An object with no subfields or with `dynamic` set; otherwise an object with subfields is
    /// only the entries under `path.`.
    Object {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dynamic: Option<Dynamic>,
    },
}

/// What a document may do with fields its mapping lacks (ES's `ObjectMapper.Dynamic`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dynamic {
    /// Map them.
    True,
    /// Keep them in `_source` only.
    False,
    /// Refuse the document.
    Strict,
    /// Map their leaves as runtime fields, by dotted path.
    Runtime,
}

impl Serialize for Dynamic {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(match self {
            Self::True => "true",
            Self::False => "false",
            Self::Strict => "strict",
            Self::Runtime => "runtime",
        })
    }
}

impl<'de> Deserialize<'de> for Dynamic {
    /// As ES's `parseObjectOrDocumentTypeProperties`: `strict` and `runtime` in any case, or a
    /// boolean (`true`/`false` strings too); any other value fails with the reason of
    /// `XContentMapValues.nodeBooleanValue`'s innermost cause.
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        match Value::deserialize(d)? {
            Value::Bool(b) => Ok(if b { Self::True } else { Self::False }),
            Value::String(s) if s.eq_ignore_ascii_case("strict") => Ok(Self::Strict),
            Value::String(s) if s.eq_ignore_ascii_case("runtime") => Ok(Self::Runtime),
            Value::String(s) if s == "true" => Ok(Self::True),
            Value::String(s) if s == "false" => Ok(Self::False),
            v => Err(D::Error::custom(format!(
                "{BAD_DYNAMIC}{}] as only [true] or [false] are allowed.",
                crate::setting_string(&v)
            ))),
        }
    }
}

/// How the reason of an unparseable `dynamic` starts.
pub const BAD_DYNAMIC: &str = "Failed to parse value [";

/// Why a document's fields cannot be mapped.
#[derive(Debug, Clone, PartialEq)]
pub enum MapError {
    /// Elasticsearch's `document_parsing_exception` reason, without its `[line:column]` prefix.
    Parsing(String),
    /// Elasticsearch's `strict_dynamic_mapping_exception` reason.
    Strict(String),
}

impl From<&str> for MapError {
    fn from(reason: &str) -> Self {
        Self::Parsing(reason.to_owned())
    }
}

impl FieldMapping {
    /// The ES|QL-facing type name (`text`, `keyword`, `long`, `integer`, `double`, `boolean`, `date`,
    /// `date_nanos`, `ip`, or `unsupported` for ranges).
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Text { .. } | Self::TextKeyword { .. } => "text",
            Self::Keyword { .. } => "keyword",
            Self::Long => "long",
            Self::Integer | Self::Short | Self::Byte => "integer",
            Self::Double | Self::Float | Self::HalfFloat => "double",
            Self::Boolean => "boolean",
            Self::Date { .. } => "date",
            Self::DateNanos { .. } => "date_nanos",
            Self::Ip => "ip",
            Self::LongRange
            | Self::IntegerRange
            | Self::FloatRange
            | Self::DoubleRange
            | Self::IpRange
            | Self::DateRange { .. } => "unsupported",
            Self::Nested { .. } => "nested",
            Self::Object { .. } => "object",
        }
    }

    /// The mapping's `type` as Elasticsearch names it (`float`, `short`, `text`, ...).
    pub fn mapping_type(&self) -> String {
        serde_json::to_value(self).expect("mappings serialize")["type"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    }

    /// The multi-fields indexed from this field's value.
    pub fn fields(&self) -> Option<&BTreeMap<String, FieldMapping>> {
        match self {
            Self::TextKeyword { fields } | Self::Date { fields, .. } => Some(fields),
            _ => None,
        }
    }

    /// Whether this maps an object (`object` or `nested`) rather than a leaf.
    pub fn is_object(&self) -> bool {
        matches!(self, Self::Nested { .. } | Self::Object { .. })
    }

    /// The `dynamic` an object sets.
    pub fn dynamic(&self) -> Option<Dynamic> {
        match self {
            Self::Nested { dynamic } | Self::Object { dynamic } => *dynamic,
            _ => None,
        }
    }

    pub fn is_numeric(&self) -> bool {
        matches!(self.type_name(), "long" | "integer" | "double")
    }

    /// Whether this maps a range field.
    pub fn is_range(&self) -> bool {
        matches!(
            self,
            Self::LongRange
                | Self::IntegerRange
                | Self::FloatRange
                | Self::DoubleRange
                | Self::IpRange
                | Self::DateRange { .. }
        )
    }

    /// The smallest and largest keys of a range field's type (RangeType.minValue and maxValue).
    pub fn range_limits(&self) -> (u128, u128) {
        match self {
            Self::IntegerRange => (long_key(i32::MIN.into()), long_key(i32::MAX.into())),
            Self::FloatRange => {
                let key = |f: f32| long_key(sortable_int(f).into());
                (key(f32::NEG_INFINITY), key(f32::INFINITY))
            }
            Self::DoubleRange => {
                let key = |f: f64| long_key(sortable_long(f));
                (key(f64::NEG_INFINITY), key(f64::INFINITY))
            }
            Self::IpRange | Self::Ip => (0, u128::MAX),
            _ => (long_key(i64::MIN), long_key(i64::MAX)),
        }
    }

    /// Why Elasticsearch rejects `v` as a value of this integer field, if it is out of range
    /// (NumberFieldMapper parsing through AbstractXContentParser and Jackson); `token` is the
    /// JSON text of a number. `None` for values in range (fractions truncate), malformed values
    /// and other field types.
    pub fn out_of_range(&self, v: &Value, token: &str) -> Option<OutOfRange> {
        let ints = f64::from(i32::MIN)..=f64::from(i32::MAX);
        let int = match (self, v) {
            (Self::Long, Value::String(s)) => {
                return (!long_string_in_range(s)?)
                    .then(|| OutOfRange::Value(format!("Value [{s}] is out of range for a long")));
            }
            (Self::Integer | Self::Short | Self::Byte, Value::String(s)) => {
                let n = s.trim();
                let unsigned = n.trim_start_matches(['+', '-']);
                if unsigned
                    .bytes()
                    .any(|b| b.is_ascii_alphabetic() && !b"eE".contains(&b))
                    && !matches!(unsigned, "Infinity" | "NaN")
                {
                    return None;
                }
                let n: f64 = n.parse().ok()?;
                let (range, what) = match self {
                    Self::Short => (f64::from(i16::MIN)..=f64::from(i16::MAX), "a short"),
                    _ => (ints, "an integer"),
                };
                if !n.is_nan() && !range.contains(&n) {
                    let why = format!("Value [{s}] is out of range for {what}");
                    return Some(OutOfRange::Value(why));
                }
                // Java's (int) cast: toward zero, NaN to 0
                n as i64
            }
            (Self::Long | Self::Integer | Self::Short | Self::Byte, Value::Number(_)) => {
                let float = token.contains(['.', 'e', 'E']);
                let coercion = |range| {
                    Some(OutOfRange::Coercion(format!(
                        "Numeric value ({token}) out of range of {range}"
                    )))
                };
                if *self == Self::Long {
                    let two63 = 2f64.powi(63);
                    let fits = match float {
                        true => (-two63..=two63).contains(&token.parse().ok()?),
                        false => token.parse::<i64>().is_ok(),
                    };
                    return match fits {
                        true => None,
                        false => coercion("long (-9223372036854775808 - 9223372036854775807)"),
                    };
                }
                let n: f64 = token.parse().ok()?;
                if !ints.contains(&n) {
                    return coercion("int (-2147483648 - 2147483647)");
                }
                if *self == Self::Short && i16::try_from(n as i64).is_err() {
                    return coercion("Java short");
                }
                n as i64
            }
            _ => return None,
        };
        (*self == Self::Byte && i8::try_from(int).is_err())
            .then(|| OutOfRange::Value(format!("Value [{int}] is out of range for a byte")))
    }

    /// A range field's bound as a key (a `gt` or `lte` date rounds up when `round_up`):
    /// `Some(None)` if malformed (Elasticsearch rejects the document), `None` if es-mini's
    /// encoding cannot hold it (a fraction or overflow of an integer type).
    pub fn range_bound(&self, v: &Value, round_up: bool) -> Option<Option<u128>> {
        let n = match self {
            Self::IntegerRange => crate::disk::integer(v, i32::MIN.into(), i32::MAX.into())?,
            Self::LongRange => crate::disk::integer(v, i64::MIN, i64::MAX)?,
            Self::FloatRange => (crate::json_num(v).map(|f| f as f32))
                .filter(|f| f.is_finite())
                .map(|f| sortable_int(f).into()),
            Self::DoubleRange => (crate::json_num(v).filter(|f| f.is_finite())).map(sortable_long),
            Self::DateRange { format } => crate::date_millis_in(
                format.as_deref(),
                None,
                v,
                &crate::tz::TimeZone::UTC,
                round_up,
            ),
            _ => return Some(crate::ip_bytes(v).map(u128::from_be_bytes)),
        };
        Some(n.map(long_key))
    }

    /// The number a key of a numeric range field holds.
    pub fn range_number(&self, k: u128) -> f64 {
        let n = key_long(k);
        match self {
            Self::FloatRange => {
                let b = n as i32;
                f32::from_bits((b ^ ((b >> 31) & i32::MAX)) as u32).into()
            }
            Self::DoubleRange => f64::from_bits((n ^ ((n >> 63) & i64::MAX)) as u64),
            _ => n as f64,
        }
    }

    /// A range field's value as RangeFieldMapper parses it: the inclusive `[from, to]` of its
    /// keys, an open end at the type's limit. Malformed (`Some(None)`) if not an object or (for
    /// `ip_range`) a CIDR block; `None` for an unknown key or `from > to` (Elasticsearch
    /// rejects those documents too) or a bound es-mini cannot hold.
    pub fn range_keys(&self, v: &Value) -> Option<Option<(u128, u128)>> {
        if let (Self::IpRange, Value::String(s)) = (self, v) {
            return Some(cidr(s).ok());
        }
        let Some(o) = v.as_object() else {
            return Some(None);
        };
        let (mut from, mut to, mut include_from, mut include_to) = (None, None, true, true);
        for (k, x) in o {
            let (bound, include, up) = match k.as_str() {
                "gt" | "gte" => (&mut from, &mut include_from, true),
                "lt" | "lte" => (&mut to, &mut include_to, false),
                _ => return None,
            };
            *include = k.ends_with('e');
            if !x.is_null() {
                // `from` rounds up when excluded, `to` when included
                let Some(n) = self.range_bound(x, *include != up)? else {
                    return Some(None);
                };
                *bound = Some(match (*include, up) {
                    (true, _) => n,
                    (false, true) => n.checked_add(1)?,
                    (false, false) => n.checked_sub(1)?,
                });
            }
        }
        let (min, max) = self.range_limits();
        let from = from.unwrap_or(min + u128::from(!include_from));
        let to = to.unwrap_or(max - u128::from(!include_to));
        (from <= to).then_some(Some((from, to)))
    }
}

/// A long as a key that orders like it.
pub fn long_key(n: i64) -> u128 {
    u128::from(n as u64 ^ 1 << 63)
}

/// The long a key of [`long_key`] holds.
pub fn key_long(k: u128) -> i64 {
    (k as u64 ^ 1 << 63) as i64
}

/// Lucene's `NumericUtils.doubleToSortableLong`, its own inverse.
pub fn sortable_long(f: f64) -> i64 {
    let b = f.to_bits() as i64;
    b ^ ((b >> 63) & i64::MAX)
}

/// Lucene's `NumericUtils.floatToSortableInt`, its own inverse.
pub fn sortable_int(f: f32) -> i32 {
    let b = f.to_bits() as i32;
    b ^ ((b >> 31) & i32::MAX)
}

/// The first and last address of a CIDR block as keys, like InetAddresses.parseIpRangeFromCidr,
/// or its error message.
pub fn cidr(t: &str) -> Result<(u128, u128), String> {
    let Some((addr, prefix)) = t
        .split_once('/')
        .filter(|(_, p)| !p.is_empty() && !p.contains('/'))
    else {
        return Err(format!("Expected [ip/prefix] but was [{t}]"));
    };
    let k = crate::ip_bytes(&Value::from(addr))
        .map(u128::from_be_bytes)
        .ok_or_else(|| format!("'{addr}' is not an IP string literal."))?;
    // Java parses an IPv4-mapped address as IPv4
    let v4 = k >> 32 == 0xffff;
    if v4 && addr.contains(':') {
        return Err(format!(
            "CIDR notation is not allowed with IPv6-mapped IPv4 address [{addr} as it introduces ambiguity as to whether the prefix length should be interpreted as a v4 prefix length or a v6 prefix length"
        ));
    }
    let len: i32 = (prefix.parse()).map_err(|_| format!("For input string: \"{prefix}\""))?;
    let bits = if v4 { 32 } else { 128 };
    if !(0..=bits).contains(&len) {
        return Err(format!(
            "Illegal prefix length [{len}] in [{t}]. Must be 0-32 for IPv4 ranges, 0-128 for IPv6 ranges"
        ));
    }
    // an IPv4 prefix covers the 96 mapping bits too
    let host = u128::MAX
        .checked_shr((128 - bits + len) as u32)
        .unwrap_or(0);
    Ok((k & !host, k | host))
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Mappings {
    /// What documents may do with unmapped fields outside objects that set their own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dynamic: Option<Dynamic>,
    /// The `_routing` mapping section, e.g. `{"required": true}`.
    #[serde(default, rename = "_routing", skip_serializing_if = "Option::is_none")]
    pub routing: Option<Value>,
    /// The `_source` mapping section, e.g. `{"enabled": false}`.
    #[serde(default, rename = "_source", skip_serializing_if = "Option::is_none")]
    pub source: Option<Value>,
    /// Whether dynamic strings may map as dates (default true).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date_detection: Option<bool>,
    /// Whether dynamic strings that parse as numbers map as `long` or `float` (default false).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub numeric_detection: Option<bool>,
    /// Leaf fields by dotted path (`obj.field`); JSON nests them in `properties` objects.
    #[serde(
        default,
        skip_serializing_if = "BTreeMap::is_empty",
        serialize_with = "nest",
        deserialize_with = "flatten"
    )]
    pub properties: BTreeMap<String, FieldMapping>,
    /// The `runtime` section: runtime field definitions by name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub runtime: BTreeMap<String, Value>,
}

impl Mappings {
    /// The names of the mapped leaf fields, each followed by its multi-fields.
    pub fn names(&self) -> Vec<String> {
        (self.with_multi_fields())
            .filter(|(_, _, m)| !m.is_object())
            .map(|(name, ..)| name)
            .collect()
    }

    /// Each mapped field followed by its multi-fields, as (name, source path, mapping): a
    /// multi-field `path.sub` indexes the values at its parent's `path`.
    pub fn with_multi_fields(&self) -> impl Iterator<Item = (String, &String, &FieldMapping)> {
        self.properties.iter().flat_map(|(path, m)| {
            let subs = (m.fields().into_iter().flatten())
                .map(move |(sub, sm)| (format!("{path}.{sub}"), path, sm));
            std::iter::once((path.clone(), path, m)).chain(subs)
        })
    }

    /// The `dynamic` for new fields of the object at `parent` (`""` for the root): the one it
    /// or its nearest enclosing object sets, else the root's.
    fn dynamic_in(&self, mut parent: &str) -> Dynamic {
        while !parent.is_empty() {
            if let Some(d) = self.properties.get(parent).and_then(FieldMapping::dynamic) {
                return d;
            }
            parent = parent.rsplit_once('.').map_or("", |(p, _)| p);
        }
        self.dynamic.unwrap_or(Dynamic::True)
    }

    /// Whether `field` is neither mapped nor an object (runtime fields included), under a `dynamic` that does not map
    /// new fields: its values stay in `_source` only, so queries find nothing in it.
    pub fn unindexed(&self, field: &str) -> bool {
        let parent = field.rsplit_once('.').map_or("", |(p, _)| p);
        self.field(field).is_none()
            && !(self.runtime.keys()).any(|r| r == field || r.starts_with(&format!("{field}.")))
            && !maps_under(&self.properties, field)
            && self.dynamic_in(parent) != Dynamic::True
    }

    /// The mapping of `field`, including a multi-field such as `title.keyword`.
    pub fn field(&self, field: &str) -> Option<&FieldMapping> {
        self.properties.get(field).or_else(|| {
            let (base, sub) = field.rsplit_once('.')?;
            self.properties.get(base)?.fields()?.get(sub)
        })
    }

    /// Adds `new` fields, replacing those at the same paths; an empty object that gains
    /// subfields stops being an entry of its own.
    pub fn extend(&mut self, new: BTreeMap<String, FieldMapping>) {
        self.properties.extend(new);
        let props = &self.properties;
        let filled: Vec<String> = (props.iter())
            .filter(|(p, m)| **m == FieldMapping::Object { dynamic: None } && maps_under(props, p))
            .map(|(p, _)| p.clone())
            .collect();
        for p in filled {
            self.properties.remove(&p);
        }
    }

    /// `new` merged into these mappings, as PUT _mapping merges: root sections `new` sets
    /// replace these, fields restated keep the multi-fields they omit, and changing a field's
    /// type, an object to a leaf (or the reverse), or a parameter Elasticsearch fixes fails with
    /// its reason.
    // ponytail: `text` fields declared with `fields` drop them on parse (only dynamic text keeps
    // its `keyword`), so their multi-fields are neither merged nor checked
    pub fn merged(&self, new: &Mappings) -> Result<Mappings, String> {
        #[derive(PartialEq)]
        enum Kind {
            Leaf,
            Object,
            Nested,
        }
        let kind = |props: &BTreeMap<String, FieldMapping>, p: &str| match props.get(p) {
            Some(FieldMapping::Nested { .. }) => Some(Kind::Nested),
            Some(FieldMapping::Object { .. }) => Some(Kind::Object),
            Some(_) => Some(Kind::Leaf),
            None => maps_under(props, p).then_some(Kind::Object),
        };
        let mut paths = std::collections::BTreeSet::new();
        for path in new.properties.keys() {
            paths.extend(path.match_indices('.').map(|(i, _)| &path[..i]));
            paths.insert(path.as_str());
        }
        let mut merged = BTreeMap::new();
        for p in paths {
            match (kind(&self.properties, p), kind(&new.properties, p)) {
                (Some(Kind::Leaf), Some(Kind::Leaf)) => {
                    let m = merge_field(p, &self.properties[p], &new.properties[p])?;
                    merged.insert(p.to_owned(), m);
                }
                (Some(Kind::Leaf), _) | (Some(_), Some(Kind::Leaf)) => {
                    return Err(format!(
                        "can't merge a non object mapping [{p}] with an object mapping"
                    ));
                }
                (Some(a), Some(b)) if a != b => {
                    return Err(format!(
                        "can't merge a non-nested mapping [{p}] with a nested mapping"
                    ));
                }
                (_, _) => {
                    if let Some(m) = new.properties.get(p) {
                        let mut m = m.clone();
                        // an object restated without `dynamic` keeps its own
                        if let FieldMapping::Object { dynamic } | FieldMapping::Nested { dynamic } =
                            &mut m
                        {
                            *dynamic =
                                dynamic.or(self.properties.get(p).and_then(FieldMapping::dynamic));
                        }
                        merged.insert(p.to_owned(), m);
                    }
                }
            }
        }
        let mut out = self.clone();
        out.extend(merged);
        // a runtime field set to null is removed
        for (name, def) in &new.runtime {
            match def {
                Value::Null => out.runtime.remove(name),
                _ => out.runtime.insert(name.clone(), def.clone()),
            };
        }
        if new.source.is_some() {
            out.source.clone_from(&new.source);
        }
        if new.routing.is_some() {
            out.routing.clone_from(&new.routing);
        }
        out.dynamic = new.dynamic.or(self.dynamic);
        out.date_detection = new.date_detection.or(self.date_detection);
        out.numeric_detection = new.numeric_detection.or(self.numeric_detection);
        Ok(out)
    }

    /// True when the mapping sets `_routing.required`: writes and gets need a routing.
    pub fn routing_required(&self) -> bool {
        self.routing.as_ref().is_some_and(|r| r["required"] == true)
    }

    /// False when the mapping sets `_source.enabled` to `false`.
    pub fn source_enabled(&self) -> bool {
        self.source.as_ref().is_none_or(|s| s["enabled"] != false)
    }
}

impl Index {
    /// Adds dynamic mappings for fields of `source` not yet mapped, object fields under their
    /// dotted paths, as the `dynamic` of their enclosing objects allows. A document giving an
    /// object to a leaf field, or a value to an object field, maps nothing and fails with
    /// Elasticsearch's `document_parsing_exception` reason (without its `[line:column]` prefix);
    /// a new field under `dynamic: strict` fails with its `strict_dynamic_mapping_exception`
    /// reason, located in `source` serialized.
    pub fn map_dynamic(&mut self, id: &str, source: &Value) -> Result<(), MapError> {
        self.map_dynamic_in(id, source, None)
    }

    /// [`Index::map_dynamic`] locating a refused field in `raw`, the document's JSON, if given.
    pub fn map_dynamic_in(
        &mut self,
        id: &str,
        source: &Value,
        raw: Option<&[u8]>,
    ) -> Result<(), MapError> {
        let (mut added, mut runtime) = (BTreeMap::new(), BTreeMap::new());
        for (k, v) in source.as_object().into_iter().flatten() {
            self.map_value(id, k, k, v, &mut added, &mut runtime)
                .map_err(|refused| match refused {
                    Refused::Parsing(r) => MapError::Parsing(r),
                    Refused::Strict { field, key, value, at_name } => {
                        let raw = raw.map_or_else(|| source.to_string().into_bytes(), <[u8]>::to_vec);
                        let at = locate(&raw, &key, &value, at_name)
                            .map_or(String::new(), |(l, c)| format!("[{l}:{c}] "));
                        let (parent, name) = field.rsplit_once('.').unwrap_or(("_doc", &field));
                        MapError::Strict(format!(
                            "{at}mapping set to strict, dynamic introduction of [{name}] within [{parent}] is not allowed"
                        ))
                    }
                })?;
        }
        self.mappings.extend(added);
        self.mappings.runtime.extend(runtime);
        Ok(())
    }

    /// The `dynamic` that applies to the new field `field`: refused under `strict`, where the
    /// error is located at `value` under the document key path `key` (or at that key's name).
    fn admits(
        &self,
        field: &str,
        key: &str,
        value: &Value,
        at_name: bool,
    ) -> Result<Dynamic, Refused> {
        let parent = field.rsplit_once('.').map_or("", |(p, _)| p);
        match self.mappings.dynamic_in(parent) {
            Dynamic::Strict => Err(Refused::Strict {
                field: field.to_owned(),
                key: key.to_owned(),
                value: value.clone(),
                at_name,
            }),
            d => Ok(d),
        }
    }

    /// Maps `v` at `path` into `added`, or its leaves into `runtime` under `dynamic: runtime`
    /// (which maps no objects); `name` is the JSON key it came from (`null` in arrays).
    fn map_value(
        &self,
        id: &str,
        path: &str,
        name: &str,
        v: &Value,
        added: &mut BTreeMap<String, FieldMapping>,
        runtime: &mut BTreeMap<String, Value>,
    ) -> Result<(), Refused> {
        let props = &self.mappings.properties;
        let get = |p: &str| added.get(p).or_else(|| props.get(p));
        let new = |p: &str| get(p).is_none() && !self.has_subfields(p, added);
        let leaf_error = |field: &str, preview: &Value| {
            Err(Refused::Parsing(format!(
                "failed to parse field [{field}] of type [{}] in document with id '{id}'. \
                 Preview of field's value: '{}'",
                get(field).map_or_else(String::new, FieldMapping::mapping_type),
                crate::setting_string(preview)
            )))
        };
        // A dotted key (`a.b`) whose prefix is a leaf gives that leaf an object.
        for (i, _) in path.match_indices('.') {
            if get(&path[..i]).is_some_and(|m| !m.is_object()) {
                let preview = (path[i + 1..].rsplit('.')).fold(v.clone(), |v, k| json!({ k: v }));
                return leaf_error(&path[..i], &preview);
            }
            if new(&path[..i]) && self.admits(&path[..i], path, v, true)? == Dynamic::False {
                return Ok(());
            }
        }
        let as_runtime = new(path)
            && match self.admits(path, path, v, false)? {
                Dynamic::False => return Ok(()),
                d => d == Dynamic::Runtime,
            };
        let mapping = match v {
            Value::Object(o) => {
                match get(path) {
                    Some(m) if m.is_range() => return Ok(()),
                    Some(m) if !m.is_object() => return leaf_error(path, v),
                    None if o.is_empty() && !as_runtime && !self.has_subfields(path, added) => {
                        added.insert(path.to_owned(), FieldMapping::Object { dynamic: None });
                    }
                    _ => {}
                }
                for (k, x) in o {
                    self.map_value(id, &format!("{path}.{k}"), k, x, added, runtime)?;
                }
                return Ok(());
            }
            // Array elements map in order; the first scalar wins.
            Value::Array(a) => {
                for x in a {
                    self.map_value(id, path, "null", x, added, runtime)?;
                }
                return Ok(());
            }
            Value::Null => return Ok(()),
            _ if get(path).is_some_and(FieldMapping::is_object)
                || self.has_subfields(path, added) =>
            {
                return Err(Refused::Parsing(format!(
                    "object mapping for [{path}] tried to parse field [{}] as object, but found a concrete value",
                    name.rsplit('.').next().unwrap_or(name)
                )));
            }
            _ if get(path).is_some() => return Ok(()),
            // Like Elasticsearch's DynamicFieldsBuilder: numeric strings are never dates.
            Value::String(s)
                if self.mappings.numeric_detection == Some(true) && s.parse::<i64>().is_ok() =>
            {
                FieldMapping::Long
            }
            Value::String(s)
                if self.mappings.numeric_detection == Some(true) && is_java_double(s) =>
            {
                FieldMapping::Float
            }
            d @ Value::String(s)
                if self.mappings.date_detection != Some(false)
                    && !is_java_double(s)
                    && parse_date_millis(d).is_some() =>
            {
                FieldMapping::Date {
                    format: None,
                    locale: None,
                    fields: BTreeMap::new(),
                }
            }
            Value::String(_)
                if self.settings.get("index.mapping.dynamic_strings.auto_text")
                    != Some("false") =>
            {
                FieldMapping::TextKeyword {
                    fields: [(
                        "keyword".to_owned(),
                        FieldMapping::Keyword {
                            ignore_above: Some(256),
                            split_queries_on_whitespace: false,
                        },
                    )]
                    .into(),
                }
            }
            Value::String(_) => FieldMapping::Keyword {
                ignore_above: Some(256),
                split_queries_on_whitespace: false,
            },
            Value::Bool(_) => FieldMapping::Boolean,
            Value::Number(n) if n.is_f64() => FieldMapping::Float,
            Value::Number(_) => FieldMapping::Long,
        };
        if as_runtime {
            if !self.mappings.runtime.contains_key(path) {
                let t = match mapping.type_name() {
                    "text" => "keyword",
                    t => t,
                };
                runtime
                    .entry(path.to_owned())
                    .or_insert(json!({ "type": t }));
            }
            return Ok(());
        }
        added.insert(path.to_owned(), mapping);
        Ok(())
    }

    /// Whether the mappings or `added` hold a field under `path.`.
    fn has_subfields(&self, path: &str, added: &BTreeMap<String, FieldMapping>) -> bool {
        maps_under(&self.mappings.properties, path) || maps_under(added, path)
    }
}

/// Why [`Index::map_value`] cannot map a value; [`Index::map_dynamic`] turns it into a
/// [`MapError`].
enum Refused {
    Parsing(String),
    Strict {
        field: String,
        key: String,
        value: Value,
        at_name: bool,
    },
}

/// Why Elasticsearch rejects an integer field's value (see [`FieldMapping::out_of_range`]).
#[derive(Debug, PartialEq)]
pub enum OutOfRange {
    /// The reason of an `illegal_argument_exception`
    Value(String),
    /// The message of Jackson's `InputCoercionException`, before its location
    Coercion(String),
}

/// Whether a string fits a long as AbstractXContentParser.toLong reads it: a decimal truncated
/// toward zero, with BigDecimal's scale limits (`0e25` is out of range, `1e-25` is 0). `None`
/// if malformed.
fn long_string_in_range(s: &str) -> Option<bool> {
    if s.parse::<i64>().is_ok() {
        return Some(true);
    }
    let (mantissa, exp) = match s.split_once(['e', 'E']) {
        Some((m, e)) => (m, e.parse::<i64>().ok()?),
        None => (s, 0),
    };
    let unsigned = mantissa.trim_start_matches(['+', '-']);
    let negative = mantissa.starts_with('-');
    if mantissa.len() - unsigned.len() > 1 {
        return None;
    }
    let (int, frac) = unsigned.split_once('.').unwrap_or((unsigned, ""));
    let digits = format!("{int}{frac}");
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let scale = frac.len() as i64 - exp;
    if scale < -19 {
        return Some(false);
    }
    if scale > 19 {
        return Some(true);
    }
    let digits = digits.trim_start_matches('0');
    let whole = match usize::try_from(scale) {
        Ok(scale) => digits[..digits.len().saturating_sub(scale)].to_owned(),
        Err(_) => format!("{digits}{}", "0".repeat(scale.unsigned_abs() as usize)),
    };
    if whole.len() > 19 {
        return Some(false);
    }
    let n = whole.parse::<i128>().unwrap_or(0);
    Some(i64::try_from(if negative { -n } else { n }).is_ok())
}

/// Where (1-based line, and column in bytes, as Jackson counts) the first value equal to `v`
/// under the dotted key `path` starts in the JSON document `raw`, or with `at_name` the key
/// naming that value.
pub fn locate(raw: &[u8], path: &str, v: &Value, at_name: bool) -> Option<(usize, usize)> {
    locate_at(raw, path, v, at_name).map(|at| line_column(raw, at))
}

/// The 1-based line, and column in bytes, of the byte at `at` in `raw`.
pub fn line_column(raw: &[u8], at: usize) -> (usize, usize) {
    let line_start = raw[..at]
        .iter()
        .rposition(|b| *b == b'\n')
        .map_or(0, |i| i + 1);
    let line = 1 + raw[..at].iter().filter(|b| **b == b'\n').count();
    (line, at - line_start + 1)
}

/// The byte offset where [`locate`] finds the value (or key).
pub fn locate_at(raw: &[u8], path: &str, v: &Value, at_name: bool) -> Option<usize> {
    fn skip_ws(raw: &[u8], at: &mut usize) {
        while raw.get(*at).is_some_and(u8::is_ascii_whitespace) {
            *at += 1;
        }
    }
    /// One past the string starting at `start`.
    fn string_end(raw: &[u8], start: usize) -> usize {
        let mut at = start + 1;
        while at < raw.len() && raw[at] != b'"' {
            at += if raw[at] == b'\\' { 2 } else { 1 };
        }
        at + 1
    }
    /// Walks the value at `at`, found under `here`, to the end of it.
    fn walk(raw: &[u8], at: &mut usize, here: &str, path: &str, v: &Value) -> Option<usize> {
        skip_ws(raw, at);
        let start = *at;
        match *raw.get(start)? {
            open @ (b'{' | b'[') => {
                *at += 1;
                while *at < raw.len() {
                    skip_ws(raw, at);
                    if matches!(raw.get(*at), Some(b'}' | b']')) {
                        *at += 1;
                        break;
                    }
                    let child = match open {
                        b'{' => {
                            let end = string_end(raw, *at);
                            let key: String = serde_json::from_slice(raw.get(*at..end)?).ok()?;
                            *at = end;
                            skip_ws(raw, at);
                            *at += 1; // the colon
                            match here {
                                "" => key,
                                _ => format!("{here}.{key}"),
                            }
                        }
                        _ => here.to_owned(),
                    };
                    if let Some(found) = walk(raw, at, &child, path, v) {
                        return Some(found);
                    }
                    skip_ws(raw, at);
                    if raw.get(*at) == Some(&b',') {
                        *at += 1;
                    }
                }
            }
            b'"' => *at = string_end(raw, start),
            _ => {
                while raw.get(*at).is_some_and(|b| !b",]} \t\r\n".contains(b)) {
                    *at += 1;
                }
            }
        }
        let value =
            (here == path).then(|| serde_json::from_slice::<Value>(raw.get(start..*at)?).ok());
        (value.flatten().as_ref() == Some(v)).then_some(start)
    }
    let mut start = walk(raw, &mut 0, "", path, v)?;
    if at_name {
        // back over `"key" :` to the key's opening quote
        let before = |at: usize| raw[..at].iter().rposition(|b| !b.is_ascii_whitespace());
        let colon = before(start)?;
        start = before(colon)?;
        while start > 0 {
            start -= 1;
            if raw[start] == b'"'
                && raw[..start]
                    .iter()
                    .rev()
                    .take_while(|b| **b == b'\\')
                    .count()
                    % 2
                    == 0
            {
                break;
            }
        }
    }
    Some(start)
}

/// Whether `props` holds a field under `path.`.
fn maps_under(props: &BTreeMap<String, FieldMapping>, path: &str) -> bool {
    let child = format!("{path}.");
    (props.range(child.clone()..).next()).is_some_and(|(p, _)| p.starts_with(&child))
}

/// `new` merged into the field `old` at `path` (see [`Mappings::merged`]).
fn merge_field(path: &str, old: &FieldMapping, new: &FieldMapping) -> Result<FieldMapping, String> {
    use FieldMapping as M;
    let (from, to) = (old.mapping_type(), new.mapping_type());
    if from != to {
        return Err(format!(
            "mapper [{path}] cannot be changed from type [{from}] to [{to}]"
        ));
    }
    let mut conflicts = String::new();
    let mut fixed = |name: &str, a: String, b: String| {
        if a != b {
            conflicts += &format!("\n\tCannot update parameter [{name}] from [{a}] to [{b}]");
        }
    };
    let date_format = |f: &Option<String>, default: &str| f.clone().unwrap_or(default.into());
    let locale = |l: &Option<String>| l.clone().unwrap_or("en".into());
    const DATE: &str = "strict_date_optional_time||epoch_millis";
    let out = match (old, new) {
        (M::Text { store: a }, M::Text { store: b }) => {
            fixed("store", a.to_string(), b.to_string());
            new.clone()
        }
        (M::TextKeyword { .. }, M::Text { store }) => {
            fixed("store", false.to_string(), store.to_string());
            old.clone()
        }
        (
            M::Date {
                format: f1,
                locale: l1,
                fields: old_fields,
            },
            M::Date {
                format,
                locale: l2,
                fields,
            },
        ) => {
            fixed("format", date_format(f1, DATE), date_format(format, DATE));
            fixed("locale", locale(l1), locale(l2));
            let mut all = old_fields.clone();
            for (sub, m) in fields {
                let m = match old_fields.get(sub) {
                    Some(o) => merge_field(&format!("{path}.{sub}"), o, m)?,
                    None => m.clone(),
                };
                all.insert(sub.clone(), m);
            }
            M::Date {
                format: format.clone(),
                locale: l2.clone(),
                fields: all,
            }
        }
        (
            M::DateNanos {
                format: f1,
                locale: l1,
            },
            M::DateNanos {
                format: f2,
                locale: l2,
            },
        ) => {
            let nanos = "strict_date_optional_time_nanos||epoch_millis";
            fixed("format", date_format(f1, nanos), date_format(f2, nanos));
            fixed("locale", locale(l1), locale(l2));
            new.clone()
        }
        (M::DateRange { format: f1 }, M::DateRange { format: f2 }) => {
            fixed("format", date_format(f1, DATE), date_format(f2, DATE));
            new.clone()
        }
        _ => new.clone(),
    };
    match conflicts.is_empty() {
        true => Ok(out),
        false => Err(format!(
            "Mapper for [{path}] conflicts with existing mapper:{conflicts}"
        )),
    }
}

/// Whether Java's `Double.parseDouble` accepts `s`: surrounding spaces and control characters,
/// a sign, `NaN`, `Infinity`, hexadecimal floats (`0x1.8p3`) and an `f`/`d` suffix.
fn is_java_double(s: &str) -> bool {
    let s = s.trim_matches(|c| c <= ' ');
    let s = s.strip_prefix(['+', '-']).unwrap_or(s);
    if s == "NaN" || s == "Infinity" {
        return true;
    }
    let s = s.strip_suffix(['f', 'F', 'd', 'D']).unwrap_or(s);
    let digits = |s: &str, hex: bool| {
        !s.is_empty()
            && s.chars().all(|c| {
                if hex {
                    c.is_ascii_hexdigit()
                } else {
                    c.is_ascii_digit()
                }
            })
    };
    match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(h) => h.split_once(['p', 'P']).is_some_and(|(m, e)| {
            let e = e.strip_prefix(['+', '-']).unwrap_or(e);
            digits(e, false) && digits(&m.replacen('.', "", 1), true)
        }),
        None => s.starts_with(|c: char| c.is_ascii_digit() || c == '.') && s.parse::<f64>().is_ok(),
    }
}

/// Serializes dotted paths as nested `properties` objects, like Elasticsearch's object mappers.
fn nest<S: Serializer>(props: &BTreeMap<String, FieldMapping>, s: S) -> Result<S::Ok, S::Error> {
    let mut root = Map::new();
    for (path, m) in props {
        let (parents, leaf) = path
            .rsplit_once('.')
            .map_or((None, path.as_str()), |(p, l)| (Some(p), l));
        let mut node = &mut root;
        for part in parents.into_iter().flat_map(|p| p.split('.')) {
            let obj = node.entry(part).or_insert_with(|| json!({}));
            let obj = obj.as_object_mut().ok_or_else(|| S::Error::custom(path))?;
            // like ES, only an object without subfields names its type
            if obj.get("type").is_some_and(|t| t == "object") {
                obj.shift_remove("type");
            }
            node = obj
                .entry("properties")
                .or_insert_with(|| json!({}))
                .as_object_mut()
                .ok_or_else(|| S::Error::custom(path))?;
        }
        node.insert(
            leaf.to_owned(),
            serde_json::to_value(m).map_err(S::Error::custom)?,
        );
    }
    root.serialize(s)
}

/// Reads nested object mappings (no `type`, `object`, or `nested`) into dotted paths.
fn flatten<'de, D: Deserializer<'de>>(d: D) -> Result<BTreeMap<String, FieldMapping>, D::Error> {
    fn walk(
        prefix: &str,
        props: Map<String, Value>,
        out: &mut BTreeMap<String, FieldMapping>,
    ) -> serde_json::Result<()> {
        for (k, mut v) in props {
            let path = format!("{prefix}{k}");
            let nested = v.get("type").is_some_and(|t| t == "nested");
            if v.is_object() && (nested || v.get("type").is_none_or(|t| t == "object")) {
                let dynamic = match v.get_mut("dynamic").map(Value::take) {
                    Some(d) => Some(serde_json::from_value(d)?),
                    None => None,
                };
                let sub: Map<String, Value> = match v["properties"].take() {
                    Value::Null => Map::new(),
                    p => serde_json::from_value(p)?,
                };
                if nested {
                    out.insert(path.clone(), FieldMapping::Nested { dynamic });
                } else if sub.is_empty() || dynamic.is_some() {
                    out.insert(path.clone(), FieldMapping::Object { dynamic });
                }
                walk(&format!("{path}."), sub, out)?;
            } else {
                out.insert(path, serde_json::from_value(v)?);
            }
        }
        Ok(())
    }
    let mut out = BTreeMap::new();
    walk("", Map::deserialize(d)?, &mut out).map_err(D::Error::custom)?;
    Ok(out)
}
