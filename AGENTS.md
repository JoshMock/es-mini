# Agent instructions

## Interactive question loops

When asked to ask questions, or to otherwise get feedback, one point at a time and wait for responses:

- Ask ONE question, then STOP. Do not proceed to the next question.
- NEVER assume, invent, or narrate a user response. Do not write "user agreed" or fabricate an answer.
- Wait for the user's actual reply before updating any file or moving on.
- A recommendation or default is a suggestion, not consent. Only apply it after the user explicitly confirms.

## Verification

Run `scripts/check.sh` before every commit; it must pass. It runs `cargo fmt --check`,
`clippy -D warnings` (lints configured in root `Cargo.toml`), tests, and `cargo doc -D warnings`.
Do not weaken lints or add `#[allow(...)]` to get it passing without stating why.

## Environment

- The sandbox makes `~/.cargo` read-only. Run cargo with `CARGO_HOME=/tmp/esm-cargo` and share
  one `CARGO_TARGET_DIR=/tmp/esm-target` across checkouts and sessions, so concurrent builds
  queue instead of compiling twice. CPU is limited: keep `-j 2`, under `nice`.
- Java cannot start in the sandbox, and Docker's published ports are unreachable; plain
  localhost connections to a process you started do work.

## Ground truth

- Elasticsearch 9.5.5 and Lucene 10.5.1 (COMPATIBILITY.md) define correct behavior and bytes.
  Port from their source at those versions (skill: port-lucene-format), not from memory.
- `fixtures/expected/*.json` are real Elasticsearch responses to the requests in
  `fixtures/generate.sh`; the parity tests compare against them. Check against a live
  Elasticsearch when they do not cover a case (skill: verify-with-elasticsearch).

## Codebase map

- `es-codec`: Lucene/Elasticsearch on-disk formats, read and write. `es-index`: index model,
  settings, mappings, analysis, alias and expression resolution, and `disk` (open/save a data
  directory). `es-query`: query DSL and ES|QL over `es-index`. `es-mini`: the axum server.
- Reuse the shared helpers instead of re-implementing them: index expressions resolve through
  `es_index::aliases` (`resolve_read` for metadata, `resolve_data` for document reads and
  searches, which refuses closed indices, and `resolve_write`), and `indices::read_indices`
  adds `ignore_unavailable`/`allow_no_indices`/`expand_wildcards`.
- Known-failure lists: `tests/yaml-xfail.txt` and `fixtures/parity-xfail.txt` must only shrink
  (a listed test that passes fails the run); `tests/yaml-out-of-scope.txt` changes only by a
  human decision.

## Durability

Document writes go through `es_index::disk::persist` as translog appends (fsynced append plus an
in-place checkpoint); `disk::open` replays the translog. Segments are committed on flush or on a
metadata change. `es_index::disk::save` must never leave a data directory half-written. Segment
commits happen in place with Lucene's protocol (write-once files under new names, fsynced, then an
atomically renamed `segments_N`; unreferenced files are deleted after); saves that change
cluster metadata build the snapshot beside the directory and swap it in (`disk::recover`
completes interrupted swaps). Saves are serialized, and a failed save fails the request. Keep those properties in any change to
persistence, and never let a writer panic or silently drop data it cannot encode.

## Reviewing agent-made changes

After any Ralph batch or unattended run, review its commits before building on them (skill:
review-ralph-batch). Passing `scripts/check.sh` has not been enough to catch duplicated
helpers, untested regressions, fake endpoints, or goals marked done early.

## Unattended runs

These standing orders apply when no human is watching (Ralph loop iterations, overnight runs).
Record anything they reserve for a human in `ralph/NEEDS_HUMAN.md` (git-ignored; append a dated
entry with the target and the decision needed), then stop or move on. Never wait for a reply.

- Do not move tests to an out-of-scope list, edit vendored tests or their runners, or change
  `z_old/PLAN.md` / `z_old/EXECUTION.md` scope. Escalate instead.
- Do not add crate dependencies. Escalate with the reason one is needed.
- Do not rewrite history (rebase, amend, reword, or reset commits you did not make this
  iteration). Never push.
- Docker is allowed only for Elasticsearch 9.5.5 verification: one container at a time, capped
  at 1 CPU and 1 GB, requests via `docker exec` (the sandbox cannot reach ports Docker
  publishes), and always removed afterwards (`docker rm -f`), even on failure.
- Keep CPU low: cargo at `-j 2` (scripts/check.sh's default) under `nice`. Release builds and
  `bench/run.sh` are allowed only for goals about performance: run the benchmark before and
  after the change and put both numbers in the commit message.
- Never regenerate `fixtures/es-9.5.5/` or `fixtures/expected/`; other tests depend on their bytes.
