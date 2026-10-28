#!/usr/bin/env bash
# A/B benchmark of es-mini: the working tree's build against <base-rev>'s, in one bench/run.sh
# run (same machine state, no Elasticsearch). Passes only if the working tree is faster: the
# geometric mean of the requests' median-time ratios is at most MIN_GAIN (default 0.95), and no
# request is slower than 1.10x. Identical builds measure up to 10% apart per request and 1.5%
# overall. With MAX_MS set, it instead passes if every request's median is at most MAX_MS on
# the working tree's build (and none is slower than 1.10x).
#   bench/ab.sh <base-rev>        # ONLY=esql DOCS=100000 RUNS=30 CPUS=2 MEMORY=2g by default
set -euo pipefail
cd "$(dirname "$0")/.."
base=$(git rev-parse "${1:?usage: bench/ab.sh <base-rev>}")
export ONLY=${ONLY:-esql} DOCS=${DOCS:-100000} RUNS=${RUNS:-30} CPUS=${CPUS:-2} MEMORY=${MEMORY:-2g}
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
out=$(mktemp -d /tmp/esm-ab-out.XXXXXX)
AB_BIN=$cache/es-mini OUT=$out bench/run.sh >&2
python3 - "$out" "${MIN_GAIN:-0.95}" "${MAX_MS:-}" <<'PY'
import glob, json, math, sys
out, min_gain, max_ms = sys.argv[1], float(sys.argv[2]), sys.argv[3]
ratios, ok = [], True
for f in sorted(glob.glob(f"{out}/*.json")):
    m = {r["name"]: r["summary"]["time_wall_clock"]["median"] * 1000 for r in json.load(open(f))["results"]}
    base, new = m["es-mini-base"], m["es-mini"]
    ratios.append(new / base)
    slow = new / base > 1.10 or (max_ms and new > float(max_ms))
    ok &= not slow
    print(f"{f.split('/')[-1][:-5]:12} base {base:8.1f} ms  new {new:8.1f} ms  x{new / base:.3f}{'  FAIL' if slow else ''}")
if not ratios:
    sys.exit("no requests matched ONLY")
gm = math.exp(sum(map(math.log, ratios)) / len(ratios))
print(f"geometric mean x{gm:.3f}")
sys.exit(0 if ok and (max_ms or gm <= min_gain) else 1)
PY
