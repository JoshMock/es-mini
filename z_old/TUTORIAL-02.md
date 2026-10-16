# Tutorial 02 — CI, Formatting, and Linting

**Series:** es-mini — Building Elasticsearch in Rust  
**Phase:** 0, Task 2  
**Prerequisites:** Tutorial 01 complete — workspace exists, all four crates created, `rust-toolchain.toml` in repo root

---

## What You'll Build

By the end of this tutorial you'll have:

- `rustfmt` configured at the workspace level with a `rustfmt.toml`
- `clippy` enforced with `#![deny(clippy::all)]` in each crate
- A GitHub Actions workflow that runs `fmt --check`, `clippy`, and `cargo test` on every push
- A trivial test in `es-query` proving the CI pipeline is end-to-end green
- `cargo deny` configured for supply-chain auditing (optional but worth five minutes)
- Release profile tuning in the root `Cargo.toml`

None of this is glamorous. It's also the last line of defense before you're debugging a bug that a linter would have caught in 200ms.

---

## 1. `rustfmt` — The Formatter

### The JS/Python analogy

`rustfmt` is Prettier or Black for Rust. It's opinionated, not configurable in any deep sense, and the community doesn't argue about tabs vs. spaces. That debate ended in 2015.

The key commands:

```bash
cargo fmt              # format everything in-place (run this locally)
cargo fmt --check      # exit non-zero if anything would change (run this in CI)
```

`--check` doesn't modify files. It's the CI-safe version: it tells you formatting is wrong without silently fixing it and leaving your CI state inconsistent with the working tree.

### `rustfmt.toml`

Create this at the workspace root:

```toml
# rustfmt.toml
edition = "2021"
max_width = 100
```

That's enough. `edition` must match your crates (you set `edition = "2021"` in Tutorial 01). `max_width = 100` is a mild relaxation from the 80-char default — 100 is where most teams land for modern monitors.

> **Why not configure more?** `rustfmt` has ~50 options. Most are unstable (nightly-only) and many actively harm readability when you deviate from convention. The Rust ecosystem expects standard formatting. Reviewers reading your code on GitHub, in docs.rs, in crates they copy from — everything looks the same. That consistency is the value.

Run `cargo fmt` now and commit the result. You want a clean baseline before CI enforces anything.

---

## 2. `clippy` — The Linter

### The JS/Python analogy

`clippy` is ESLint + flake8 + pylint combined, but written by people who deeply understand the language semantics. It doesn't just catch style — it catches logical bugs, unnecessary allocations, and patterns that compile but are almost certainly wrong.

```bash
cargo clippy --workspace --all-targets
```

- `--workspace`: check all crates, not just the current directory
- `--all-targets`: includes tests, examples, and benchmarks — not just `lib` and `bin`

### Lint levels

Clippy lints have four levels, same as rustc itself:

| Level | Meaning |
|-------|---------|
| `allow` | Silence this lint entirely |
| `warn` | Emit a warning, don't fail compilation |
| `deny` | Treat this lint as a hard error |
| `forbid` | Like `deny` but you can't `allow` it locally |

The default when you run `cargo clippy` with no flags is `warn` — it reports issues but doesn't fail. In CI you want failure. Two ways to do this:

**Option A — pass a flag at the command line:**
```bash
cargo clippy --workspace --all-targets -- -D warnings
```
The `--` separates cargo flags from flags passed through to the underlying compiler. `-D warnings` means "deny all warnings", which includes clippy's `warn`-level lints.

**Option B — set it in source code with an inner attribute:**
```rust
#![deny(clippy::all)]
```

Put this at the top of each crate's root file (`lib.rs` or `main.rs`). The `#!` prefix means "this attribute applies to the containing item" — in a root file, that's the whole crate.

**Use both.** The `-D warnings` flag in CI catches rustc warnings too (deprecated APIs, unused variables, etc.). The `#![deny(clippy::all)]` in source means the deny is enforced even when running clippy locally without that flag — it's self-documenting and makes code review clearer.

### Add it to each crate

**`es-mini/src/main.rs`** (top of file):
```rust
#![deny(clippy::all)]

fn main() {
    println!("es-mini: starting up (HTTP server coming in Tutorial 04)");
}
```

