# Benchmark results

Elasticsearch 9.5.5 vs es-mini, each request timed with hyperfine (`curl` per run, so every time
includes a few ms of curl process start-up for both servers).

| Setting | Value |
|---|---|
| Date | 2026-11-05 |
| Commit | da69a74 |
| Command | `DOCS=100000 RUNS=100 CPUS=2 MEMORY=2g bench/run.sh` |
| Warmup | 5 runs |
| Hardware | Intel Core i7-1365U laptop (12 threads); each server in Docker with 2 CPUs, 2 GB; Elasticsearch heap 512 MB |

Times are mean ± σ in ms; speedup = Elasticsearch mean / es-mini mean (above 1 means es-mini is
faster). The last column is es-mini's mean in the previous run (2026-10-29, commit b680a96).

| Request | Elasticsearch (ms) | es-mini (ms) | Speedup | es-mini 2026-10-29 (ms) |
|---|---|---|---|---|
| info | 9.37 ± 2.33 | 7.82 ± 1.63 | 1.20× | 9.80 |
| get_doc | 9.70 ± 2.38 | 7.95 ± 1.07 | 1.22× | 11.61 |
| match_all | 12.36 ± 2.84 | 10.17 ± 1.39 | 1.22× | 12.14 |
| match | 11.51 ± 2.32 | 11.91 ± 1.55 | 0.97× | 14.02 |
| term | 12.72 ± 2.74 | 8.70 ± 1.46 | 1.46× | 18.99 |
| range_sort | 12.28 ± 2.68 | 8.61 ± 2.07 | 1.43× | 16.45 |
| bool | 10.55 ± 1.73 | 9.69 ± 1.93 | 1.09× | 11.09 |
| count | 12.09 ± 3.75 | 7.76 ± 1.51 | 1.56× | 9.62 |
| esql | 20.26 ± 2.99 | 15.90 ± 1.86 | 1.27× | 19.76 |
| index_doc | 23.95 ± 3.47 | 21.39 ± 2.53 | 1.12× | 18.84 |
| terms_agg | 11.39 ± 2.70 | 8.34 ± 1.62 | 1.37× | 9.74 |
| date_histogram | 10.97 ± 2.39 | 7.92 ± 1.64 | 1.39× | 10.16 |
| range_agg | 10.94 ± 2.93 | 7.67 ± 1.58 | 1.43× | 10.07 |
| stats_agg | 11.41 ± 2.48 | 8.12 ± 1.42 | 1.41× | 10.20 |
| match_phrase | 8.93 ± 2.08 | 8.83 ± 1.42 | 1.01× | 11.15 |
| esql_stats | 23.23 ± 3.63 | 17.36 ± 2.99 | 1.34× | 18.17 |
| esql_eval | 20.89 ± 4.23 | 18.20 ± 2.88 | 1.15× | 19.39 |

## Where es-mini is slower

- `match`: es-mini 11.9 ms vs Elasticsearch 11.5 ms (0.97×), within noise

## Sanity lines

HTTP status matched between the two servers for all 17 requests.

## Notes

- ES|QL right after a write is about as fast as repeated queries: `ralph/esql-cold.sh` (a write,
  then each ES|QL request, median of 7, run locally outside Docker) gives esql 12.1 ms,
  esql_stats 18.7 ms and esql_eval 16.5 ms.
- Right after the bulk load, es-mini holds every document in memory (translog, no segments), so
  searches here take its in-memory path.
- es-mini answers HTTP 413 to a 17 MB `_bulk` body (its limit is 2 MB; Elasticsearch allows
  100 MB), so `bench/inside.sh` loads the data in 2,000-document chunks for both servers.
- Single run on a shared laptop (load average about 2.5-3.5); treat differences under ~15% as noise.
