# Tutorial 01 — Cargo Workspaces & Project Bootstrap

**Series:** es-mini — Building Elasticsearch in Rust  
**Phase:** 0, Task 1  
**Prerequisites:** Linux/macOS terminal, Git, familiarity with npm/pip/cargo analogies below

---

## What You'll Build

By the end of this tutorial you'll have a compilable Rust **workspace** containing four crates — the same structure that all subsequent tutorials build on:

```
es-mini/
├── Cargo.toml          ← workspace root (no code here)
├── Cargo.lock          ← committed (binary workspace)
├── rust-toolchain.toml ← pinned toolchain
├── .gitignore
├── es-mini/            ← binary crate (HTTP server entrypoint)
│   ├── Cargo.toml
│   └── src/main.rs
├── es-codec/           ← library crate (Lucene on-disk format)
│   ├── Cargo.toml
│   └── src/lib.rs
├── es-query/           ← library crate (JSON DSL + ES|QL)
│   ├── Cargo.toml
│   └── src/lib.rs
└── es-index/           ← library crate (index model, mappings)
    ├── Cargo.toml
    └── src/lib.rs
```

`cargo build` and `cargo test` will both pass on this skeleton. Nothing runs yet — that's Tutorial 04.

---

## 1. Installing Rust via rustup

### The analogies

| Tool | JS ecosystem | Python ecosystem | Rust ecosystem |
|------|-------------|-----------------|----------------|
| Toolchain manager | nvm | pyenv | **rustup** |
| Package manager | npm | pip | **cargo** |
| Package registry | npmjs.com | PyPI | **crates.io** |
| Build tool | npm scripts / webpack | setuptools / build | **cargo** (same tool) |

`rustup` manages Rust compiler versions. One command installs it, and it handles everything else.

### Install

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Accept the default installation when prompted. Then either restart your shell or:

```bash
source "$HOME/.cargo/env"
```

Verify:

```bash
rustup --version   # rustup 1.29.0 (or newer)
rustc --version    # rustc 1.97.1 (or newer stable)
cargo --version    # cargo 1.97.1 (or newer)
```

### What rustup installs

- `rustc` — the compiler  
- `cargo` — build tool, package manager, test runner, formatter launcher  
- `rustfmt` — formatter  
- `clippy` — linter  
- Standard library sources and docs (optional but useful)

Everything lands in `~/.cargo/bin/` (on PATH after install) and `~/.rustup/`.

### Toolchain channels

```bash
rustup toolchain list
# stable-x86_64-unknown-linux-gnu (default)
# nightly-x86_64-unknown-linux-gnu
```

`stable` — production releases, every 6 weeks. Use this.  
`nightly` — daily snapshot, has unstable features. Don't use for this project.  
`beta` — RC of the next stable. Ignore.

---

## 2. Pinning the Toolchain with `rust-toolchain.toml`

### Why pin?

In Python you write `python==3.12.4` in `requirements.txt`. In Node you write `"engines": {"node": ">=20"}` in `package.json`. In Rust, `rust-toolchain.toml` tells every developer (and CI) **exactly** which compiler to use. No "works on my machine" compiler drift.

When anyone runs any `cargo` command in the repo, `rustup` reads this file and **auto-installs and switches** to the pinned version transparently. You don't call `rustup run 1.97.1 cargo build` manually; `cargo build` does it for you.

### Create the file

At the **repo root**:

```toml
# rust-toolchain.toml
[toolchain]
channel = "1.97.1"
```

> **Format note:** This is the modern TOML format. There is a legacy plain-text `rust-toolchain` file (no extension, just contains the channel string) — ignore it. Always use the TOML format with the `[toolchain]` section.

Verify it takes effect:

```bash
rustc --version
# rustc 1.97.1 (8bab26f4f 2026-07-14)
```

If that version wasn't installed, rustup fetches it automatically.

### Optional fields

```toml
[toolchain]
channel = "1.97.1"
components = ["rustfmt", "clippy"]   # ensure these are installed
targets = ["x86_64-unknown-linux-gnu"]
```

For now the minimal one-liner is enough.

---

## 3. What a Cargo Workspace Is

### The analogy

| Concept | npm | Python | Cargo |
|---------|-----|--------|-------|
| Single project | `package.json` | `pyproject.toml` | `Cargo.toml` |
| Monorepo root | `package.json` (root, workspaces field) | a `pyproject.toml` + `uv`/`hatch` workspace | root `Cargo.toml` with `[workspace]` |
| Sub-package | `packages/foo/package.json` | `packages/foo/pyproject.toml` | `foo/Cargo.toml` |
| Shared lockfile | `package-lock.json` at root | `uv.lock` at root | `Cargo.lock` at root |
| Shared build output | `node_modules/` | `.venv/` | `target/` |

