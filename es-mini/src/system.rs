//! Elasticsearch 9.5.5's system indices and system data streams, which its security restricts
//! and its index APIs manage.

/// The system index patterns (with their aliases) and system data streams of Elasticsearch
/// 9.5.5's default distribution, in `SystemIndexDescriptor`'s pattern syntax (a Lucene regular
/// expression, with `.` literal and `*` any string).
const SYSTEM_INDICES: [(&str, Option<&str>); 37] = [
    (".tasks*", None),
    (".synonyms-*", Some(".synonyms")),
    (".security-[0-9]+*", Some(crate::security::ALIAS)),
    (".security-tokens-[0-9]+*", Some(".security-tokens")),
    (".security-profile-[0-9]+*", Some(".security-profile")),
    (".async-search*", None),
    (".kibana_~(change_history*)", Some(".kibana")),
    (".reporting-*", None),
    (".chat-*", None),
    (".workflows~(-events*|-execution-data-stream-logs*)", None),
    (".apm-agent-configuration*", None),
    (".apm-custom-link*", None),
    (".fleet-actions~(-results*)", Some(".fleet-actions")),
    (".fleet-agents*", Some(".fleet-agents")),
    (
        ".fleet-enrollment-api-keys*",
        Some(".fleet-enrollment-api-keys"),
    ),
    (".fleet-secrets*", Some(".fleet-secrets")),
    (".fleet-policies-[0-9]+*", Some(".fleet-policies")),
    (".fleet-policies-leader*", Some(".fleet-policies-leader")),
    (".fleet-servers*", Some(".fleet-servers")),
    (".fleet-artifacts*", Some(".fleet-artifacts")),
    (".integration_knowledge*", Some(".integration_knowledge")),
    (".geoip_databases*", None),
    (".enrich-*", None),
    (".search-app-*", Some(".search-app")),
    (".query-rules-*", Some(".query-rules")),
    (".connector-secrets*", Some(".connector-secrets")),
    (".inference*", Some(".inference-alias")),
    (".secrets-inference*", None),
    (".ccm-inference*", None),
    (".logstash*", None),
    (".ml-meta*", None),
    (".ml-config*", None),
    (".ml-inference-*", None),
    (".snapshot-blob-cache*", None),
    (".transform-internal-*", None),
    (".watches*", None),
    (".triggered_watches*", None),
];
const SYSTEM_DATA_STREAMS: [&str; 4] = [
    ".kibana_change_history",
    ".workflows-events",
    ".workflows-execution-data-stream-logs",
    ".fleet-actions-results",
];

/// `SystemIndices.getSystemNameAutomaton`: the names `SYSTEM_INDICES` and
/// `SYSTEM_DATA_STREAMS` match, with the data streams' backing indices.
pub(crate) static RESTRICTED: std::sync::LazyLock<es_query::Regexp> =
    std::sync::LazyLock::new(|| {
        let indices = SYSTEM_INDICES
            .iter()
            .flat_map(|(p, alias)| [Some(*p), *alias])
            .flatten();
        // `SystemDataStreamDescriptor.getBackingIndexPattern`
        let backing = SYSTEM_DATA_STREAMS.map(|d| format!(".(migrated-){{0,}}[fd]s-{d}-*"));
        automaton(
            indices
                .chain(SYSTEM_DATA_STREAMS)
                .chain(backing.iter().map(|b| &**b)),
        )
    });

/// `SystemIndices.isSystemIndex`: the names the `SYSTEM_INDICES` patterns match.
static INDICES: std::sync::LazyLock<es_query::Regexp> =
    std::sync::LazyLock::new(|| automaton(SYSTEM_INDICES.iter().map(|(p, _)| *p)));

/// The names any of `patterns` matches.
fn automaton<'a>(patterns: impl Iterator<Item = &'a str>) -> es_query::Regexp {
    // `SystemIndexDescriptor.patternToRegex`
    let regex = |p: &str| format!("({})", p.replace('.', "\\.").replace('*', ".*"));
    let names: Vec<String> = patterns.map(regex).collect();
    es_query::Regexp::new(&names.join("|")).expect("valid system index patterns")
}

/// Marks `index`, being created as `name`, hidden if `name` is a system index, as
/// Elasticsearch's `MetadataCreateIndexService` does; it refuses to create one visible.
// ponytail: plain hidden indices; Elasticsearch builds them from their descriptors' mappings
// and settings (only `.security-7`'s is ported, `crate::security::managed_index`)
pub(crate) fn hide(index: &mut es_index::Index, name: &str) -> Result<(), crate::error::AppError> {
    if !INDICES.matches(name) {
        return Ok(());
    }
    if index.settings.get("index.hidden") == Some("false") {
        return Err(crate::error::AppError::Other(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "illegal_state_exception",
            format!("Cannot create system index [{name}] with [index.hidden] set to 'false'"),
        ));
    }
    index.settings.merge(&serde_json::json!({"hidden": "true"}));
    Ok(())
}

/// Refuses auto-creating the system data stream `name`, as Elasticsearch refuses any request
/// but its own system operations.
pub(crate) fn refuse_data_stream(name: &str) -> Result<(), crate::error::AppError> {
    if !SYSTEM_DATA_STREAMS.contains(&name) {
        return Ok(());
    }
    Err(crate::error::AppError::IllegalArgument(format!(
        "Data stream(s) [{name}] use and access is reserved for system operations"
    )))
}
