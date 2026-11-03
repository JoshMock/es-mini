# Compatibility

| Component | Pinned version | Git ref |
|---|---|---|
| Elasticsearch | 9.5.5 | [`v9.5.5`](https://github.com/elastic/elasticsearch/tree/v9.5.5) |
| Lucene (codec) | 10.5.1 | from `build-tools-internal/version.properties` at `v9.5.5` |
| REST API spec / YAML tests | 9.5.5 | `elasticsearch` repo `v9.5.5`, `rest-api-spec/` |
| elasticsearch-specification | 9.5 | branch `9.5` @ `84bf510a907bae62ffe29af35f516af287a79eab` |

## Support policy

es-mini supports **only** Elasticsearch 9.5.5 on-disk data and its Lucene 10 codec.
On-disk data written by any other major version or Lucene codec is rejected with a clear
error. No other versions are supported.

Indices must also have been created by 9.5.5: es-mini routes documents to shards with the function
Elasticsearch uses for indices created on or after `IndexVersions.MOD_ROUTING_FUNCTION`
(`floorMod` of the murmur3 hash). Indices created by older versions route with
`routing_num_shards`, which es-mini does not port, so their documents would be looked up in the
wrong shards.
