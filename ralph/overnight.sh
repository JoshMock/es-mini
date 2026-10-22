#!/usr/bin/env bash
# Keeps ralph.ts running until its deadline: restarts it after a crash (ralph.ts rolls back the
# interrupted iteration on start), and stops when it ends on purpose (exit code 3), after 5
# crashes in a row, or at the deadline. Arguments go to ralph.ts and must include --until HH:MM.
#   ralph/overnight.sh --suites goals,yaml,parity --until 07:00 --review-every 10
set -uo pipefail
cd "$(dirname "$0")"
notify() {
  curl -s -o /dev/null -H "Authorization: Bearer $(pass agents/ntfy/pi)" -H "Title: $1" -d "$2" \
    https://ntfy.sh/joshmock-pi-agent-events || true
}
until_hhmm=$(printf '%s\n' "$@" | grep -A1 -x -- '--until' | tail -1)
[ -n "$until_hhmm" ] || { echo "pass --until HH:MM"; exit 2; }
deadline=$(date -d "$until_hhmm" +%s); [ "$deadline" -gt "$(date +%s)" ] || deadline=$((deadline + 86400))
crashes=0
while [ "$(date +%s)" -lt "$deadline" ]; do
  node ralph.ts "$@"
  code=$?
  [ "$code" -eq 3 ] && exit 0
  crashes=$((crashes + 1))
  echo "- $(date -Is) overnight: ralph.ts exited with $code (crash $crashes in a row); restarting" >> REPORT.md
  if [ "$crashes" -ge 5 ]; then
    notify "ralph: gave up" "ralph.ts crashed 5 times in a row; see ralph/REPORT.md."
    exit 1
  fi
  notify "ralph: restarting" "ralph.ts exited with code $code; restart $crashes of 5."
  sleep 60
done
