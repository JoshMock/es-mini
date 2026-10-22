# Benchmarks: Elasticsearch 9.5.5 vs es-mini

```sh
bench/run.sh                          # 10000 docs, 50 runs per request, 1 CPU / 1 GB each
DOCS=100000 RUNS=100 CPUS=2 MEMORY=2g bench/run.sh
```

Needs Docker and `hyperfine` on the host. `run.sh` builds es-mini in release mode, starts both
servers with the same CPU and memory limits on a private Docker network (es-mini persisting to
a data directory, like Elasticsearch), loads the same generated dataset (`gen_data.py`) into
each, and benchmarks every request in `requests.txt` with hyperfine from a third container on
that network. Before timing a request it prints each server's status code and hit count, so
answers that differ show up next to the numbers.

Results go to `bench/results/<timestamp>/` (git-ignored): `summary.md` plus hyperfine's
markdown and JSON per request. Add requests by appending `name|METHOD|path|json-body` lines to
`requests.txt`.

Each sample includes starting `curl` (a few ms, the same for both servers), so small latencies
compare less sharply than the raw numbers suggest.
