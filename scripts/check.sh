#!/usr/bin/env bash
# Deterministic guardrails. Run before every commit; CI runs the same script.
# Low-CPU by default: JOBS=2. Override with JOBS=N.
set -euo pipefail
cd "$(dirname "$0")/.."
jobs="${JOBS:-2}"
step() { echo "==> $*"; nice -n 19 "$@"; }
step cargo fmt --all --check
step cargo clippy -j "$jobs" --workspace --all-targets --locked -- -D warnings
step cargo test -j "$jobs" --workspace --locked
RUSTDOCFLAGS="-D warnings" step cargo doc -j "$jobs" --workspace --no-deps --locked
