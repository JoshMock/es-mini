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
    // ponytail: only the range types the YAML tests map; add integer/float/double/ip_range alike
    LongRange,
    DateRange {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        format: Option<String>,
    },
    /// Marks an object path as `nested`; its subfields are the entries under `path.`.
    // ponytail: es-mini searches nested objects as part of their root document (only segments
    // on disk hold them as separate documents, for Elasticsearch), so nested queries and
    // aggregations are unsupported; the mapping only round-trips and keeps nested objects out of hit counts
    Nested,
    /// An object with no subfields; one with subfields is only the entries under `path.`.
    Object,
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
            Self::LongRange | Self::DateRange { .. } => "unsupported",
            Self::Nested => "nested",
            Self::Object => "object",
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
        matches!(self, Self::Nested | Self::Object)
    }

    pub fn is_numeric(&self) -> bool {
        matches!(self.type_name(), "long" | "integer" | "double")
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Mappings {
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
}

impl Mappings {
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
            .filter(|(p, m)| **m == FieldMapping::Object && maps_under(props, p))
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
            Some(FieldMapping::Nested) => Some(Kind::Nested),
            Some(FieldMapping::Object) => Some(Kind::Object),
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
                        merged.insert(p.to_owned(), m.clone());
                    }
                }
            }
        }
        let mut out = self.clone();
        out.extend(merged);
        if new.source.is_some() {
            out.source.clone_from(&new.source);
        }
        out.date_detection = new.date_detection.or(self.date_detection);
        out.numeric_detection = new.numeric_detection.or(self.numeric_detection);
        Ok(out)
    }

    /// False when the mapping sets `_source.enabled` to `false`.
    pub fn source_enabled(&self) -> bool {
        self.source.as_ref().is_none_or(|s| s["enabled"] != false)
    }
}

impl Index {
    /// Adds dynamic mappings for fields of `source` not yet mapped, object fields under their
    /// dotted paths. A document giving an object to a leaf field, or a value to an object field,
    /// maps nothing and fails with Elasticsearch's `document_parsing_exception` reason (without
    /// its `[line:column]` prefix).
    pub fn map_dynamic(&mut self, id: &str, source: &Value) -> Result<(), String> {
        let mut added = BTreeMap::new();
        for (k, v) in source.as_object().into_iter().flatten() {
            self.map_value(id, k, k, v, &mut added)?;
        }
        self.mappings.extend(added);
        Ok(())
    }

    /// Maps `v` at `path` into `added`; `name` is the JSON key it came from (`null` in arrays).
    fn map_value(
        &self,
        id: &str,
        path: &str,
        name: &str,
        v: &Value,
        added: &mut BTreeMap<String, FieldMapping>,
    ) -> Result<(), String> {
        let props = &self.mappings.properties;
        let get = |p: &str| added.get(p).or_else(|| props.get(p));
        let leaf_error = |field: &str, preview: &Value| {
            Err(format!(
                "failed to parse field [{field}] of type [{}] in document with id '{id}'. \
                 Preview of field's value: '{}'",
                get(field).map_or_else(String::new, FieldMapping::mapping_type),
                crate::setting_string(preview)
            ))
        };
        // A dotted key (`a.b`) whose prefix is a leaf gives that leaf an object.
        for (i, _) in path.match_indices('.') {
            if get(&path[..i]).is_some_and(|m| !m.is_object()) {
                let preview = (path[i + 1..].rsplit('.')).fold(v.clone(), |v, k| json!({ k: v }));
                return leaf_error(&path[..i], &preview);
            }
        }
        let mapping = match v {
            Value::Object(o) => {
                match get(path) {
                    Some(FieldMapping::LongRange | FieldMapping::DateRange { .. }) => return Ok(()),
                    Some(m) if !m.is_object() => return leaf_error(path, v),
                    None if o.is_empty() && !self.has_subfields(path, added) => {
                        added.insert(path.to_owned(), FieldMapping::Object);
                    }
                    _ => {}
                }
                for (k, x) in o {
                    self.map_value(id, &format!("{path}.{k}"), k, x, added)?;
                }
                return Ok(());
            }
            // Array elements map in order; the first scalar wins.
            Value::Array(a) => {
                for x in a {
                    self.map_value(id, path, "null", x, added)?;
                }
                return Ok(());
            }
            Value::Null => return Ok(()),
            _ if get(path).is_some_and(FieldMapping::is_object)
                || self.has_subfields(path, added) =>
            {
                return Err(format!(
                    "object mapping for [{path}] tried to parse field [{}] as object, but found a concrete value",
                    name.rsplit('.').next().unwrap_or(name)
                ));
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
        added.insert(path.to_owned(), mapping);
        Ok(())
    }

    /// Whether the mappings or `added` hold a field under `path.`.
    fn has_subfields(&self, path: &str, added: &BTreeMap<String, FieldMapping>) -> bool {
        maps_under(&self.mappings.properties, path) || maps_under(added, path)
    }
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
            node = obj
                .as_object_mut()
                .ok_or_else(|| S::Error::custom(path))?
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
            if nested {
                out.insert(path.clone(), FieldMapping::Nested);
            }
            if v.is_object() && (nested || v.get("type").is_none_or(|t| t == "object")) {
                let sub: Map<String, Value> = match v["properties"].take() {
                    Value::Null => Map::new(),
                    p => serde_json::from_value(p)?,
                };
                if sub.is_empty() && !nested {
                    out.insert(path, FieldMapping::Object);
                } else {
                    walk(&format!("{path}."), sub, out)?;
                }
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
