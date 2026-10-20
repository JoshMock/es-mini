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
  at 1 CPU and 1 GB, requests via `docker exec` (the sandbox blocks localhost TCP), and always
  removed afterwards (`docker rm -f`), even on failure.
- Keep CPU low: cargo at `-j 2` (scripts/check.sh's default) under `nice`; no benchmarks or
  release builds.
- Never regenerate `fixtures/es-9.5.5/` or `fixtures/expected/`; other tests depend on their bytes.
