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
    /// Managed through a dedicated API; PUT _settings rejects it.
    Internal,
}

#[derive(Debug, Clone, Copy)]
enum Kind {
    /// `Setting.intSetting`/`longSetting` with inclusive bounds.
    Int(i64, i64),
    Bool,
    /// `Setting.timeSetting` with a minimum and maybe a maximum: millis, and their
    /// `TimeValue.getStringRep`.
    Time(i64, &'static str, Option<(i64, &'static str)>),
    /// `Setting.doubleSetting` with inclusive bounds.
    Float(f64, f64),
    /// `Setting.enumSetting`: the enum's canonical class name and constants.
    Enum(&'static str, &'static [&'static str]),
    Codec,
    /// `Setting.byteSizeSetting` with a minimum and maybe a maximum in bytes, and their
    /// `ByteSizeValue.toString`.
    Bytes(i64, &'static str, Option<(i64, &'static str)>),
    /// `Setting.listSetting`: each element goes through the parser.
    List(fn(&str) -> Result<(), String>),
    /// A setting's own parser.
    Parse(fn(&str) -> Result<(), String>),
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
        Kind::Time(-1, "-1", None),
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
        Kind::Time(-1, "-1", None),
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
        Kind::Time(100, "100ms", None),
    ),
    s(
        TRANSLOG_FLUSH_THRESHOLD_SIZE,
        Update::Dynamic,
        Some("10gb"),
        Kind::Bytes(8, "8b", None),
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
        Kind::Time(0, "0s", None),
    ),
    s(
        "index.unassigned.node_left.delayed_timeout",
        Update::Dynamic,
        Some("1m"),
        Kind::Time(0, "0s", None),
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
    // The rest of IndexScopedSettings, x-pack's included, as ES 9.5.5 lists them under defaults.
    // ponytail: bespoke parsers are not ported, so these take any value: compound_format,
    // data_path, dense_vector.post_filter_selectivity_threshold, indexing.slowlog.source,
    // recovery.type, store.type, semantic_text.default_inference_id, time_series.start_time and
    // end_time, and merge.policy.floor_segment's and max_merged_segment's own bounds
    s(
        "index.allocation.existing_shards_allocator",
        Update::Private,
        Some("gateway_allocator"),
        Kind::Any,
    ),
    s(
        "index.allocation.max_retries",
        Update::Dynamic,
        Some("5"),
        Kind::Int(0, INT),
    ),
    s(
        "index.analyze.max_char_count",
        Update::Dynamic,
        Some("1000000"),
        Kind::Int(1, INT),
    ),
    s(
        "index.bloom_filter_for_id_field.enabled",
        Update::Dynamic,
        Some("true"),
        Kind::Bool,
    ),
    s(
        "index.compound_format",
        Update::Dynamic,
        Some("1gb"),
        Kind::Any,
    ),
    s("index.data_path", Update::Static, Some(""), Kind::Any),
    s(
        "index.dense_vector.experimental_features",
        Update::Final,
        Some("false"),
        Kind::Bool,
    ),
    s(
        "index.dense_vector.hnsw_enable_early_termination",
        Update::Dynamic,
        Some("true"),
        Kind::Bool,
    ),
    s(
        "index.dense_vector.hnsw_filter_heuristic",
        Update::Dynamic,
        Some("ACORN"),
        Kind::Enum(
            "org.elasticsearch.index.mapper.vectors.DenseVectorFieldMapper.FilterHeuristic",
            &["FANOUT", "ACORN"],
        ),
    ),
    s(
        "index.dense_vector.post_filter_selectivity_threshold",
        Update::Private,
        Some("1.0"),
        Kind::Any,
    ),
    s(
        "index.dimensions",
        Update::Private,
        Some("[]"),
        Kind::List(|_| Ok(())),
    ),
    s(
        "index.dimensions_tsid_strategy_enabled",
        Update::Final,
        Some("true"),
        Kind::Bool,
    ),
    s(
        "index.disable_sequence_numbers",
        Update::Final,
        Some("false"),
        Kind::Bool,
    ),
    s(
        "index.dlm.frozen.created",
        Update::Internal,
        Some("false"),
        Kind::Bool,
    ),
    s(
        "index.downsample.interval",
        Update::Internal,
        Some(""),
        Kind::Any,
    ),
    s(
        "index.downsample.origin.name",
        Update::Private,
        Some(""),
        Kind::Any,
    ),
    s(
        "index.downsample.origin.uuid",
        Update::Private,
        Some(""),
        Kind::Any,
    ),
    s(
        "index.downsample.sampling_method",
        Update::Internal,
        Some(""),
        Kind::Any,
    ),
    s(
        "index.downsample.source.name",
        Update::Private,
        Some(""),
        Kind::Any,
    ),
    s(
        "index.downsample.source.uuid",
        Update::Private,
        Some(""),
        Kind::Any,
    ),
    s(
        "index.downsample.status",
        Update::Internal,
        Some("unknown"),
        Kind::Enum(
            "org.elasticsearch.cluster.metadata.IndexMetadata.DownsampleTaskStatus",
            &["UNKNOWN", "STARTED", "SUCCESS"],
        ),
    ),
    s(
        "index.esql.stored_fields_sequential_proportion",
        Update::Dynamic,
        Some("0.2"),
        Kind::Float(0.0, 1.0),
    ),
    s(
        "index.failure_store.version",
        Update::Static,
        Some("0"),
        Kind::Int(-INT - 1, INT),
    ),
    s(
        "index.fast_refresh",
        Update::Final,
        Some("false"),
        Kind::Bool,
    ),
    s(
        "index.fielddata.cache",
        Update::Static,
        Some("node"),
        Kind::Parse(fielddata_cache),
    ),
    s(
        "index.flush_after_merge",
        Update::Dynamic,
        Some("512mb"),
        Kind::Bytes(0, "0", None),
    ),
    s(
        "index.format",
        Update::Final,
        Some("0"),
        Kind::Int(-INT - 1, INT),
    ),
    s(
        "index.highlight.weight_matches_mode.enabled",
        Update::Dynamic,
        Some("true"),
        Kind::Bool,
    ),
    s(
        "index.indexing.slowlog.include.user",
        Update::Dynamic,
        Some("false"),
        Kind::Bool,
    ),
    s(
        "index.indexing.slowlog.reformat",
        Update::Dynamic,
        Some("true"),
        Kind::Bool,
    ),
    s(
        "index.indexing.slowlog.source",
        Update::Dynamic,
        Some("1000"),
        Kind::Any,
    ),
    s(
        "index.lifecycle.origination_date",
        Update::Dynamic,
        Some("-1"),
        Kind::Int(-1, i64::MAX),
    ),
    s(
        "index.lifecycle.prefer_ilm",
        Update::Dynamic,
        Some("true"),
        Kind::Bool,
    ),
    s(
        "index.lifecycle.skip",
        Update::Dynamic,
        Some("false"),
        Kind::Bool,
    ),
    s(
        "index.lifecycle.step.wait_time_threshold",
        Update::Dynamic,
        Some("12h"),
        Kind::Time(3600000, "1h", None),
    ),
    s(
        "index.logsdb.add_host_name_field",
        Update::Private,
        Some("false"),
        Kind::Bool,
    ),
    s(
        "index.logsdb.default_sort_on_message_template",
        Update::Final,
        Some("false"),
        Kind::Bool,
    ),
    s(
        "index.logsdb.route_on_sort_fields",
        Update::Final,
        Some("false"),
        Kind::Bool,
    ),
    s(
        "index.logsdb.sort_on_host_name",
        Update::Private,
        Some("false"),
        Kind::Bool,
    ),
    s(
        "index.logsdb.sort_on_message_template",
        Update::Private,
        Some("false"),
        Kind::Bool,
    ),
    s(
        "index.look_ahead_time",
        Update::Dynamic,
        Some("9m"),
        Kind::Time(60000, "1m", Some((604800000, "7d"))),
    ),
    s(
        "index.look_back_time",
        Update::Dynamic,
        Some("2h"),
        Kind::Time(60000, "1m", Some((604800000, "7d"))),
    ),
    s(
        "index.mapping.doc_values.multi_value",
        Update::Final,
        Some("true"),
        Kind::Bool,
    ),
    s(
        "index.mapping.doc_values.nullability",
        Update::Final,
        Some("true"),
        Kind::Bool,
    ),
    s(
        "index.mapping.exclude_source_vectors",
        Update::Final,
        Some("true"),
        Kind::Bool,
    ),
    s(
        "index.mapping.flattened.store_root_doc_values",
        Update::Final,
        Some("false"),
        Kind::Bool,
    ),
    s(
        "index.mapping.ignore_above",
        Update::Static,
        Some("2147483647"),
        Kind::Int(0, INT),
    ),
    s(
        "index.mapping.index_disabled_by_default",
        Update::Final,
        Some("false"),
        Kind::Bool,
    ),
    s(
        "index.mapping.pattern_text.disable_templating",
        Update::Private,
        Some("false"),
        Kind::Bool,
    ),
    s(
        "index.mapping.semantic_text.use_legacy_format",
        Update::Internal,
        Some("false"),
        Kind::Bool,
    ),
    s(
        "index.mapping.source.mode",
        Update::Final,
        Some("STORED"),
        Kind::Enum(
            "org.elasticsearch.index.mapper.SourceFieldMapper.Mode",
            &["DISABLED", "STORED", "SYNTHETIC", "COLUMNAR_STORED"],
        ),
    ),
    s(
        "index.mapping.synthetic_id",
        Update::Final,
        Some("false"),
        Kind::Bool,
    ),
    s(
        "index.mapping.synthetic_source.skip_ignored_source_read",
        Update::Dynamic,
        Some("false"),
        Kind::Bool,
    ),
    s(
        "index.mapping.synthetic_source.skip_ignored_source_write",
        Update::Dynamic,
        Some("false"),
        Kind::Bool,
    ),
    s(
        "index.mapping.synthetic_source_keep",
        Update::Static,
        Some("none"),
        Kind::Enum(
            "org.elasticsearch.index.mapper.Mapper.SourceKeepMode",
            &["NONE", "ARRAYS", "ALL"],
        ),
    ),
    s(
        "index.mapping.use_columnar_id_mode_by_default",
        Update::Final,
        Some("false"),
        Kind::Bool,
    ),
    s(
        "index.mapping.use_doc_values_skipper",
        Update::Final,
        Some("false"),
        Kind::Bool,
    ),
    s(
        "index.max_slices_per_scroll",
        Update::Dynamic,
        Some("1024"),
        Kind::Int(1, INT),
    ),
    s(
        "index.merge.intra_merge_parallelism_enabled",
        Update::Dynamic,
        Some("false"),
        Kind::Bool,
    ),
    s(
        "index.merge.policy.deletes_pct_allowed",
        Update::Dynamic,
        Some("20.0"),
        Kind::Float(5.0, 50.0),
    ),
    s(
        "index.merge.policy.expunge_deletes_allowed",
        Update::Dynamic,
        Some("10.0"),
        Kind::Float(0.0, f64::INFINITY),
    ),
    s(
        "index.merge.policy.floor_segment",
        Update::Dynamic,
        Some("16mb"),
        Kind::Bytes(-1, "-1", None),
    ),
    s(
        "index.merge.policy.max_merge_at_once",
        Update::Dynamic,
        Some("16"),
        Kind::Int(2, INT),
    ),
    s(
        "index.merge.policy.max_merge_at_once_explicit",
        Update::Dynamic,
        Some("30"),
        Kind::Int(2, INT),
    ),
    s(
        "index.merge.policy.max_merged_segment",
        Update::Dynamic,
        Some("0b"),
        Kind::Bytes(-1, "-1", None),
    ),
    s(
        "index.merge.policy.merge_factor",
        Update::Dynamic,
        Some("32"),
        Kind::Int(2, INT),
    ),
    s(
        "index.merge.policy.segments_per_tier",
        Update::Dynamic,
        Some("8.0"),
        Kind::Float(2.0, f64::INFINITY),
    ),
    s(
        "index.merge.policy.type",
        Update::Dynamic,
        Some("UNSET"),
        Kind::Enum(
            "org.elasticsearch.index.MergePolicyConfig.Type",
            &["UNSET", "TIERED", "TIME_BASED"],
        ),
    ),
    s(
        "index.merge.scheduler.auto_throttle",
        Update::Dynamic,
        Some("true"),
        Kind::Bool,
    ),
    s(
        "index.merge.scheduler.max_merge_count",
        Update::Dynamic,
        Some("6"),
        Kind::Int(1, INT),
    ),
    s(
        "index.merge.scheduler.max_thread_count",
        Update::Dynamic,
        Some("1"),
        Kind::Int(1, INT),
    ),
    s(
        "index.mode",
        Update::Final,
        Some("standard"),
        Kind::Enum(
            "org.elasticsearch.index.IndexMode",
            &[
                "STANDARD",
                "TIME_SERIES",
                "LOGSDB",
                "LOOKUP",
                "COLUMNAR",
                "LOGSDB_COLUMNAR",
                "VECTORDB_DOCUMENT",
            ],
        ),
    ),
    s(
        "index.override_write_load_forecast",
        Update::Dynamic,
        Some("0.0"),
        Kind::Float(0.0, f64::INFINITY),
    ),
    s(
        "index.percolator.map_unmapped_fields_as_text",
        Update::Static,
        Some("false"),
        Kind::Bool,
    ),
    s(
        "index.query.default_field",
        Update::Dynamic,
        Some("[\"*\"]"),
        Kind::List(|_| Ok(())),
    ),
    s(
        "index.query.parse.allow_unmapped_fields",
        Update::Static,
        Some("true"),
        Kind::Bool,
    ),
    s("index.recovery.type", Update::Static, Some(""), Kind::Any),
    s(
        "index.recovery.use_synthetic_source",
        Update::Final,
        Some("false"),
        Kind::Bool,
    ),
    s(
        "index.rollup.source.name",
        Update::Private,
        Some(""),
        Kind::Any,
    ),
    s(
        "index.rollup.source.uuid",
        Update::Private,
        Some(""),
        Kind::Any,
    ),
    s(
        "index.routing.allocation.disk.watermark.ignore",
        Update::Private,
        Some("false"),
        Kind::Bool,
    ),
    s(
        "index.routing.allocation.enable",
        Update::Dynamic,
        Some("all"),
        Kind::Enum(
            "org.elasticsearch.cluster.routing.allocation.decider.EnableAllocationDecider.Allocation",
            &["NONE", "NEW_PRIMARIES", "PRIMARIES", "ALL"],
        ),
    ),
    s(
        "index.routing.rebalance.enable",
        Update::Dynamic,
        Some("all"),
        Kind::Enum(
            "org.elasticsearch.cluster.routing.allocation.decider.EnableAllocationDecider.Rebalance",
            &["NONE", "PRIMARIES", "REPLICAS", "ALL"],
        ),
    ),
    s(
        "index.routing_path",
        Update::Final,
        Some("[]"),
        Kind::List(|_| Ok(())),
    ),
    s(
        "index.search.slowlog.include.user",
        Update::Dynamic,
        Some("false"),
        Kind::Bool,
    ),
    s(
        "index.semantic_text.default_inference_id",
        Update::Final,
        Some(""),
        Kind::Any,
    ),
    s(
        "index.seq_no.index_options",
        Update::Final,
        Some("POINTS_AND_DOC_VALUES"),
        Kind::Enum(
            "org.elasticsearch.index.mapper.SeqNoFieldMapper.SeqNoIndexOptions",
            &["DOC_VALUES_ONLY", "POINTS_AND_DOC_VALUES"],
        ),
    ),
    s(
        "index.shard_limit.group",
        Update::Private,
        Some("normal"),
        Kind::Any,
    ),
    s(
        "index.slice.enabled",
        Update::Final,
        Some("false"),
        Kind::Bool,
    ),
    s(
        "index.soft_deletes.retention.operations",
        Update::Dynamic,
        Some("0"),
        Kind::Int(0, i64::MAX),
    ),
    s(
        "index.soft_deletes.retention_lease.period",
        Update::Dynamic,
        Some("12h"),
        Kind::Time(0, "0ms", None),
    ),
    s(
        "index.source_only",
        Update::Private,
        Some("false"),
        Kind::Bool,
    ),
    s(
        "index.store.fs.directio_async_prefetch_limit",
        Update::Static,
        Some("64"),
        Kind::Int(0, 256),
    ),
    s(
        "index.store.fs.fs_lock",
        Update::Static,
        Some("native"),
        Kind::Parse(fs_lock),
    ),
    s(
        "index.store.preload",
        Update::Static,
        Some("[]"),
        Kind::List(|_| Ok(())),
    ),
    s(
        "index.store.snapshot.blob_cache.metadata_files.max_length",
        Update::Static,
        Some("64kb"),
        Kind::Bytes(1024, "1kb", None),
    ),
    s(
        "index.store.snapshot.cache.enabled",
        Update::Static,
        Some("true"),
        Kind::Bool,
    ),
    s(
        "index.store.snapshot.cache.excluded_file_types",
        Update::Static,
        Some("[]"),
        Kind::List(|_| Ok(())),
    ),
    s(
        "index.store.snapshot.cache.prewarm.enabled",
        Update::Static,
        Some("true"),
        Kind::Bool,
    ),
    s(
        "index.store.snapshot.delete_searchable_snapshot",
        Update::Private,
        Some("false"),
        Kind::Bool,
    ),
    s(
        "index.store.snapshot.index_name",
        Update::Private,
        Some(""),
        Kind::Any,
    ),
    s(
        "index.store.snapshot.index_uuid",
        Update::Private,
        Some(""),
        Kind::Any,
    ),
    s(
        "index.store.snapshot.partial",
        Update::Private,
        Some("false"),
        Kind::Bool,
    ),
    s(
        "index.store.snapshot.repository_name",
        Update::Private,
        Some(""),
        Kind::Any,
    ),
    s(
        "index.store.snapshot.repository_uuid",
        Update::Private,
        Some(""),
        Kind::Any,
    ),
    s(
        "index.store.snapshot.snapshot_name",
        Update::Private,
        Some(""),
        Kind::Any,
    ),
    s(
        "index.store.snapshot.snapshot_uuid",
        Update::Private,
        Some(""),
        Kind::Any,
    ),
    s(
        "index.store.snapshot.uncached_chunk_size",
        Update::Static,
        Some("-1b"),
        Kind::Bytes(-1, "-1", None),
    ),
    s(
        "index.store.stats_refresh_interval",
        Update::Static,
        Some("10s"),
        Kind::Time(-1, "-1", None),
    ),
    s("index.store.type", Update::Static, Some(""), Kind::Any),
    s(
        "index.synthetic_id.bloom_filter.high_bits_per_doc",
        Update::Final,
        Some("128.0"),
        Kind::Float(8.0, 256.0),
    ),
    s(
        "index.synthetic_id.bloom_filter.large_segment_min_docs",
        Update::Final,
        Some("320000"),
        Kind::Int(1, INT),
    ),
    s(
        "index.synthetic_id.bloom_filter.low_bits_per_doc",
        Update::Final,
        Some("24.0"),
        Kind::Float(8.0, 256.0),
    ),
    s(
        "index.synthetic_id.bloom_filter.max_size",
        Update::Final,
        Some("8mb"),
        Kind::Bytes(65536, "64kb", Some((8388608, "8mb"))),
    ),
    s(
        "index.synthetic_id.bloom_filter.num_hash_functions",
        Update::Final,
        Some("4"),
        Kind::Int(1, INT),
    ),
    s(
        "index.synthetic_id.bloom_filter.optimized_merge",
        Update::Final,
        Some("true"),
        Kind::Bool,
    ),
    s(
        "index.synthetic_id.bloom_filter.small_segment_max_docs",
        Update::Final,
        Some("160000"),
        Kind::Int(1, INT),
    ),
    s(
        "index.time_series.end_time",
        Update::Dynamic,
        Some("9999-12-31T23:59:59.999Z"),
        Kind::Any,
    ),
    s(
        "index.time_series.es87tsdb_codec.enabled",
        Update::Final,
        Some("true"),
        Kind::Bool,
    ),
    s(
        "index.time_series.es95_codec.enabled",
        Update::Final,
        Some("false"),
        Kind::Bool,
    ),
    s(
        "index.time_series.start_time",
        Update::Final,
        Some("-9999-01-01T00:00:00Z"),
        Kind::Any,
    ),
    s(
        "index.time_series.temporality_field",
        Update::Final,
        Some(""),
        Kind::Any,
    ),
    s(
        "index.top_metrics_max_size",
        Update::Dynamic,
        Some("10"),
        Kind::Int(1, INT),
    ),
    s(
        "index.translog.flush_threshold_age",
        Update::Dynamic,
        Some("1m"),
        Kind::Time(1000, "1s", Some((3600000, "1h"))),
    ),
    s(
        "index.translog.generation_threshold_size",
        Update::Dynamic,
        Some("64mb"),
        Kind::Bytes(56, "56b", None),
    ),
    s(
        "index.translog.retention.age",
        Update::Dynamic,
        Some("-1"),
        Kind::Parse(|v| {
            time_millis(v, "index.translog.retention.age")?;
            Err(TRANSLOG_RETENTION.into())
        }),
    ),
    s(
        "index.translog.retention.size",
        Update::Dynamic,
        Some("-1"),
        Kind::Parse(|v| {
            byte_size(v, "index.translog.retention.size")?;
            Err(TRANSLOG_RETENTION.into())
        }),
    ),
    s(
        "index.use_legacy_postings_format",
        Update::Final,
        Some("false"),
        Kind::Bool,
    ),
    s(
        "index.use_time_series_doc_values_format",
        Update::Final,
        Some("false"),
        Kind::Bool,
    ),
    s(
        "index.use_time_series_doc_values_format_large_binary_block_size",
        Update::Final,
        Some("true"),
        Kind::Bool,
    ),
    s(
        "index.use_time_series_doc_values_format_large_block_size",
        Update::Final,
        Some("false"),
        Kind::Bool,
    ),
    s(
        "index.verified_before_close",
        Update::Private,
        Some("false"),
        Kind::Bool,
    ),
    s(
        "index.verified_read_only",
        Update::Private,
        Some("false"),
        Kind::Bool,
    ),
    s(
        "index.version.compatibility",
        Update::Private,
        None,
        Kind::Any,
    ),
    s(
        "index.warmer.enabled",
        Update::Dynamic,
        Some("true"),
        Kind::Bool,
    ),
    s(
        "index.xpack.ccr.following_index",
        Update::Internal,
        Some("false"),
        Kind::Bool,
    ),
    s("index.xpack.version", Update::Static, Some(""), Kind::Any),
    s(
        "index.xpack.watcher.template.version",
        Update::Static,
        Some(""),
        Kind::Any,
    ),
];