A **Cargo workspace** is a root `Cargo.toml` that declares which subdirectories are **members**. All members:

- Share one `Cargo.lock` (dependency versions are consistent across the whole repo)
- Share one `target/` directory (compilation cache — build once, used by all crates)
- Can declare dependencies on each other as **path dependencies**

A member is called a **crate**. Crates are either **binary** (produce an executable) or **library** (produce a `.rlib` that other crates link against). A crate can be both.

---

## 4. Creating the Workspace Root `Cargo.toml`

The root `Cargo.toml` does **not** have a `[package]` section — it only describes the workspace:

```toml
# Cargo.toml  (repo root)
[workspace]
members = [
    "es-mini",
    "es-codec",
    "es-query",
    "es-index",
]
resolver = "2"

[workspace.package]
edition = "2021"
version = "0.1.0"
authors = []
license = "Apache-2.0"
```

**Key fields:**

- `members` — list of crate directories (relative to this file)
- `resolver = "2"` — use Cargo's v2 feature resolver. This is the default for edition 2021+, but being explicit avoids surprises.
- `[workspace.package]` — shared defaults that member crates can inherit (optional but convenient)

> **Why edition 2021, not 2024:** Edition 2024 is stable and usable here, but this series pins **edition 2021** throughout (see Tutorial 06's scratch-file example) for consistency across all crates and tutorials.

---

## 5. Creating the Four Crates

### The dependency graph

```
es-mini (binary)
  ├── es-query (lib)
  │     └── es-codec (lib)
  └── es-index (lib)
        └── es-codec (lib)
```

`es-codec` has no internal dependencies — it's the bottom of the stack. `es-query` and `es-index` each depend on `es-codec`. `es-mini` depends on `es-query` and `es-index` (and will later add `axum`, `tokio`, etc.).

### 5.1 `es-codec` — library crate

```bash
mkdir es-codec && mkdir es-codec/src
```

```toml
# es-codec/Cargo.toml
[package]
name = "es-codec"
version.workspace = true
edition.workspace = true
```

```rust
// es-codec/src/lib.rs
//! Lucene on-disk codec: read/write segment files.
//! (Skeleton — implementation in later tutorials.)

#[cfg(test)]
mod tests {
    #[test]
    fn placeholder() {
        assert!(true);
    }
}
```

**`version.workspace = true`** — inherits the version from `[workspace.package]` in the root `Cargo.toml`. Same for `edition`. This is the inheritance mechanism; without it you'd repeat `0.1.0` in every crate.

### 5.2 `es-query` — library crate

```bash
mkdir es-query && mkdir es-query/src
```

```toml
# es-query/Cargo.toml
[package]
name = "es-query"
version.workspace = true
edition.workspace = true

[dependencies]
es-codec = { path = "../es-codec" }
```

```rust
// es-query/src/lib.rs
//! JSON Query DSL and ES|QL parsing/execution.
//! (Skeleton — implementation in later tutorials.)

#[cfg(test)]
mod tests {
    #[test]
    fn placeholder() {
        assert!(true);
    }
}
```

**`{ path = "../es-codec" }`** is a **path dependency** — tells Cargo to find `es-codec` on disk rather than from crates.io. This is the standard pattern for workspace-internal dependencies.

### 5.3 `es-index` — library crate

```bash
mkdir es-index && mkdir es-index/src
```

```toml
# es-index/Cargo.toml
[package]
name = "es-index"
version.workspace = true
edition.workspace = true

[dependencies]
es-codec = { path = "../es-codec" }
```

```rust
// es-index/src/lib.rs
//! Index and document model: metadata, mappings, in-memory store.
//! (Skeleton — implementation in later tutorials.)

#[cfg(test)]
mod tests {
    #[test]
    fn placeholder() {
        assert!(true);
    }
}
```

### 5.4 `es-mini` — binary crate

```bash
mkdir es-mini && mkdir es-mini/src
```

```toml
# es-mini/Cargo.toml
[package]
name = "es-mini"
version.workspace = true
edition.workspace = true

[dependencies]
es-query = { path = "../es-query" }
es-index = { path = "../es-index" }
```

```rust
// es-mini/src/main.rs
fn main() {
    println!("es-mini: starting up (HTTP server coming in Tutorial 04)");
}
```

> **`main.rs` vs `lib.rs`:** Cargo uses filenames as a convention.  
> - `src/main.rs` → binary crate (produces an executable)  
> - `src/lib.rs` → library crate (produces a linkable `.rlib`)  
> A crate with both files is both a binary and a library — useful but not needed here.

---

## 6. Understanding `Cargo.toml` Fields

```toml
[package]
name = "es-index"          # crate name, must match directory name by convention
version = "0.1.0"          # semver; cargo enforces this format
edition = "2021"           # Rust language edition (2015, 2018, 2021, 2024)
authors = ["Your Name"]    # optional metadata
description = "..."        # optional, shown on crates.io if published
license = "Apache-2.0"     # SPDX identifier

[dependencies]
es-codec = { path = "../es-codec" }     # path dep (workspace-internal)
serde = { version = "1", features = ["derive"] }  # crates.io dep with feature flags
tokio = "1"                              # crates.io dep, shorthand

[dev-dependencies]
# deps only available in test/bench code — don't bloat production binary
# e.g. test fixtures, assertion helpers

[lib]
# Optional — Cargo infers src/lib.rs automatically.
# Only needed to customize name, crate-type, or path.
name = "es_index"          # defaults to package.name with hyphens→underscores
crate-type = ["rlib"]      # default for libraries; "cdylib" for C-compatible .so

[[bin]]
# Double-bracket = array of tables (TOML). Each [[bin]] is one executable.
# Optional — Cargo infers src/main.rs automatically.
name = "es-mini"
path = "src/main.rs"
```

> **Hyphen vs underscore:** Cargo package names use hyphens (`es-index`), but Rust module names use underscores (`es_index`). Cargo converts automatically. `use es_index::...` in code even though `Cargo.toml` says `es-index`.

---

## 7. Path Dependencies and the Dependency Graph

The path dependency syntax:

```toml
[dependencies]
es-codec = { path = "../es-codec" }
```

This is equivalent to npm's:

```json
"dependencies": {
  "es-codec": "file:../es-codec"
}
```

Cargo reads the `Cargo.toml` at the given path, compiles it, and links it. The version field is optional for path dependencies in a workspace (since they're all versioned together), but you can add it as a sanity check:

```toml
es-codec = { path = "../es-codec", version = "0.1.0" }
```

### Workspace dependency shorthand (optional)

The root `Cargo.toml` can declare shared dependencies once:

```toml
# Cargo.toml (root)
[workspace.dependencies]
es-codec = { path = "es-codec" }
serde = { version = "1", features = ["derive"] }
```

Then members inherit with:

```toml
# es-query/Cargo.toml
[dependencies]
es-codec.workspace = true
serde.workspace = true
```

Skip this for now — with four crates it's not worth the abstraction layer yet.

---

## 8. `cargo build`, `cargo run`, `cargo check`

| Command | What it does | When to use |
|---------|-------------|-------------|
| `cargo check` | Type-checks and borrow-checks the code. **Does not link or produce binaries.** | Fast feedback while writing code. ~3–10× faster than build. |
| `cargo build` | Compiles and links everything into `target/debug/`. | Before running, or to verify it compiles end-to-end. |
| `cargo build --release` | Same, with optimizations (`-O3`). Slow to compile, fast to run. | Benchmarks, production builds. |
| `cargo run` | Builds then runs the binary. | During development. Pass args after `--`: `cargo run -- --port 9200`. |
| `cargo test` | Builds test binaries and runs them. | After every change. |
| `cargo test -p es-codec` | Tests only the `es-codec` crate. | When you only changed one crate. |

Try them now:

```bash
# From the repo root:
cargo check
# Compiling es-codec v0.1.0 (.../es-codec)
# Compiling es-query v0.1.0 (.../es-query)
# Compiling es-index v0.1.0 (.../es-index)
# Compiling es-mini v0.1.0 (.../es-mini)
# Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.XX s

cargo build
# ...similar output, ends with:
# Finished `dev` profile [unoptimized + debuginfo] target(s) in X.XXs

cargo run -p es-mini
# Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.00s
# Running `target/debug/es-mini`
# es-mini: starting up (HTTP server coming in Tutorial 04)

cargo test
# running 1 test
# test tests::placeholder ... ok
# test result: ok. 1 passed; 0 failed; ...
# (repeated for each crate)
```

> **`-p` flag:** Targets a specific workspace member. `cargo build -p es-codec` builds only that crate. Useful in large workspaces to avoid rebuilding everything.

---

## 9. The `target/` Directory

`cargo build` produces all compiled artifacts in `target/`:

```
target/
├── debug/          ← unoptimized builds (default)
│   ├── es-mini     ← your runnable binary
│   ├── deps/       ← compiled dependencies (.rlib files)
│   ├── build/      ← output from build scripts (build.rs)
│   └── .fingerprint/  ← incremental compilation metadata
└── release/        ← optimized builds (cargo build --release)
```

**Always `.gitignore` this directory.** It can easily be gigabytes. It's fully reproducible from source.

```
# .gitignore
/target
```

The `/` prefix means "only at the repo root", not any nested `target/` directories. Important if you later embed test fixtures.

**Incremental compilation:** Cargo tracks file modification times and checksums. Only changed crates (and their dependents) recompile. On a cold build with no dependencies, this workspace compiles in seconds. After adding `axum` + `tokio` in Tutorial 04, the first build takes ~60–90 seconds on a typical machine; subsequent incremental builds are 1–5 seconds.

---

## 10. `Cargo.lock` — When to Commit It

`Cargo.lock` records the exact versions of every transitive dependency resolved during the last build. Think of it as `package-lock.json` or `uv.lock`.

**Rule:**

| Crate type | Commit `Cargo.lock`? |
|-----------|---------------------|
| Binary crate (produces an executable) | **Yes** |
| Library crate published to crates.io | **No** (consumers resolve their own) |
| Workspace containing a binary | **Yes** |

This workspace contains `es-mini` (a binary), so **commit `Cargo.lock`**.

Why it matters for binaries: without a lockfile, `cargo build` on a fresh clone resolves dependencies fresh. `serde = "1"` means "any 1.x" — you might get `1.0.219` today and `1.0.220` tomorrow. The lockfile makes builds reproducible and CI reliable.

Right now `Cargo.lock` doesn't exist because we have no external dependencies. It will be created when you run `cargo build` after adding `axum` and `tokio` in Tutorial 04. Commit it then.

---

## Definition of Done

Complete each step; all commands should produce the output described.

1. **`rust-toolchain.toml` exists** at the repo root and contains `channel = "1.97.1"` (or the current stable you're targeting). Running `rustc --version` in the repo root reports that exact version.

2. **Workspace `Cargo.toml` exists** at the repo root with `[workspace]`, all four members listed, and `resolver = "2"`.

3. **All four crate directories exist** (`es-mini/`, `es-codec/`, `es-query/`, `es-index/`) each with a `Cargo.toml` and the appropriate `src/main.rs` or `src/lib.rs`.

4. **`cargo check` passes** with no errors or warnings:
   ```bash
   cargo check 2>&1 | tail -1
   # Finished `dev` profile [unoptimized + debuginfo] target(s) in ...
   ```

5. **`cargo build` passes:**
   ```bash
   cargo build 2>&1 | tail -1
   # Finished `dev` profile [unoptimized + debuginfo] target(s) in ...
   ```

6. **`cargo test` passes** (three placeholder tests, one per library crate — `es-mini` is a binary with no test module):
   ```bash
   cargo test 2>&1 | grep -E "^test result"
   # test result: ok. 1 passed; 0 failed; 0 ignored; ...  (×3 for lib crates)
   ```

7. **`cargo run -p es-mini` executes** the binary and prints the startup message:
   ```bash
   cargo run -p es-mini 2>&1 | grep "es-mini"
   # es-mini: starting up (HTTP server coming in Tutorial 04)
   ```

8. **`.gitignore` excludes `target/`:** Verify with:
   ```bash
   git status --short | grep target
   # (no output — target/ is ignored)
   ```

9. **Dependency graph compiles correctly:** Confirm that `es-mini` cannot accidentally use `es-codec` directly (it's not in `es-mini/Cargo.toml`'s dependencies). Add `use es_codec;` to `es-mini/src/main.rs`, run `cargo check`, confirm it errors with `unresolved import`. Remove it.

10. **No stray `[package]` in root `Cargo.toml`:** The root file must not have a `[package]` section. Verify:
    ```bash
    grep -c '^\[package\]' Cargo.toml
    # 0
    ```

---

## What's Next

**Tutorial 02** covers CI setup: GitHub Actions workflow, `cargo fmt --check`, `cargo clippy -- -D warnings`, and caching the `target/` directory between runs. By the end, every push will verify this workspace compiles and passes clippy.

**Tutorial 03** introduces Rust's type system — structs, enums, and `serde` for JSON serialization — building the first real domain types in `es-index`.
