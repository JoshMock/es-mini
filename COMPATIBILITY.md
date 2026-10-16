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