**`es-codec/src/lib.rs`**, **`es-query/src/lib.rs`**, **`es-index/src/lib.rs`** (top of each):
```rust
#![deny(clippy::all)]
```

### Suppressing a specific lint locally

When you have a legitimate reason to deviate (rare, but real):

```rust
#[allow(clippy::too_many_arguments)]
fn build_query_context(/* ... */) { }
```

The `#[allow(...)]` attribute is applied per-item and overrides the crate-level `deny`. It's intentional, localized, and visible in code review. Don't suppress at the crate level unless you've confirmed the lint is systematically wrong for your use case.

### Run clippy now

```bash
cargo clippy --workspace --all-targets -- -D warnings
```

On a fresh workspace with only stub files it should pass clean. If it reports anything, fix it before moving on.

---

## 3. `cargo test` and the Built-in Test Framework

### Why there's no test framework to install

Rust's test runner is built into `cargo`. No Jest, no pytest, no Mocha. The standard attributes are:

```rust
#[cfg(test)]          // conditional compilation: only include this module in test builds
mod tests {
    use super::*;     // import everything from the parent module

    #[test]           // mark this fn as a test case
    fn it_works() {
        assert_eq!(2 + 2, 4);
    }
}
```

- `#[cfg(test)]` — this is Rust's conditional compilation system. The `test` cfg flag is set by `cargo test` but not by `cargo build`. The entire `mod tests` block is stripped from release binaries. Compare to Jest's `if (process.env.NODE_ENV === 'test')` but enforced at compile time with zero runtime cost.
- `#[test]` — marks a function as a test. The function must take no arguments and return `()` (or `Result<(), E>` for fallible tests).
- `assert_eq!(left, right)` — macro that panics with a useful message if the values aren't equal. A panicking test is a failing test.

### Write a trivial test to prove CI works

Add this to **`es-query/src/lib.rs`**:

```rust
#![deny(clippy::all)]

/// Placeholder until real query parsing is implemented.
pub fn placeholder() -> &'static str {
    "es-query"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_returns_crate_name() {
        assert_eq!(placeholder(), "es-query");
    }
}
```

Run it:

```bash
cargo test -p es-query
```

You should see:

```
running 1 test
test tests::placeholder_returns_crate_name ... ok

test result: ok. 1 passed; 0 failed
```

To run all tests across the workspace:

```bash
cargo test --workspace
```

---

## 4. GitHub Actions CI Workflow

### Toolchain action choice: `dtolnay/rust-toolchain`

`actions-rs` was the community standard action for years. It's now **unmaintained** — the last commit was 2022. Do not use it.

[`dtolnay/rust-toolchain`](https://github.com/dtolnay/rust-toolchain) is the current community standard. It's actively maintained, handles `rust-toolchain.toml` automatically, and has clean one-liner syntax.

### Caching: `Swatinem/rust-cache`

Rust's incremental compilation is powerful but the `target/` directory gets large fast. The Cargo registry cache (`~/.cargo/registry`) also benefits from caching between runs. [`Swatinem/rust-cache@v2`](https://github.com/Swatinem/rust-cache) handles both with automatic cache key invalidation based on `Cargo.lock`, `Cargo.toml` files, and the Rust toolchain version.

It's a one-liner that replaces a dozen lines of manual `actions/cache` configuration and handles the tricky invalidation edge cases (like: don't restore a cache that was built with a different Rust version).

### The workflow

Create **`.github/workflows/ci.yml`**:

```yaml
name: CI

on:
  push:
    branches: [main]
  pull_request:
    branches: [main]

env:
  CARGO_TERM_COLOR: always   # force color output even in CI logs
  RUST_BACKTRACE: 1          # print backtraces on panics

jobs:
  ci:
    name: fmt + clippy + test
    runs-on: ubuntu-latest

    steps:
      - name: Checkout
        uses: actions/checkout@v4

      # Installs the toolchain declared in rust-toolchain.toml.
      # If rust-toolchain.toml doesn't specify components, this adds them explicitly.
      - name: Install Rust toolchain
        uses: dtolnay/rust-toolchain@stable
        with:
          components: rustfmt, clippy

      # Smart cache for ~/.cargo/registry, ~/.cargo/git, and target/.
      # Cache key includes the toolchain version and a hash of all Cargo.toml/Cargo.lock files.
      # Place this AFTER the toolchain step so the cache key includes the resolved rustc version.
      - name: Cache Cargo artifacts
        uses: Swatinem/rust-cache@v2

      # Check formatting without modifying files.
      # Exits non-zero if any file would be changed.
      - name: Check formatting
        run: cargo fmt --all --check

      # Run clippy across all crates and all targets.
      # -D warnings treats all warnings (rustc + clippy) as errors.
      - name: Clippy
        run: cargo clippy --workspace --all-targets -- -D warnings

      # Run all tests in the workspace.
      - name: Test
        run: cargo test --workspace
```

