//! Field mappings and dynamic mapping.

use crate::{Index, parse_date_millis};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _, ser::Error as _};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

/// MVP leaf field types; objects live in [`Mappings::properties`] as dotted paths (no `nested`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FieldMapping {
    Text {
        /// Kept as a stored field; es-mini answers `stored_fields` from `_source`.
        // ponytail: disk saves do not write the Lucene stored field, so Elasticsearch reading
        // a Rust-written index finds no stored values; write them if that matters
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

    /// False when the mapping sets `_source.enabled` to `false`.
    pub fn source_enabled(&self) -> bool {
        self.source.as_ref().is_none_or(|s| s["enabled"] != false)
    }
}

impl Index {
    /// Adds dynamic mappings for fields of `source` not yet mapped, object fields under their
    /// dotted paths.
    pub fn map_dynamic(&mut self, source: &Value) {
        if let Some(obj) = source.as_object() {
            obj.iter().for_each(|(k, v)| self.map_value(k, v));
        }
    }

    fn map_value(&mut self, path: &str, v: &Value) {
        let mapping = match v {
            // ponytail: empty objects map nothing; ES maps them as `{"type": "object"}`
            Value::Object(o) => {
                return o
                    .iter()
                    .for_each(|(k, x)| self.map_value(&format!("{path}.{k}"), x));
            }
            // Array elements map in order; the first scalar wins.
            Value::Array(a) => return a.iter().for_each(|x| self.map_value(path, x)),
            // Like Elasticsearch's DynamicFieldsBuilder: numeric strings are never dates.
            // ponytail: Rust's f64 parse stands in for Java's Double.parseDouble (which also
            // takes `1f`, hex and surrounding spaces, but not `inf`)
            Value::String(s)
                if self.mappings.numeric_detection == Some(true) && s.parse::<i64>().is_ok() =>
            {
                FieldMapping::Long
            }
            Value::String(s)
                if self.mappings.numeric_detection == Some(true) && s.parse::<f64>().is_ok() =>
            {
                FieldMapping::Float
            }
            d @ Value::String(s)
                if self.mappings.date_detection != Some(false)
                    && s.parse::<f64>().is_err()
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
            Value::Null => return,
        };
        let props = &mut self.mappings.properties;
        // ponytail: a leaf meeting an object (or the reverse) is left unmapped; ES rejects the document
        let child = format!("{path}.");
        let conflict = path
            .match_indices('.')
            .any(|(i, _)| props.contains_key(&path[..i]))
            || props
                .range(child.clone()..)
                .next()
                .is_some_and(|(p, _)| p.starts_with(&child));
        if !conflict {
            props.entry(path.to_owned()).or_insert(mapping);
        }
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
            let obj = node
                .entry(part)
                .or_insert_with(|| json!({"properties": {}}));
            node = obj["properties"]
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

/// Reads nested object mappings (no `type`, or `object`) into dotted paths.
fn flatten<'de, D: Deserializer<'de>>(d: D) -> Result<BTreeMap<String, FieldMapping>, D::Error> {
    fn walk(
        prefix: &str,
        props: Map<String, Value>,
        out: &mut BTreeMap<String, FieldMapping>,
    ) -> serde_json::Result<()> {
        for (k, mut v) in props {
            let path = format!("{prefix}{k}");
            if v.is_object() && v.get("type").is_none_or(|t| t == "object") {
                // ponytail: an object without properties maps nothing (see map_value)
                let sub = match v["properties"].take() {
                    Value::Null => Map::new(),
                    p => serde_json::from_value(p)?,
                };
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
