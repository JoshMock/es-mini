#!/usr/bin/env python3
"""Vendor in-scope Elasticsearch YAML REST tests into tests/yaml/, verbatim.

Downloads rest-api-spec tests at the pinned tag (see COMPATIBILITY.md) with `gh`, keeps files
that pass the out-of-scope feature scan below (plus a short allowlist of core API files whose
out-of-scope sections the runner's xfail list covers), and writes tests/yaml/INCLUDED.md.
Never edit vendored files; rerun this script instead.
"""
import json, re, subprocess, urllib.error, urllib.request
from pathlib import Path

TAG = "v9.5.5"
API = "repos/elastic/elasticsearch/contents/rest-api-spec/src/yamlRestTest/resources/rest-api-spec/test"
DIRS = ["bulk", "cluster.health", "count", "create", "delete", "exists", "get", "get_source", "index",
        "indices.create", "indices.delete", "indices.exists", "indices.get", "indices.get_mapping",
        "indices.get_settings", "indices.put_settings", "search", "update",
        "indices.put_alias", "indices.delete_alias", "indices.get_alias", "indices.exists_alias",
        "indices.update_aliases"]
# Core files kept despite matching a rule; their out-of-scope sections are xfailed per test.
ALLOW = {"bulk/10_basic.yml", "cluster.health/10_basic.yml", "count/10_basic.yml", "exists/10_basic.yml",
         "indices.create/10_basic.yml", "indices.delete/10_basic.yml", "indices.exists/10_basic.yml",
         "indices.get/10_basic.yml", "indices.get_settings/10_basic.yml", "indices.put_settings/10_basic.yml",
         "search/20_default_values.yml", "update/10_doc.yml"}
OUT_OF_SCOPE = {
    "highlighting": r"\bhighlight\b",
    "suggesters": r"\bsuggest\b",
    "scripting": r"\bscript\b|\bpainless\b",
    "nested/join fields": r"type: *(nested|join)\b|has_child|has_parent",
    "runtime fields": r"runtime_mappings|\bruntime\b",
    "ingest pipelines": r"\bpipeline\b",
    "multi-shard/routing": r"\brouting\b|number_of_shards: *[2-9]|_routing",
    "scroll/PIT/async": r"\bscroll\b|point_in_time|\bpit\b|search_after|_async",
    "vectors": r"dense_vector|sparse_vector|\bknn\b|semantic",
    "geo": r"\bgeo_",
    "remote clusters": r"remote|cluster:",
    "out-of-scope APIs": r"\bmget\b|\bmsearch\b|\bcat\.|reindex|snapshot|indices\.(open|close|stats|forcemerge|flush|rollover|shrink|split|clone|put_mapping|put_template|put_index_template|simulate|resolve)|cluster\.(put_settings|state|stats|allocation)|nodes\.|tasks\.|termvectors|\bexplain\b|field_caps|_by_query",
    "out-of-scope queries": r"\b(query_string|simple_query_string|multi_match|match_phrase|match_phrase_prefix|match_bool_prefix|prefix|wildcard|regexp|fuzzy|exists|ids|terms|function_score|constant_score|dis_max|span_\w+|combined_fields|intervals|rank_feature|more_like_this|distance_feature)\b\"?\s*:",
    "out-of-scope search options": r"\b(collapse|rescore|_source_includes|_source_excludes|stored_fields|docvalue_fields|post_filter|min_score|search_type|indices_boost|profile|terminate_after|seq_no_primary_term|retriever)\b\"?\s*:",
    "external versioning / CAS": r"\bversion_type\b|if_seq_no|if_primary_term",
    "out-of-scope field types": r"type: *\"?(object|flattened|ip|geo_point|geo_shape|wildcard|match_only_text|constant_keyword|unsigned_long|half_float|scaled_float|binary|\w+_range|date_nanos|completion|search_as_you_type|token_count|alias|version|histogram|aggregate_metric_double|counted_keyword|rank_features?|percolator|shape|point)\b",
    "index modes / synthetic source": r"index\.mode|time_series|logsdb|synthetic",
    "analysis config": r"\banalyzer\b|\btokenizer\b|char_filter|normalizer|dynamic_templates",
    "data streams": r"data_stream",
    "merge settings": r"merge\.scheduler",
}


