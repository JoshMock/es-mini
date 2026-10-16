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

Suites: `yaml` (tests/yaml-xfail.txt). Later phases add suites to `SUITES` in `ralph.ts`.
