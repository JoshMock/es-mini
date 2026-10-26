#!/usr/bin/env bash
# Writes es-codec/tests/data/zstd/<case>.zst with the zstd CLI. es-codec/tests/zstd.rs rebuilds
# each case's input with the same generator and checks the decoder returns it.
set -euo pipefail
cd "$(dirname "$0")"
out=../data/zstd
rm -rf "$out"; mkdir -p "$out"
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
python3 - "$tmp" <<'PY'
import sys
WORDS = ("alpha bravo charlie delta echo foxtrot golf hotel india juliet kilo lima mike "
         "november oscar papa quebec romeo sierra tango uniform victor whiskey xray yankee "
         "zulu red orange yellow green blue indigo violet black white grey brown pink cyan "
         "magenta").split()
def lcg(seed):
    x = seed
    while True:
        x = (x * 6364136223846793005 + 1442695040888963407) % 2**64
        yield x >> 33
def words(n, seed):
    r, s = lcg(seed), ""
    while len(s) < n:
        s += WORDS[next(r) % 40] + " "
    return s[:n].encode()
def lines(n, seed):
    r, s = lcg(seed), ""
    while len(s) < n:
        v = next(r)
        s += WORDS[v % 40] + (" " if v % 9 else "\n" + "-" * (v % 50) + "\n")
    return s[:n].encode()
def noise(n, seed, modulo):
    r = lcg(seed)
    return bytes(next(r) % modulo for _ in range(n))
cases = {
    "empty": b"", "hello": b"hello world", "rle": b"a" * 5000,
    "words-small": words(1000, 1), "words": words(160_000, 2),
    "lines": lines(280_000, 5),
    "noise": noise(5000, 3, 256), "nibbles": noise(20_000, 4, 16),
}
for name, data in cases.items():
    open(f"{sys.argv[1]}/{name}", "wb").write(data)
PY
z() { zstd -q -f "$@"; }
z -3 --no-check "$tmp/empty" -o "$out/empty.zst"
z -3 --no-check "$tmp/hello" -o "$out/hello.zst"
z -3 --no-check "$tmp/rle" -o "$out/rle.zst"
z -1 --no-check "$tmp/words-small" -o "$out/words-small.zst"
z -3 --no-check "$tmp/words" -o "$out/words-3.zst"
z -19 --check "$tmp/words" -o "$out/words-19.zst"
z -1 --no-check "$tmp/lines" -o "$out/lines.zst"
z -3 --no-check "$tmp/noise" -o "$out/noise.zst"
z -19 --no-check "$tmp/nibbles" -o "$out/nibbles.zst"
ls -l "$out"
