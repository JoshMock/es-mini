# Benchmark results

Elasticsearch 9.5.5 vs es-mini, each request timed with hyperfine (`curl` per run, so every time
includes a few ms of curl process start-up for both servers).

| Setting | Value |
|---|---|
| Date | 2026-10-29 |
| Commit | b680a96 |
| Command | `DOCS=100000 RUNS=100 CPUS=2 MEMORY=2g bench/run.sh` |
| Warmup | 5 runs |
| Hardware | Intel Core i7-1365U laptop (12 threads); each server in Docker with 2 CPUs, 2 GB; Elasticsearch heap 512 MB |

Times are mean ± σ in ms; speedup = Elasticsearch mean / es-mini mean (above 1 means es-mini is
faster). The last column is es-mini's mean in the previous run (2026-10-27, commit d0dd9c9).

| Request | Elasticsearch (ms) | es-mini (ms) | Speedup | es-mini 2026-10-27 (ms) |
|---|---|---|---|---|
| info | 10.13 ± 3.51 | 9.80 ± 3.24 | 1.03× | 7.87 |
| get_doc | 13.35 ± 4.99 | 11.61 ± 3.82 | 1.15× | 7.60 |
| match_all | 17.08 ± 4.71 | 12.14 ± 2.79 | 1.41× | 35.30 |
| match | 13.32 ± 3.92 | 14.02 ± 3.81 | 0.95× | 11.76 |
| term | 13.85 ± 4.69 | 18.99 ± 4.34 | 0.73× | 15.76 |
| range_sort | 13.53 ± 5.64 | 16.45 ± 4.15 | 0.82× | 19.61 |
| bool | 15.63 ± 3.93 | 11.09 ± 2.48 | 1.41× | 29.88 |
| count | 14.57 ± 3.56 | 9.62 ± 2.40 | 1.51× | 66.13 |
| esql | 28.39 ± 5.81 | 19.76 ± 4.40 | 1.44× | 232.11 |
| index_doc | 20.21 ± 4.48 | 18.84 ± 3.89 | 1.07× | 18.66 |
| terms_agg | 12.67 ± 3.79 | 9.74 ± 2.54 | 1.30× | 8.40 |
| date_histogram | 10.83 ± 2.51 | 10.16 ± 2.24 | 1.07× | 8.07 |
| range_agg | 12.95 ± 3.77 | 10.07 ± 2.43 | 1.29× | 8.70 |
| stats_agg | 13.66 ± 3.85 | 10.20 ± 1.87 | 1.34× | 8.61 |
| match_phrase | 11.14 ± 3.29 | 11.15 ± 2.85 | 1.00× | 9.28 |
| esql_stats | 22.99 ± 4.83 | 18.17 ± 4.88 | 1.27× | 198.51 |
| esql_eval | 22.52 ± 13.97 | 19.39 ± 4.21 | 1.16× | 198.19 |

## Where es-mini is slower

- `match`: es-mini 14.0 ms vs Elasticsearch 13.3 ms (0.95×)
- `term`: es-mini 19.0 ms vs Elasticsearch 13.9 ms (0.73×)
- `range_sort`: es-mini 16.5 ms vs Elasticsearch 13.5 ms (0.82×)
- `match_phrase`: es-mini 11.2 ms vs Elasticsearch 11.1 ms (1.00×)

## Sanity lines

Status and hit count matched between the two servers for all 17 requests. No differences.

## Notes

- ES|QL right after a write is about as fast as repeated queries: `ralph/esql-cold.sh` (a write,
  then each ES|QL request, median of 7, run locally outside Docker) gives esql 12.1 ms,
  esql_stats 18.7 ms and esql_eval 16.5 ms.
- Right after the bulk load, es-mini holds every document in memory (translog, no segments), so
  searches here take its in-memory path.
- es-mini answers HTTP 413 to a 17 MB `_bulk` body (its limit is 2 MB; Elasticsearch allows
  100 MB), so `bench/inside.sh` loads the data in 2,000-document chunks for both servers.
- Single run on a shared laptop (load average about 4-5); treat differences under ~15% as noise.