### What each line does

**`on: push/pull_request`** — runs on pushes to `main` and on any PR targeting `main`. This is the standard gate: PRs can't merge broken code, direct pushes to `main` are also checked.

**`CARGO_TERM_COLOR: always`** — by default Cargo detects that stdout is not a TTY in CI and strips color. This forces color on. GitHub Actions renders ANSI colors in logs, making compiler errors much easier to read.

**`RUST_BACKTRACE: 1`** — when a test panics, Rust prints a backtrace. Without this env var set, the backtrace is suppressed in CI, making failures harder to diagnose.

**`dtolnay/rust-toolchain@stable`** — installs the latest stable toolchain. The `rust-toolchain.toml` you created in Tutorial 01 will also be respected; if it pins a specific version, that version wins over `@stable`. The `components: rustfmt, clippy` line ensures those tools are installed — they're not always included in minimal toolchain installs.

**`Swatinem/rust-cache@v2`** — the default configuration is correct for a standard workspace. It caches `~/.cargo/registry` (downloaded crate sources), `~/.cargo/git` (git dependencies), and `target/` (compiled artifacts). Cache keys include a hash of all `Cargo.*` files so a dependency change busts the cache automatically.

**`cargo fmt --all --check`** — `--all` is equivalent to `--workspace` for fmt. Exits 1 if formatting is wrong, causing the CI job to fail.

**`cargo clippy --workspace --all-targets -- -D warnings`** — see Section 2. The `--all-targets` flag includes test code, which otherwise has its own common clippy issues (unused imports inside `#[cfg(test)]` modules, for example).

**`cargo test --workspace`** — runs all `#[test]` functions across all crates. Output shows per-crate test results.

### Push it and watch it go

```bash
mkdir -p .github/workflows
# ... create ci.yml as above
git add .github/workflows/ci.yml rustfmt.toml
git add es-mini/src/main.rs es-codec/src/lib.rs es-query/src/lib.rs es-index/src/lib.rs
git commit -m "ci: add formatting, clippy, and test pipeline"
git push
```

Go to your repository's Actions tab. The `CI` workflow should appear and go green.

---

## 5. `cargo deny` — Supply-Chain Auditing (Recommended)

### The analogy

`cargo deny` is `npm audit` + `license-checker` for Rust. It checks:

