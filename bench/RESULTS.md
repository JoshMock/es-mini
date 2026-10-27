# Benchmark results

Elasticsearch 9.5.5 vs es-mini, each request timed with hyperfine (`curl` per run, so every time
includes a few ms of curl process start-up for both servers).

| Setting | Value |
|---|---|
| Date | 2026-10-27 |
| Commit | d0dd9c9 (plus the bulk-chunking change in `bench/inside.sh`) |
| Command | `DOCS=100000 RUNS=100 CPUS=2 MEMORY=2g bench/run.sh` |
| Warmup | 5 runs |
| Hardware | Intel Core i7-1365U laptop (12 threads); each server in Docker with 2 CPUs, 2 GB; Elasticsearch heap 512 MB |

Times are mean ± σ in ms; speedup = Elasticsearch mean / es-mini mean (above 1 means es-mini is faster).

| Request | Elasticsearch (ms) | es-mini (ms) | Speedup |
|---|---|---|---|
| info | 8.52 ± 1.93 | 7.87 ± 1.46 | 1.08× |
| get_doc | 10.43 ± 2.22 | 7.60 ± 1.33 | 1.37× |
| match_all | 12.39 ± 2.78 | 35.30 ± 6.42 | 0.35× |
| match | 12.66 ± 2.68 | 11.76 ± 1.62 | 1.08× |
| term | 12.04 ± 3.51 | 15.76 ± 1.38 | 0.76× |
| range_sort | 11.92 ± 2.62 | 19.61 ± 2.31 | 0.61× |
| bool | 12.08 ± 2.30 | 29.88 ± 4.77 | 0.40× |
| count | 11.68 ± 2.27 | 66.13 ± 7.88 | 0.18× |
| esql | 18.70 ± 3.54 | 232.11 ± 18.43 | 0.08× |
| index_doc | 22.33 ± 3.08 | 18.66 ± 2.69 | 1.20× |
| terms_agg | 11.09 ± 2.33 | 8.40 ± 0.90 | 1.32× |
| date_histogram | 11.40 ± 3.58 | 8.07 ± 1.03 | 1.41× |
| range_agg | 11.99 ± 1.73 | 8.70 ± 1.18 | 1.38× |
| stats_agg | 11.05 ± 2.12 | 8.61 ± 0.89 | 1.28× |
| match_phrase | 10.23 ± 2.11 | 9.28 ± 1.82 | 1.10× |
| esql_stats | 23.57 ± 5.43 | 198.51 ± 16.80 | 0.12× |
| esql_eval | 21.57 ± 14.96 | 198.19 ± 14.06 | 0.11× |

## Where es-mini is slower

- `match_all`: es-mini 35.3 ms vs Elasticsearch 12.4 ms (0.35×)
- `term`: es-mini 15.8 ms vs Elasticsearch 12.0 ms (0.76×)
- `range_sort`: es-mini 19.6 ms vs Elasticsearch 11.9 ms (0.61×)
- `bool`: es-mini 29.9 ms vs Elasticsearch 12.1 ms (0.40×)
- `count`: es-mini 66.1 ms vs Elasticsearch 11.7 ms (0.18×)
- `esql`: es-mini 232.1 ms vs Elasticsearch 18.7 ms (0.08×)
- `esql_stats`: es-mini 198.5 ms vs Elasticsearch 23.6 ms (0.12×)
- `esql_eval`: es-mini 198.2 ms vs Elasticsearch 21.6 ms (0.11×)

## Sanity lines

Status and hit count matched between the two servers for all 17 requests. No differences.

## Notes

- es-mini answers HTTP 413 to the 17 MB `_bulk` body (its body limit is 2 MB; Elasticsearch allows
  100 MB), so `bench/inside.sh` now loads the data in 2,000-document chunks for both servers.
- Single run on a shared laptop; treat small differences as noise.
