#!/usr/bin/env bash
# A/B benchmark of es-mini: the working tree's build against <base-rev>'s, in one bench/run.sh
# run (no Elasticsearch). Each request is timed in PASSES passes that alternate which build goes
# first, with both servers and the client pinned to matching cores, and samples are pooled. Passes
# only if the working tree is faster: the geometric mean of the requests' median ratios is at most
# MIN_GAIN (default 0.95), and no request is slower (above 1.10x and 1.5 ms). Identical builds
# measure x0.99-1.04 overall, so a no-regression gate needs MIN_GAIN=1.05. With MAX_MS set, it
# instead passes if every request's median is at most MAX_MS on the working tree's build (and none
# is slower).
#   bench/ab.sh <base-rev>        # ONLY=esql DOCS=100000 RUNS=20 PASSES=4 TRIES=2 CPUS=2 MEMORY=2g
set -euo pipefail
cd "$(dirname "$0")/.."
base=$(git rev-parse "${1:?usage: bench/ab.sh <base-rev>}")
# Matching E-cores of this laptop's i7-1365U (lscpu -e); override for other machines.
export PIN_A=${PIN_A-4,5} PIN_B=${PIN_B-6,7} PIN_CLIENT=${PIN_CLIENT-8-11}
export ONLY=${ONLY:-esql} DOCS=${DOCS:-100000} RUNS=${RUNS:-20} PASSES=${PASSES:-4} CPUS=${CPUS:-2} MEMORY=${MEMORY:-2g}
# Base builds are cached by commit: an accepted change's HEAD is the next iteration's base.
cache=/tmp/esm-ab-bins/$base
if [ ! -x "$cache/es-mini" ]; then
  wt=$(mktemp -d /tmp/esm-ab-wt.XXXXXX)
  git worktree add -q --detach "$wt" "$base"
  (cd "$wt" && nice cargo build -q --release -j "${JOBS:-2}" -p es-mini)
  target=$(cd "$wt" && cargo metadata --format-version 1 --no-deps | python3 -c 'import json,sys;print(json.load(sys.stdin)["target_directory"])')
  mkdir -p "$cache" && cp "$target/release/es-mini" "$cache/"
  git worktree remove --force "$wt"
fi
judge() {  # results dir: prints the table, fails unless the working tree passes
python3 - "$1" "${MIN_GAIN:-0.95}" "${MAX_MS:-}" <<'PY'
import glob, json, math, sys
out, min_gain, max_ms = sys.argv[1], float(sys.argv[2]), sys.argv[3]
import re, statistics
times = {}  # request -> server -> every sample of every pass
for f in glob.glob(f"{out}/*.json"):
    name = re.sub(r"(\.p\d+)?\.json$", "", f.split("/")[-1])
    for r in json.load(open(f))["results"]:
        samples = [m["time_wall_clock"]["value"] for m in r["measurements"]]
        times.setdefault(name, {}).setdefault(r["name"], []).extend(samples)
ratios, ok = [], True
for name in sorted(times):
    m = {s: statistics.median(t) * 1000 for s, t in times[name].items()}
    base, new = m["es-mini-base"], m["es-mini"]
    ratios.append(new / base)
    # A few-ms request jitters by a ms or so, so slower also means by more than 1.5 ms.
    slow = (new / base > 1.10 and new - base > 1.5) or (max_ms and new > float(max_ms))
    ok &= not slow
    print(f"{name:12} base {base:8.1f} ms  new {new:8.1f} ms  x{new / base:.3f}{'  FAIL' if slow else ''}")
if not ratios:
    sys.exit("no requests matched ONLY")
gm = math.exp(sum(map(math.log, ratios)) / len(ratios))
print(f"geometric mean x{gm:.3f}")
sys.exit(0 if ok and (max_ms or gm <= min_gain) else 1)
PY
}
# Noise fails about one identical-build comparison in three, each time on a different request, so
# a failed comparison is benchmarked again (TRIES, default 2) and passes if any try does.
for try in $(seq "${TRIES:-2}"); do
  out=$(mktemp -d /tmp/esm-ab-out.XXXXXX)
  AB_BIN=$cache/es-mini OUT=$out bench/run.sh >&2
  judge "$out" && exit 0
  echo "try $try failed" >&2
done
exit 1
