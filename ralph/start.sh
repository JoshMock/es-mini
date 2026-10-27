#!/usr/bin/env bash
# Starts ralph/overnight.sh as the systemd user service `ralph`, detached from any terminal. systemd
# restarts it if anything kills it outright (SIGKILL leaves overnight.sh no chance to restart
# ralph.ts), and the journal records how each run ended. Arguments go to overnight.sh.
#   ralph/start.sh --suites goals,yaml,parity --until 07:00 --review-every 10
#   journalctl --user -u ralph -f      # follow output
#   systemctl --user stop ralph        # stop
set -euo pipefail
cd "$(dirname "$0")"
systemctl --user reset-failed ralph 2>/dev/null || true
exec systemd-run --user --unit ralph -p Restart=on-failure -p RestartSec=60 -p WorkingDirectory="$PWD" \
  -E PATH="$PATH" -E HOME="$HOME" \
  -- nono run --profile pi-es-mini -- env CARGO_HOME=/tmp/esm-cargo CARGO_TARGET_DIR=/tmp/esm-target CARGO_INCREMENTAL=0 ./overnight.sh "$@"