- **Advisories** — known vulnerabilities in your dependencies (via the [RustSec Advisory Database](https://rustsec.org/))
- **Licenses** — ensures all transitive dependencies use licenses you're OK with
- **Bans** — blocks specific crates or duplicate major versions of a crate
- **Sources** — restricts which registries and git sources are allowed

It's a cargo subcommand you install once:

```bash
cargo install --locked cargo-deny
```

Initialize the config:

```bash
cargo deny init
```

This creates `deny.toml` in the workspace root. The defaults are reasonable: it requires OSI-approved licenses and checks against the RustSec database.

Run it:

```bash
cargo deny check
```

On a fresh project with no dependencies, this passes immediately. As you add crates (starting in Tutorial 04 with `tokio` and `axum`), it will track everything they pull in.

### Add it to CI (optional but recommended)

Add a step to your CI job after `cargo test`:

```yaml
      # Install and run cargo-deny for license + vulnerability checks.
      # Using the dedicated action avoids installing cargo-deny from source on every run.
      - name: Cargo deny
        uses: EmbarkStudios/cargo-deny-action@v2
```

`cargo-deny-action` ships a prebuilt binary, so it doesn't add a full `cargo install` compile step to your CI time.

---

## 6. Release Profile Tuning

### Why this exists

`cargo build --release` enables `opt-level = 3` by default, which is good. But three additional settings matter for a production server binary:

**`lto`** (Link-Time Optimization) — the compiler can only optimize within a compilation unit (a crate) during normal compilation. LTO allows it to optimize across crate boundaries at link time. This catches dead code elimination and inlining opportunities that single-crate analysis misses. For a server binary it typically reduces binary size and improves hot-path performance. The cost is longer link time.

**`codegen-units = 1`** — by default Rust splits a crate into multiple codegen units and compiles them in parallel. This is faster to compile but limits cross-unit optimization. Setting to 1 forces all code through a single optimizer pass, improving the quality of optimization at the expense of compile time. Only relevant for release builds; debug builds use the default.

**`strip = true`** (or `strip = "debuginfo"`) — strips debug symbols from the binary. A stripped Rust binary can be 5–10x smaller. Use `"debuginfo"` if you need crash reporting (keeps symbol names but strips DWARF line info); use `true` for the smallest possible binary.

Add this to the root **`Cargo.toml`**:

```toml
[profile.release]
opt-level = 3          # default, explicit for clarity
lto = true             # cross-crate link-time optimization
codegen-units = 1      # single codegen unit for best optimization
strip = true           # strip debug symbols from release binary
```

These only affect `cargo build --release`. Debug builds (`cargo build`) are unaffected — they already prioritize compile speed over runtime speed.

> **Note:** `lto = true` (full LTO) can be slow for large projects. `lto = "thin"` is a faster alternative that gets most of the benefit. For es-mini's current size it doesn't matter; at Phase 4+ you may want to benchmark both.

Commit this along with `deny.toml` (if you initialized it in Section 5) so they land before the Definition of Done checks below:

```bash
git add Cargo.toml deny.toml
git commit -m "build: tune release profile, add cargo-deny config"
git push
```

---

## 7. Complete File Reference

After this tutorial, your repo should contain these new or modified files:

```
es-mini/
├── .github/
│   └── workflows/
│       └── ci.yml                 ← new
├── rustfmt.toml                   ← new
├── deny.toml                      ← new (if you ran cargo deny init)
├── Cargo.toml                     ← modified: [profile.release] added
├── es-mini/src/main.rs            ← modified: #![deny(clippy::all)]
├── es-codec/src/lib.rs            ← modified: #![deny(clippy::all)]
├── es-query/src/lib.rs            ← modified: #![deny(clippy::all)] + test
└── es-index/src/lib.rs            ← modified: #![deny(clippy::all)]
```

Root `Cargo.toml` additions:

```toml
[profile.release]
opt-level = 3
lto = true
codegen-units = 1
strip = true
```

---

## Definition of Done

Complete each item and run the associated validation command before moving to Tutorial 03.

1. **Formatter baseline committed**  
   `cargo fmt --all --check` exits 0 with no output.

2. **`rustfmt.toml` present at workspace root**  
   `cat rustfmt.toml` shows `edition = "2021"` and `max_width = 100`.

3. **`#![deny(clippy::all)]` in all four crate roots**  
   `grep -r 'deny(clippy::all)' es-mini/src es-codec/src es-query/src es-index/src` returns four matches.

4. **Clippy passes clean**  
   `cargo clippy --workspace --all-targets -- -D warnings` exits 0.

5. **At least one test passing**  
   `cargo test --workspace` shows at least `1 passed; 0 failed`.

6. **CI workflow runs green**  
   Push to `main` (or open a PR). Confirm the `CI` workflow in the Actions tab shows all steps green.

7. **`[profile.release]` present in root `Cargo.toml`**  
   `grep -A4 '\[profile.release\]' Cargo.toml` shows `lto`, `codegen-units`, and `strip`.

8. **Verify `--check` actually fails on bad formatting** *(optional but educational)*  
   Add a line like `let x=1;` (no spaces around `=`) to any `lib.rs`, run `cargo fmt --all --check`, confirm non-zero exit, then run `cargo fmt --all` to auto-fix and confirm it's green again.

---

## What's Next

Tutorial 03 introduces Rust's type system: structs, enums, `impl` blocks, and `serde` for JSON serialization. You'll define the first real data types for es-mini — the index settings and mapping structures — and see how Rust's type system makes invalid states unrepresentable at compile time.
