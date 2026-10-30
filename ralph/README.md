# Ralph loop

Each iteration starts a fresh pi agent session with one known-failing test, then independently
verifies the result: clean tree, a new commit, no edits to vendored tests, the target removed
from the xfail list with nothing added, and `scripts/check.sh` passing. Rejected iterations are
reset to the starting commit and the test is skipped for the rest of the run.

```sh
cd ralph && npm install
node ralph.ts --max 5            # fix up to 5 YAML tests, in xfail-list order
node ralph.ts --target 'bulk/10_basic.yml/Array of objects'
node ralph.ts --dry-run          # show the next target and prompt; no agent
```

Suites: `yaml` (tests/yaml-xfail.txt), `parity` (fixtures/parity-xfail.txt), and `goals`
(ralph/goals.txt, ordered multi-iteration tasks).

## Overnight

```sh
node ralph.ts --suites yaml,goals,parity --until 07:00 --review-every 10
```

`--until` also takes a weekday (`--until 'mon 07:00'`) for runs longer than a day; `ralph/start.sh`
runs this detached under systemd. Each suite ends after 3 rejections in a row; a review iteration runs after every 10 accepted
ones. Progress goes to `ralph/REPORT.md`, escalations to `ralph/NEEDS_HUMAN.md` (both
git-ignored), and a notification is sent at the end. Standing orders are in AGENTS.md.
