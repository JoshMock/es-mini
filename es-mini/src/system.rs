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
        // `SystemIndexDescriptor.patternToRegex`
        let regex = |p: &str| format!("({})", p.replace('.', "\\.").replace('*', ".*"));
        let indices = SYSTEM_INDICES
            .iter()
            .flat_map(|(p, alias)| [Some(*p), *alias])
            .flatten();
        // `SystemDataStreamDescriptor.getBackingIndexPattern`
        let backing = SYSTEM_DATA_STREAMS.map(|d| format!(".(migrated-){{0,}}[fd]s-{d}-*"));
        let names: Vec<String> = (indices.chain(SYSTEM_DATA_STREAMS))
            .map(regex)
            .chain(backing.iter().map(|b| regex(b)))
            .collect();
        es_query::Regexp::new(&names.join("|")).expect("valid system index patterns")
    });
