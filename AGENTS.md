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