def gh(path):
    return json.loads(subprocess.check_output(["gh", "api", f"{path}?ref={TAG}"]))


# In-scope aggregation suites live in the aggregations module, vendored as aggregations/<file>.
AGG_API = "repos/elastic/elasticsearch/contents/modules/aggregations/src/yamlRestTest/resources/rest-api-spec/test/aggregations"
AGG_FILES = ["avg_metric.yml", "sum_metric.yml", "min_metric.yml", "max_metric.yml", "value_count_metric.yml",
             "stats_metric.yml", "extended_stats_metric.yml", "cardinality_metric.yml", "percentiles_tdigest_metric.yml",
             "top_hits.yml", "terms.yml", "histogram.yml", "date_histogram.yml", "range.yml", "date_range.yml",
             "filter.yml", "filters_bucket.yml", "missing.yml", "global.yml", "typed_keys.yml"]

root = Path(__file__).parent.parent / "tests/yaml"
included, excluded = [], []
for d in DIRS:
    for entry in gh(f"{API}/{d}"):
        if entry["type"] != "file" or not entry["name"].endswith(".yml"):
            continue
        name = f"{d}/{entry['name']}"
        text = urllib.request.urlopen(entry["download_url"]).read()
        reasons = [k for k, p in OUT_OF_SCOPE.items() if re.search(p, text.decode())]
        if reasons and name not in ALLOW:
            excluded.append((name, reasons))
            continue
        out = root / name
        out.parent.mkdir(parents=True, exist_ok=True)
        out.write_bytes(text)
        included.append((name, reasons))

for entry in gh(AGG_API):
    if entry["type"] != "file" or entry["name"] not in AGG_FILES:
        continue
    name = f"aggregations/{entry['name']}"
    text = urllib.request.urlopen(entry["download_url"]).read()
    reasons = [k for k, p in OUT_OF_SCOPE.items() if re.search(p, text.decode())]
    out = root / name
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_bytes(text)
    included.append((name, reasons))

lines = [f"# Vendored YAML REST tests", "",
         f"Source: `elastic/elasticsearch` tag `{TAG}`, `rest-api-spec/src/yamlRestTest/resources/rest-api-spec/test/`.",
         "Files are verbatim; regenerate with `scripts/vendor_yaml_tests.py`. Never edit them.", "",
         f"## Included ({len(included)})", ""]
lines += [f"- `{n}`" + (f" (allowlisted core file; also touches: {', '.join(r)})" if r else "") for n, r in included]
lines += ["", f"## Excluded ({len(excluded)})", "",
          "Excluded files exercise features outside the MVP scope (see z_old/PLAN.md).", ""]
lines += [f"- `{n}`: {', '.join(r)}" for n, r in excluded]
# API specs map `do: {api: {...}}` steps to HTTP method and path.
apis = set()
for name, _ in included:
    text = (root / name).read_text()
    apis |= set(re.findall(r"^\s+(?:- )?do:\s*\n\s+(?:(?:catch|headers|warnings|allowed_warnings|node_selector)\b.*\n(?:\s{8,}.*\n)*\s+)*([a-z_.]+):", text, re.M))
spec_dir = root / "api"
spec_dir.mkdir(exist_ok=True)
SPEC_URL = f"https://raw.githubusercontent.com/elastic/elasticsearch/{TAG}/rest-api-spec/src/main/resources/rest-api-spec/api"
for api in sorted(apis.copy()):
    try:
        (spec_dir / f"{api}.json").write_bytes(urllib.request.urlopen(f"{SPEC_URL}/{api}.json").read())
    except urllib.error.HTTPError as e:
        if e.code != 404:
            raise
        apis.discard(api)  # the scan also catches non-API keys under `do:`
lines += ["", f"## API specs ({len(apis)})", "", "`api/` holds the matching `rest-api-spec/api/*.json` files: " + ", ".join(sorted(apis)) + "."]
(root / "INCLUDED.md").write_text("\n".join(lines) + "\n")
print(f"included {len(included)}, excluded {len(excluded)}")
