//! Field mappings and dynamic mapping.

use crate::{Index, parse_date_millis};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// MVP field types; flat mappings only (no object/nested).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FieldMapping {
    Text,
    /// Dynamically mapped string: `text` with a `keyword` subfield.
    #[serde(rename = "text", skip_deserializing)]
    TextKeyword {
        fields: BTreeMap<String, FieldMapping>,
    },
    Keyword {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ignore_above: Option<u32>,
    },
    Long,
    Integer,
    Short,
    Byte,
    Double,
    Float,
    Boolean,
    Date {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        format: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        locale: Option<String>,
    },
    Ip,
}

impl FieldMapping {
    /// The ES|QL-facing type name (`text`, `keyword`, `long`, `integer`, `double`, `boolean`, `date`,
    /// `ip`).
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Text | Self::TextKeyword { .. } => "text",
            Self::Keyword { .. } => "keyword",
            Self::Long => "long",
            Self::Integer | Self::Short | Self::Byte => "integer",
            Self::Double | Self::Float => "double",
            Self::Boolean => "boolean",
            Self::Date { .. } => "date",
            Self::Ip => "ip",
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
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub properties: BTreeMap<String, FieldMapping>,
}

impl Mappings {
    /// False when the mapping sets `_source.enabled` to `false`.
    pub fn source_enabled(&self) -> bool {
        self.source.as_ref().is_none_or(|s| s["enabled"] != false)
    }
}

impl Index {
    /// Adds dynamic mappings for fields of `source` not yet mapped.
    pub fn map_dynamic(&mut self, source: &Value) {
        // ponytail: top-level fields only; no nested objects or numeric_detection
        let auto_text =
            self.settings.get("index.mapping.dynamic_strings.auto_text") != Some("false");
        let Some(obj) = source.as_object() else {
            return;
        };
        for (k, v) in obj {
            // An array maps like its first non-null element.
            let v = match v {
                Value::Array(a) => a.iter().find(|e| !e.is_null()),
                v => Some(v),
            };
            let mapping = match v {
                Some(d @ Value::String(_)) if parse_date_millis(d).is_some() => {
                    FieldMapping::Date {
                        format: None,
                        locale: None,
                    }
                }
                Some(Value::String(_)) if auto_text => FieldMapping::TextKeyword {
                    fields: [(
                        "keyword".to_owned(),
                        FieldMapping::Keyword {
                            ignore_above: Some(256),
                        },
                    )]
                    .into(),
                },
                Some(Value::String(_)) => FieldMapping::Keyword {
                    ignore_above: Some(256),
                },
                Some(Value::Bool(_)) => FieldMapping::Boolean,
                Some(Value::Number(n)) if n.is_f64() => FieldMapping::Float,
                Some(Value::Number(_)) => FieldMapping::Long,
                _ => continue,
            };
            self.mappings.properties.entry(k.clone()).or_insert(mapping);
        }
    }
}