const TRANSLOG_RETENTION: &str = "Translog retention settings [index.translog.retention.age] and [index.translog.retention.size] are no longer supported. Please do not specify values for these settings";

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
            Kind::Time(min, rep, max) => bounded(v, key, time_millis(v, key)?, (min, rep), max),
            Kind::Float(min, max) => {
                let n = (v.trim().parse::<f64>())
                    .map_err(|_| format!("Failed to parse value [{v}] for setting [{key}]"))?;
                match () {
                    _ if n < min => Err(format!(
                        "Failed to parse value [{v}] for setting [{key}] must be >= {min:?}"
                    )),
                    _ if n > max => Err(format!(
                        "Failed to parse value [{v}] for setting [{key}] must be <= {max:?}"
                    )),
                    _ => Ok(()),
                }
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
            Kind::Bytes(min, rep, max) => {
                let n = byte_size(v, key)?
                    .try_into()
                    .expect("byte_size caps at i64::MAX");
                bounded(v, key, n, (min, rep), max)
            }
            // A string holds a comma-separated list, as Settings.getAsList splits it.
            Kind::List(parse) => (v.split(',').map(str::trim))
                .filter(|e| !e.is_empty())
                .try_for_each(parse),
            Kind::Parse(parse) => parse(v),
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

/// Checks a parsed time or byte size `n` against a setting's bounds, with their renderings.
fn bounded(
    v: &str,
    key: &str,
    n: i64,
    (min, rep): (i64, &str),
    max: Option<(i64, &str)>,
) -> Result<(), String> {
    if n < min {
        return Err(format!(
            "failed to parse value [{v}] for setting [{key}], must be >= [{rep}]"
        ));
    }
    match max {
        Some((max, rep)) if n > max => Err(format!(
            "failed to parse value [{v}] for setting [{key}], must be <= [{rep}]"
        )),
        _ => Ok(()),
    }
}

fn fielddata_cache(v: &str) -> Result<(), String> {
    match v {
        "node" | "none" => Ok(()),
        _ => Err(format!("failed to parse [{v}] must be one of [node,none]")),
    }
}

fn fs_lock(v: &str) -> Result<(), String> {
    match v {
        "native" | "simple" => Ok(()),
        _ => Err(format!(
            "unrecognized [index.store.fs.fs_lock] \"{v}\": must be native or simple"
        )),
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
pub fn time_millis(v: &str, key: &str) -> Result<i64, String> {
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
