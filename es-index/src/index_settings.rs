//! The registered index settings es-mini models, from Elasticsearch 9.5.5's
//! IndexScopedSettings: how each may change after creation, its default, and how its value
//! parses (Setting's parsers, with their messages).

use serde_json::Value;

/// How Elasticsearch lets a setting change after index creation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Update {
    Dynamic,
    /// Only on closed indices (or with `reopen=true`).
    Static,
    /// Never.
    Final,
    /// Managed by Elasticsearch; PUT _settings rejects it.
    Private,
}

#[derive(Debug, Clone, Copy)]
enum Kind {
    /// `Setting.intSetting`/`longSetting` with inclusive bounds.
    Int(i64, i64),
    Bool,
    /// `Setting.timeSetting` with a minimum: millis, and its `TimeValue.getStringRep`.
    Time(i64, &'static str),
    /// `Setting.enumSetting`: the enum's canonical class name and constants.
    Enum(&'static str, &'static [&'static str]),
    Codec,
    /// `Setting.byteSizeSetting` with a minimum in bytes, and its `ByteSizeValue.toString`.
    Bytes(i64, &'static str),
    /// `Setting.listSetting`: each element goes through the parser.
    List(fn(&str) -> Result<(), String>),
    Any,
}

#[derive(Debug)]
pub struct Setting {
    pub key: &'static str,
    pub update: Update,
    /// What GET _settings?include_defaults shows when the index does not set it.
    pub default: Option<&'static str>,
    kind: Kind,
}

const fn s(
    key: &'static str,
    update: Update,
    default: Option<&'static str>,
    kind: Kind,
) -> Setting {
    Setting {
        key,
        update,
        default,
        kind,
    }
}

pub const NUMBER_OF_SHARDS: &str = "index.number_of_shards";
pub const NUMBER_OF_REPLICAS: &str = "index.number_of_replicas";
pub const REFRESH_INTERVAL: &str = "index.refresh_interval";
pub const GC_DELETES: &str = "index.gc_deletes";
pub const UUID: &str = "index.uuid";
pub const CREATION_DATE: &str = "index.creation_date";
pub const VERSION_CREATED: &str = "index.version.created";
pub const TRANSLOG_DURABILITY: &str = "index.translog.durability";
pub const TRANSLOG_SYNC_INTERVAL: &str = "index.translog.sync_interval";
pub const TRANSLOG_FLUSH_THRESHOLD_SIZE: &str = "index.translog.flush_threshold_size";

const INT: i64 = i32::MAX as i64;

pub const SETTINGS: &[Setting] = &[
    s("index.codec", Update::Static, Some("default"), Kind::Codec),
    s(
        NUMBER_OF_SHARDS,
        Update::Final,
        Some("1"),
        Kind::Int(1, 1024),
    ),
    s(
        "index.number_of_routing_shards",
        Update::Final,
        None,
        Kind::Int(1, INT),
    ),
    s(
        "index.routing_partition_size",
        Update::Final,
        Some("1"),
        Kind::Int(1, INT),
    ),
    s(
        NUMBER_OF_REPLICAS,
        Update::Dynamic,
        Some("1"),
        Kind::Int(0, INT),
    ),
    s(
        REFRESH_INTERVAL,
        Update::Dynamic,
        Some("1s"),
        Kind::Time(-1, "-1"),
    ),
    s("index.hidden", Update::Dynamic, Some("false"), Kind::Bool),
    s(
        "index.max_result_window",
        Update::Dynamic,
        Some("10000"),
        Kind::Int(1, INT),
    ),
    s(
        GC_DELETES,
        Update::Dynamic,
        Some("60s"),
        Kind::Time(-1, "-1"),
    ),
    s(
        "index.soft_deletes.enabled",
        Update::Final,
        Some("true"),
        Kind::Bool,
    ),
    s(
        "index.requests.cache.enable",
        Update::Dynamic,
        Some("true"),
        Kind::Bool,
    ),
    s(
        "index.query_string.lenient",
        Update::Static,
        Some("false"),
        Kind::Bool,
    ),
    s(
        TRANSLOG_DURABILITY,
        Update::Dynamic,
        Some("REQUEST"),
        Kind::Enum(
            "org.elasticsearch.index.translog.Translog.Durability",
            &["REQUEST", "ASYNC"],
        ),
    ),
    s(
        TRANSLOG_SYNC_INTERVAL,
        Update::Dynamic,
        Some("5s"),
        Kind::Time(100, "100ms"),
    ),
    s(
        TRANSLOG_FLUSH_THRESHOLD_SIZE,
        Update::Dynamic,
        Some("10gb"),
        Kind::Bytes(8, "8b"),
    ),
    s(
        "index.mapping.array_objects.limit",
        Update::Dynamic,
        Some("50000"),
        Kind::Int(1, i64::MAX),
    ),
    s(
        "index.mapping.dynamic_strings.auto_text",
        Update::Dynamic,
        Some("true"),
        Kind::Bool,
    ),
    s(
        "index.mapping.total_fields.limit",
        Update::Dynamic,
        Some("1000"),
        Kind::Int(0, i64::MAX),
    ),
    s(
        "index.mapping.total_fields.ignore_dynamic_beyond_limit",
        Update::Dynamic,
        Some("false"),
        Kind::Bool,
    ),
    s(
        "index.mapping.depth.limit",
        Update::Dynamic,
        Some("20"),
        Kind::Int(1, i64::MAX),
    ),
    s(
        "index.mapping.nested_fields.limit",
        Update::Dynamic,
        Some("100"),
        Kind::Int(0, i64::MAX),
    ),
    s(
        "index.mapping.nested_parents.limit",
        Update::Dynamic,
        Some("50"),
        Kind::Int(0, i64::MAX),
    ),
    s(
        "index.mapping.nested_objects.limit",
        Update::Dynamic,
        Some("10000"),
        Kind::Int(0, i64::MAX),
    ),
    s(
        "index.mapping.field_name_length.limit",
        Update::Dynamic,
        Some("9223372036854775807"),
        Kind::Int(1, i64::MAX),
    ),
    s(
        "index.mapping.field_name_length.ignore_dynamic_beyond_limit",
        Update::Dynamic,
        Some("false"),
        Kind::Bool,
    ),
    s(
        "index.mapping.dimension_fields.limit",
        Update::Dynamic,
        Some("32768"),
        Kind::Int(0, i64::MAX),
    ),
    s(
        "index.blocks.read_only",
        Update::Dynamic,
        Some("false"),
        Kind::Bool,
    ),
    s(
        "index.blocks.read",
        Update::Dynamic,
        Some("false"),
        Kind::Bool,
    ),
    s(
        "index.blocks.write",
        Update::Dynamic,
        Some("false"),
        Kind::Bool,
    ),
    s(
        "index.blocks.metadata",
        Update::Dynamic,
        Some("false"),
        Kind::Bool,
    ),
    s(
        "index.blocks.read_only_allow_delete",
        Update::Dynamic,
        Some("false"),
        Kind::Bool,
    ),
    s(
        "index.sort.field",
        Update::Final,
        Some("[]"),
        Kind::List(|_| Ok(())),
    ),
    s(
        "index.sort.order",
        Update::Final,
        Some("[]"),
        Kind::List(sort_order),
    ),
    s(
        "index.sort.mode",
        Update::Final,
        Some("[]"),
        Kind::List(sort_mode),
    ),
    s(
        "index.sort.missing",
        Update::Final,
        Some("[]"),
        Kind::List(sort_missing),
    ),
    s(
        "index.max_inner_result_window",
        Update::Dynamic,
        Some("100"),
        Kind::Int(1, INT),
    ),
    s(
        "index.max_rescore_window",
        Update::Dynamic,
        Some("10000"),
        Kind::Int(1, INT),
    ),
    s(
        "index.max_docvalue_fields_search",
        Update::Dynamic,
        Some("100"),
        Kind::Int(0, INT),
    ),
    s(
        "index.max_script_fields",
        Update::Dynamic,
        Some("32"),
        Kind::Int(0, INT),
    ),
    s(
        "index.max_ngram_diff",
        Update::Dynamic,
        Some("1"),
        Kind::Int(0, INT),
    ),
    s(
        "index.max_shingle_diff",
        Update::Dynamic,
        Some("3"),
        Kind::Int(0, INT),
    ),
    s(
        "index.max_refresh_listeners",
        Update::Dynamic,
        None,
        Kind::Int(0, INT),
    ),
    s(
        "index.max_terms_count",
        Update::Dynamic,
        Some("65536"),
        Kind::Int(1, INT),
    ),
    s(
        "index.max_regex_length",
        Update::Dynamic,
        Some("1000"),
        Kind::Int(1, INT),
    ),
    s(
        "index.highlight.max_analyzed_offset",
        Update::Dynamic,
        Some("1000000"),
        Kind::Int(1, INT),
    ),
    s(
        "index.analyze.max_token_count",
        Update::Dynamic,
        Some("10000"),
        Kind::Int(1, INT),
    ),
    s(
        "index.search.idle.after",
        Update::Dynamic,
        Some("30s"),
        Kind::Time(0, "0s"),
    ),
    s(
        "index.unassigned.node_left.delayed_timeout",
        Update::Dynamic,
        Some("1m"),
        Kind::Time(0, "0s"),
    ),
    s(
        "index.priority",
        Update::Dynamic,
        Some("1"),
        Kind::Int(0, INT),
    ),
    s(
        "index.routing.allocation.total_shards_per_node",
        Update::Dynamic,
        Some("-1"),
        Kind::Int(-1, INT),
    ),
    s(
        "index.queries.cache.enabled",
        Update::Static,
        Some("true"),
        Kind::Bool,
    ),
    s(
        "index.load_fixed_bitset_filters_eagerly",
        Update::Dynamic,
        Some("true"),
        Kind::Bool,
    ),
    s(
        "index.mapping.ignore_malformed",
        Update::Dynamic,
        Some("false"),
        Kind::Bool,
    ),
    s(
        "index.mapping.coerce",
        Update::Dynamic,
        Some("true"),
        Kind::Bool,
    ),
    s(
        "index.default_pipeline",
        Update::Dynamic,
        Some("_none"),
        Kind::Any,
    ),
    s(
        "index.final_pipeline",
        Update::Dynamic,
        Some("_none"),
        Kind::Any,
    ),
    s(
        "index.auto_expand_replicas",
        Update::Dynamic,
        Some("false"),
        Kind::Any,
    ),
    s(
        "index.write.wait_for_active_shards",
        Update::Dynamic,
        Some("1"),
        Kind::Any,
    ),
    s(
        "index.shard.check_on_startup",
        Update::Static,
        Some("false"),
        Kind::Any,
    ),
    s("index.lifecycle.name", Update::Dynamic, None, Kind::Any),
    s(
        "index.lifecycle.rollover_alias",
        Update::Dynamic,
        None,
        Kind::Any,
    ),
    s(
        "index.lifecycle.indexing_complete",
        Update::Dynamic,
        None,
        Kind::Bool,
    ),
    s(
        "index.lifecycle.parse_origination_date",
        Update::Dynamic,
        None,
        Kind::Bool,
    ),
    s(
        "index.search.slowlog.threshold.query.",
        Update::Dynamic,
        None,
        Kind::Any,
    ),
    s(
        "index.search.slowlog.threshold.fetch.",
        Update::Dynamic,
        None,
        Kind::Any,
    ),
    s(
        "index.indexing.slowlog.threshold.index.",
        Update::Dynamic,
        None,
        Kind::Any,
    ),
    // Group and affix settings: a key ending in `.` covers every key under it.
    s("index.analysis.", Update::Static, None, Kind::Any),
    s("index.similarity.", Update::Static, None, Kind::Any),
    s(
        "index.routing.allocation.include.",
        Update::Dynamic,
        None,
        Kind::Any,
    ),
    s(
        "index.routing.allocation.exclude.",
        Update::Dynamic,
        None,
        Kind::Any,
    ),
    s(
        "index.routing.allocation.require.",
        Update::Dynamic,
        None,
        Kind::Any,
    ),
    s(VERSION_CREATED, Update::Private, None, Kind::Any),
];

/// The registered setting with this full key (`index.` included), or the group setting covering it.
pub fn get(key: &str) -> Option<&'static Setting> {
    SETTINGS
        .iter()
        .find(|s| s.key == key || (s.key.ends_with('.') && key.starts_with(s.key)))
}

impl Setting {
    /// Checks a value parses as Elasticsearch's setting parser would, with its error message.
    pub fn validate(&self, v: &str) -> Result<(), String> {
        let key = self.key;
        match self.kind {
            Kind::Int(min, max) => {
                let digits = v.strip_prefix(['+', '-']).unwrap_or(v);
                if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(format!("Failed to parse value [{v}] for setting [{key}]"));
                }
                let bound = match v.parse::<i64>() {
                    Ok(n) if n < min => Some((">=", min)),
                    Ok(n) if n > max => Some(("<=", max)),
                    Ok(_) => None,
                    Err(_) if v.starts_with('-') => Some((">=", min)),
                    Err(_) => Some(("<=", max)),
                };
                match bound {
                    Some((op, b)) => Err(format!(
                        "Failed to parse value [{v}] for setting [{key}] must be {op} {b}"
                    )),
                    None => Ok(()),
                }
            }
            Kind::Bool if v == "true" || v == "false" => Ok(()),
            Kind::Bool => Err(format!(
                "Failed to parse value [{v}] as only [true] or [false] are allowed."
            )),
            Kind::Time(min, rep) => {
                if time_millis(v, key)? < min {
                    return Err(format!(
                        "failed to parse value [{v}] for setting [{key}], must be >= [{rep}]"
                    ));
                }
                Ok(())
            }
            Kind::Enum(class, values) => {
                let upper = v.to_uppercase();
                if values.contains(&upper.as_str()) {
                    Ok(())
                } else {
                    Err(format!("No enum constant {class}.{upper}"))
                }
            }
            // ponytail: Lucene codec names are the Lucene9x/10x ones; SimpleText is not shipped
            Kind::Codec => match v {
                "default"
                | "legacy_default"
                | "best_compression"
                | "legacy_best_compression"
                | "lucene_default"
                | "Lucene104"
                | "Lucene103"
                | "Lucene101"
                | "Lucene100"
                | "Lucene99"
                | "Lucene95"
                | "Lucene94"
                | "Lucene92"
                | "Lucene91"
                | "Lucene90" => Ok(()),
                _ => Err(format!(
                    "unknown value for [index.codec] must be one of [default, best_compression] but was: {v}"
                )),
            },
            Kind::Bytes(min, rep) => {
                if byte_size(v, key)? < min as i128 {
                    return Err(format!(
                        "failed to parse value [{v}] for setting [{key}], must be >= [{rep}]"
                    ));
                }
                Ok(())
            }
            // A string holds a comma-separated list, as Settings.getAsList splits it.
            Kind::List(parse) => (v.split(',').map(str::trim))
                .filter(|e| !e.is_empty())
                .try_for_each(parse),
            Kind::Any => Ok(()),
        }
    }

    /// Like [`validate`](Self::validate), for a value as stored: a JSON list is checked per element.
    pub fn validate_value(&self, v: &Value) -> Result<(), String> {
        match (self.kind, v) {
            (Kind::List(parse), Value::Array(items)) => items
                .iter()
                .try_for_each(|e| parse(&crate::setting_string(e))),
            _ => self.validate(&crate::setting_string(v)),
        }
    }
}

fn sort_order(v: &str) -> Result<(), String> {
    match v.to_lowercase().as_str() {
        "asc" | "desc" => Ok(()),
        _ => Err(format!("Illegal sort order:{v}")),
    }
}

fn sort_mode(v: &str) -> Result<(), String> {
    let upper = v.to_uppercase();
    match upper.as_str() {
        "MIN" | "MAX" => Ok(()),
        "SUM" | "AVG" | "MEDIAN" => Err(format!(
            "Illegal index sort mode:[{upper}], must be one of [MAX, MIN]"
        )),
        _ => Err(format!(
            "No enum constant org.elasticsearch.search.MultiValueMode.{upper}"
        )),
    }
}

fn sort_missing(v: &str) -> Result<(), String> {
    match v {
        "_last" | "_first" => Ok(()),
        _ => Err(format!(
            "Illegal missing value:[{v}], must be one of [_last, _first]"
        )),
    }
}

/// `ByteSizeValue.parseBytesSizeValue`, in bytes.
fn byte_size(v: &str, key: &str) -> Result<i128, String> {
    let n = v.to_lowercase();
    let n = n.trim();
    let units: [(&str, i128); 6] = [
        ("pb", 1 << 50),
        ("tb", 1 << 40),
        ("gb", 1 << 30),
        ("mb", 1 << 20),
        ("kb", 1 << 10),
        ("b", 1),
    ];
    let Some((suffix, mult)) = units.into_iter().find(|(u, _)| n.ends_with(u)) else {
        if n == "-1" || n == "0" {
            return Ok(n.parse().expect("-1 or 0"));
        }
        return Err(format!(
            "failed to parse setting [{key}] with value [{v}] as a size in bytes: unit is missing or unrecognized"
        ));
    };
    let num = n[..n.len() - suffix.len()].trim();
    let bytes = match num.parse::<i64>() {
        Ok(d) => d as i128 * mult,
        Err(_) => match num.parse::<f64>() {
            Ok(d) => (d * mult as f64) as i128,
            Err(_) => {
                return Err(format!(
                    "failed to parse setting [{key}] with value [{v}] as a size in bytes"
                ));
            }
        },
    };
    if bytes < -1 {
        return Err(format!("Values less than -1 bytes are not supported: {v}"));
    }
    if bytes > i64::MAX as i128 {
        return Err(format!(
            "failed to parse setting [{key}] with value [{v}] because it is larger than the max size of 9223372036854775807 bytes"
        ));
    }
    Ok(bytes)
}

/// `TimeValue.parseTimeValue`, in millis.
pub(crate) fn time_millis(v: &str, key: &str) -> Result<i64, String> {
    const S: i128 = 1_000_000_000;
    let lower = v.to_lowercase();
    let n = lower.trim();
    let unit = [("nanos", 1), ("micros", 1_000), ("ms", 1_000_000), ("s", S)]
        .into_iter()
        .find(|(u, _)| n.ends_with(u))
        // `m` alone is case-sensitive: `M` would mean months.
        .or_else(|| v.ends_with('m').then_some(("m", 60 * S)))
        .or_else(|| {
            [("h", 3_600 * S), ("d", 86_400 * S)]
                .into_iter()
                .find(|(u, _)| n.ends_with(u))
        });
    let Some((suffix, nanos)) = unit else {
        // -1 and 0 need no unit.
        if n.strip_prefix('-')
            .is_some_and(|r| r.trim_start_matches('0') == "1")
        {
            return Ok(-1);
        }
        if !n.is_empty() && n.bytes().all(|b| b == b'0') {
            return Ok(0);
        }
        return Err(format!(
            "failed to parse setting [{key}] with value [{v}] as a time value: unit is missing or unrecognized"
        ));
    };
    let num = n[..n.len() - suffix.len()].trim();
    match num.parse::<i64>() {
        Ok(d) if d < -1 => Err(format!(
            "failed to parse setting [{key}] with value [{v}] as a time value: negative durations are not supported"
        )),
        Ok(d) => {
            Ok((d as i128 * nanos / 1_000_000).clamp(i64::MIN as i128, i64::MAX as i128) as i64)
        }
        Err(_) if num.parse::<f64>().is_ok() => Err(format!(
            "failed to parse [{v}], fractional time values are not supported"
        )),
        Err(_) => Err(format!("failed to parse [{v}]")),
    }
}
