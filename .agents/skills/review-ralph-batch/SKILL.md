---
name: review-ralph-batch
description: Review the commits a Ralph loop batch or unattended run made in es-mini before building on them. Use when the user says a batch, loop, or overnight run finished, or asks what's next after one.
---

# Review a Ralph batch

Ralph iterations each fix one target and pass `scripts/check.sh`, but passing checks has not
been enough: batches have shipped duplicated helpers, untested regressions, fake endpoints,
data-loss bugs, and goals marked done that were not. Review every batch before the next one.

## Safety Rules (Do Not Violate)

- Do not rewrite or revert agent commits without the user's approval; fix forward in new commits.
- Fix problems red-green (failing test first), one commit per fix, `scripts/check.sh` passing.
- Do not edit files while a Ralph loop runs in the same checkout: a rejected iteration runs
  `git reset --hard` and erases them. Check `git status` and ask if unsure.
- Decisions that change scope or behavior (keep/remove a feature, policy trade-offs) go to the
  user one at a time, with a recommendation.

## Suggested Workflow

1. Find the starting commit (the last one you reviewed, or the `started` line in
   `ralph/REPORT.md`), then run `.agents/skills/review-ralph-batch/scripts/batch_summary.sh <rev>`.
2. Stop and report if any of these show up: missing or generic trailers, protected paths
   touched, entries added to a list, or leftover `es-mini-*` containers.
3. For every goal line removed, confirm its done-criteria actually hold: find the code path the
   goal names and its test, and rerun the goal's `Done-check` yourself if it is cheap (Docker
   checks take about 3 minutes).
4. Read the diffs of the largest and most cross-cutting commits (see the checklist), not only
   their messages.
5. Fix what does not need a decision, then present the rest as questions.

## Checklist (each item has happened at least once)

- **Duplication**: new parsing/resolution logic instead of the shared helpers
  (`es_index::aliases::{resolve_read, resolve_data, resolve_write}`, `indices::read_indices`,
  `es_index::flatten_settings`, `tests/common/parity.rs`). Grep for the helper's job.
- **Untested regressions**: a change to shared types or data (mappings, `Index`) breaking a path
  no test covers. Example: dynamic `TextKeyword` mappings hid `.keyword` columns from ES|QL.
- **Fake behavior**: endpoints that acknowledge without doing anything (`_close` once did).
- **Durability and memory**: deleting before writing, failures logged but answered with success,
  saves under a read lock, data kept forever (superseded sources, tombstones).
- **Silent omissions**: code that skips what it cannot handle without an error (the segment
  writer once dropped any field with more than 48 terms, so Elasticsearch found nothing).
- **Premature "done"**: a goal removed when only part of it exists (task 19 had a codec writer
  but no server code calling it).
- **Scope**: features `z_old/PLAN.md` excludes, or edits to `z_old/`, vendored tests, runners,
  fixtures, or the out-of-scope list.
- **Atomicity**: several behaviors in one commit, or meaningless messages (`tmp`).
- **Stale `ponytail:` comments** that no longer describe the code.

## Notes

- `ralph/NEEDS_HUMAN.md` is meant for decisions; agents also use it as a progress log. Read it
  for blockers either way.
- The loop's prompt lives in `ralph/ralph.ts` (`COMMON_RULES`). When a problem repeats across
  batches, add a rule there in addition to fixing the code.
