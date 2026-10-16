# Tutorial 13 — Integration Testing and YAML REST Test Harness

Phase 3, Tasks 12–13. Builds on Tutorial 07's integration-test scaffolding (`tests/`, `mod
common`, `#[tokio::test]`) and every HTTP endpoint built in Tutorials 04–12. This tutorial does
not add a single new production endpoint — it adds a **test harness** that drives the whole
server as a black box, using Elasticsearch's own vendored YAML REST test suite as the assertion
source instead of hand-written Rust `assert_eq!` calls.

Scope: (1) organize integration tests properly, (2) build an `xtask` task-runner crate, (3)
vendor a subset of ES's `rest-api-spec` YAML tests, (4) write a runner that parses that YAML and
executes it against a real running `es-mini` binary over HTTP, reporting PASS/FAIL/XFAIL.

---

## 1. Why this needs a different test shape than Tutorial 07's

Tutorial 07 introduced `axum-test::TestServer` — builds the `Router` in-process, no real socket,
fast, parallel-safe. That's the right tool for "does this one endpoint return the right JSON,"
and nothing here replaces it — keep writing those for new endpoints.

The YAML suite is a different job: it's ES's own compatibility contract, written against a real
running server reachable over HTTP, and it exercises **sequences** of calls (`setup` → multiple
numbered test cases → `teardown`) with cross-step data flow (`set` a value in step 2, reference
it with `$stash_key` in step 5). Driving that against an in-process `Router` would work
technically, but the point of vendoring ES's own tests is to test the actual compiled binary
people will run — same reasoning as testing a built Docker image instead of only unit-testing
the code that goes into it. Subprocess it is.

---

## 2. Integration test organization, properly

Quick recap + the piece Tutorial 07 didn't need yet:

- Every `.rs` file directly under `<crate>/tests/` is compiled as its own **independent test
  binary** — its own `fn main()`, own process, own copy of anything `static`. `es-mini/tests/
  index_management.rs` and `es-mini/tests/document_crud.rs` (Tutorial 07/08) are two of these.
- A file *without* `#[test]` functions under `tests/` still gets compiled and run as a test
  binary — with zero tests, which is harmless but wasteful, and cargo will try to find a `main`.
  That's why shared helper code goes in a **module**, not a sibling test file.
- Rust's module system treats any file at `tests/common/mod.rs` as a submodule you `mod common;`
  into another file — the `mod.rs` filename is the signal "this is a module directory entry, not
  a standalone compilation unit," so cargo's test-file auto-discovery (which only looks at
  direct children of `tests/`, `tests/*.rs`) skips it. `tests/common.rs` (a bare file, no
  subdirectory) works identically as of Rust 2018+ — either is fine; `tests/common/mod.rs` is
  used here because it's what Tutorial 07 already established, and because it leaves room to
  grow `common/` into multiple files (`common/server.rs`, `common/http.rs`) later without a
  rename.
- Naming a file `tests/helpers.rs` (no `mod` prefix trick) is the classic beginner mistake this
  avoids: cargo compiles it as its own test binary, finds no `#[test]` fns, and it "works" but
  wastes a full crate compile+link per test run for nothing. Symptom to recognize: an extra
  binary in `cargo test`'s output list that runs 0 tests.

New file this tutorial adds: `es-mini/tests/yaml_common/mod.rs` — the `ServerGuard` (§5) and
readiness-poll helper, kept separate from Tutorial 07's `tests/common/mod.rs` because it spawns
a real subprocess where the existing helper builds an in-process `Router`; different enough
setup/teardown shape to not share one module.

---

## 3. The `xtask` pattern

`cargo` has no built-in task runner — no `Makefile`, no `package.json` `"scripts"` block. The
community-standard workaround, used by `EXECUTION.md`'s Task 13 directly, is **`xtask`**: an
ordinary workspace member that's a *binary* crate whose whole job is "run project automation,"
invoked via a `cargo` alias.

**This is pure convention, not a cargo feature.** There is no `[xtask]` section in `Cargo.toml`,
no special cargo subcommand support — `xtask` is just the name everyone independently settled on
(popularized by a widely-cited `matklad` blog post and now used across dozens of major Rust
projects: `rust-analyzer`, `cargo-nextest`, etc.). Verified against crates.io: packages literally
named `xtask`, `xtask-wasm`, `xtask-watch`, `tracel-xtask` exist as *libraries of reusable
helpers* for building your own xtask crate — none of them are a cargo plugin or subcommand
implementation. The mechanism that makes `cargo xtask test-yaml` type at the CLI is a two-line
alias, shown below, not a crate.

Analogy: same relationship `npm run <script>` has to whatever tool actually runs (`webpack`,
`jest`, a plain shell one-liner) — `npm run` is generic invocation syntax defined by `npm`
itself; the *scripts* are project-specific and could be anything. `xtask` is the same idea
without `npm`'s built-in `"scripts"` object — the project defines its own dispatcher because
cargo doesn't ship one.

### Adding the `xtask` member

```toml
# Cargo.toml (root)
[workspace]
members = ["es-mini", "es-codec", "es-query", "es-index", "xtask"]
```

```toml
# xtask/Cargo.toml
[package]
name = "xtask"
version.workspace = true
edition.workspace = true
publish = false        # never meant to be published to crates.io

[dependencies]
serde_norway = { workspace = true }   # §7 — YAML parsing
serde_json = { workspace = true }
reqwest = { workspace = true }
tokio = { workspace = true, features = ["full"] }
```

`publish = false` is worth calling out explicitly — this crate has no reason to ever exist on
crates.io, and `cargo publish --workspace` (or a CI job doing it per-member) would otherwise try.

### The alias that makes `cargo xtask <name>` work

```toml
# .cargo/config.toml (root — create if it doesn't exist)
[alias]
xtask = "run --package xtask --"
```

`cargo xtask test-yaml` now expands to `cargo run --package xtask -- test-yaml` — `xtask`'s
`main()` receives `test-yaml` as `std::env::args().nth(1)` and dispatches on it, exactly like a
`Makefile` target name or an `npm run` script name. No other magic.

```rust
// xtask/src/main.rs
use std::env;

mod yaml_runner;

fn main() {
    let task = env::args().nth(1);
    match task.as_deref() {
        Some("test-yaml") => yaml_runner::run(),
        Some(other) => {
            eprintln!("unknown xtask: {other}");
            std::process::exit(1);
        }
        None => {
            eprintln!("usage: cargo xtask <task-name>");
            eprintln!("tasks: test-yaml");
            std::process::exit(1);
        }
    }
}
```

`fn main()` here is plain, synchronous, non-`async` — `xtask` builds its own tiny `#[tokio::
main]`-wrapped inner function inside `yaml_runner::run()` where the actual HTTP calls live
(§10), rather than making the whole CLI dispatcher async for no reason.

---

## 4. Starting the server for tests: two approaches, and which one this tutorial uses

**(a) In-process, `axum_test::TestServer`** — Tutorial 07's approach. Builds `es_mini::
build_app(...)` directly, no subprocess, no port, sub-millisecond startup, trivially parallel
(each test gets its own `Router` instance, no shared OS resource). Correct default for
hand-written Rust integration tests going forward (Tutorial 07's precedent stands — nothing here
changes it for `tests/index_management.rs` etc.).

**(b) Subprocess, `std::process::Command`** — what this tutorial uses for the YAML runner, and
only for the YAML runner. Spawns the actual `es-mini` binary as a child OS process, talks to it
over a real TCP port via `reqwest`, same as a human running `curl` against it. Slower (real
process startup, real socket), but it's testing the artifact that ships, not a test harness
around the library that builds it — the right trade for a vendored *compatibility* suite whose
entire purpose is "does the real thing behave like real ES."

```rust
use std::process::{Child, Command, Stdio};

fn spawn_server(port: u16) -> Child {
    Command::new("cargo")
        .args(["run", "--quiet", "--bin", "es-mini"])
        .env("ES_PORT", port.to_string())
        .stdout(Stdio::null())   // don't spam the xtask's own output with server logs
        .stderr(Stdio::inherit()) // do let panics/errors through
        .spawn()
        .expect("failed to spawn es-mini")
}
```

`cargo run --bin es-mini` (not a raw `target/debug/es-mini` path) is deliberate — it rebuilds if
stale, matching what a developer running the suite locally after an edit expects, at the cost of
a first-run compile delay that a pre-built binary path wouldn't have. Flagged as an accepted
trade, not revisited: CI already runs `cargo build` earlier in the pipeline, so the binary is
warm by the time the YAML job runs.

---

## 5. Waiting for readiness — and `ServerGuard` for cleanup

A freshly spawned `Child` is not immediately ready to accept connections — `cargo run` itself
takes time before the child binary even starts binding a socket. Polling `GET /` (Tutorial 04's
info endpoint, deliberately chosen because it needs no state and always succeeds once the server
is up) is the standard "wait for it" strategy — same shape as a Docker healthcheck loop or a CI
`wait-for-it.sh` script.

```rust
use reqwest::Client;
use std::time::Duration;
use tokio::time::sleep;

async fn wait_for_ready(client: &Client, base_url: &str) {
    let mut backoff = Duration::from_millis(50);
    for attempt in 1..=20 {
        if client.get(base_url).send().await.is_ok() {
            return;
        }
        if attempt == 20 {
            panic!("es-mini did not become ready after {attempt} attempts");
        }
        sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(2)); // exponential, capped
    }
}
```

Exponential backoff (double each attempt, capped at 2s) rather than a fixed sleep — fast when the
server is already up (common case on a warm build), doesn't hammer a slow-starting process with
a tight loop, doesn't wait unnecessarily long once it's actually ready.

### `Drop` and `ServerGuard`

Every early return, panic, or `assert!` failure inside the YAML runner must still kill the child
process — otherwise a failed test run leaves an orphaned `es-mini` bound to the test port,
breaking the *next* run with "address already in use." Rust's `Drop` trait is the guaranteed
mechanism for this: **`drop()` runs when a value goes out of scope, including during a panic
unwind** — not just on the normal-exit path. This is the same job Python's `with` statement
(`__exit__`) or a `try/finally` block does, except it doesn't require the caller to remember to
wrap anything in a block — it's tied to the value's lifetime, automatically, everywhere that
value can go out of scope. (Contrast: a Python `__del__` is *not* a reliable analogue — CPython
doesn't guarantee `__del__` runs promptly or at all under every condition; Rust's `Drop` is
deterministic and scope-tied, closer to `with`/`finally` than to `__del__`.)

```rust
struct ServerGuard {
    child: Child,
}

impl ServerGuard {
    fn spawn(port: u16) -> Self {
        ServerGuard { child: spawn_server(port) }
    }
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        // .ok() — if the process already exited (e.g. it crashed), kill() errors;
        // that's not a reason to panic inside a Drop impl (panicking during an
        // existing panic's unwind aborts the whole process instead of reporting
        // the original failure).
        let _ = self.child.kill();
        let _ = self.child.wait(); // reap the zombie, don't leave it defunct
    }
}
```

Usage — the guard is the only thing that needs to stay in scope; nothing calls `.kill()`
explicitly anywhere else:

```rust
fn run() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let port = 9299; // fixed test port, distinct from the default 9200
        let _guard = ServerGuard::spawn(port); // killed on drop, any exit path
        let client = Client::new();
        let base_url = format!("http://127.0.0.1:{port}");
        wait_for_ready(&client, &base_url).await;

        // ... run the whole YAML suite here (§10) ...
        // if this panics, `_guard`'s Drop still runs during unwind.
    });
}
```

**Never call `.kill()` manually and also rely on `Drop`** — do one or the other. The manual call
is redundant once the guard exists and invites a stale/duplicated cleanup path if someone edits
one but not the other later.

---

## 6. `reqwest` for HTTP in integration tests

Time to add `reqwest` — the async HTTP client. Verified current version: **`reqwest = "0.13"`**
resolves to **0.13.4**; default features already include an async client with TLS
(`default-tls`), so no extra flag is needed to get `.send().await` working. The `json` feature
is required to get `.json::<T>()` (response body → typed struct via serde) and to send a JSON
body directly — add it explicitly, it is not in `default`.

```toml
# Cargo.toml (root, [workspace.dependencies])
reqwest = { version = "0.13", features = ["json"] }
```

```toml
# xtask/Cargo.toml — a normal (not dev-) dependency, since xtask IS the test tool
reqwest = { workspace = true }
```

```toml
# es-mini/Cargo.toml — [dev-dependencies] only if any hand-written Rust test needs it later
# (none does yet — Tutorial 07's axum-test covers all in-process cases)
```

Core calls used throughout the runner:

```rust
let resp = client
    .put(format!("{base_url}/test-index"))
    .json(&serde_json::json!({}))
    .send()
    .await?;

let status: u16 = resp.status().as_u16();
let body: serde_json::Value = resp.json().await?;
```

`reqwest::Client::new()` is cheap to construct once and reuse (it holds a connection pool
internally) — build one `Client` for the whole YAML run, not one per request.

---

## 7. The ES YAML REST test format

ES's own `rest-api-spec/src/main/resources/rest-api-spec/test/` directory (part of the
`elasticsearch-specification`/main ES source tree, pinned per `COMPATIBILITY.md`) is a directory
of `.yml` files, each containing **multiple YAML documents separated by `---`**. Example, the
exact shape `EXECUTION.md` Task 12/13 targets:

```yaml
setup:
  - do:
      indices.create:
        index: test
---
"test basic get":
  - do:
      index:
        index: test
        id: "1"
        body: { "title": "hello" }
  - match: { _id: "1" }
---
"test missing":
  - do:
      get:
        index: test
        id: "missing"
        ignore: 404
  - is_true: _source
```

Reading this structure:

- **The first `---`-delimited document, if its top-level key is `setup`,** is a list of steps run
  once *before every test case in the file* — not once per file total, once per test. (`teardown`
  is the symmetric post-test cleanup block, same shape, run after each case.)
- **Every other document** is a single test case: a YAML string key (the test's human-readable
  name, e.g. `"test basic get"`) mapping to a **list of steps**, executed in order.
- **A `do:` step** is a map with exactly one key — the ES API action name (`indices.create`,
  `index`, `get`, `delete`, `search`, …) — whose value is that action's parameters, essentially a
  YAML-shaped version of that action's REST request body/path params combined.
- **Everything else in a step is an assertion** on the *result of the most recent `do:`* — `match`,
  `length`, `is_true`, `is_false`, `set`, `catch` (below).

### Assertions and directives to support

| Directive | Meaning | Example |
|---|---|---|
| `match` | Field at a dot-notation path equals a value (supports partial-object match too) | `match: { _id: "1" }` |
| `length` | Field at a path has exactly N elements/entries/chars | `length: { hits.hits: 3 }` |
| `is_true` | Field at a path exists and is JSON-truthy (not `false`/`null`/`0`/`""`) | `is_true: _source` |
| `is_false` | Field at a path is absent or JSON-falsy | `is_false: found` |
| `set` | Stash the value at a path under a name, for later `$name` substitution | `set: { _id: doc_id }` |
| `skip` | Unconditionally or conditionally skip this test | `skip: { version: "< 9.0.0", reason: "..." }` |
| `catch` | Assert the `do:` was expected to fail, and how | `catch: missing` or `catch: /no such index.*/` |

`catch` values map to ES's own conventional short names for status codes: `missing` → 404,
`conflict` → 409, `bad_request` → 400, `unauthorized` → 401, `forbidden` → 403,
`request_timeout` → 408, `param` (client-side validation, not applicable to a already-parsed
YAML test) — plus a `/regex/`-delimited form that matches the error `reason` string instead of a
fixed name. A `do:` under `catch` that does *not* fail as expected is itself a test failure, same
as a `match` that doesn't match.

`set` + `$name` is cross-step **stash** — capture a server-generated value (auto `_id`, a
`_version`, a `_seq_no`) in one step, reference it in a later step's `do:` parameters:

```yaml
- do:
    index:
      index: test
      body: { "title": "hi" }
  - set: { _id: created_id }
  - do:
      get:
        index: test
        id: "$created_id"
```

`skip` is a header-shaped stanza (usually the first step of `setup`, or the first step of an
individual test) that says "don't run this at all" — either unconditionally or gated on a version
range, with a `reason` string that's purely documentation (surfaced in the runner's report, not
otherwise used). Version-gated skips matter here because a single vendored `.yml` file may
contain cases written for both older and newer ES behavior; a single-version project like
es-mini (`COMPATIBILITY.md`'s pinned ES 9.x) can, in practice, treat any `skip` with a version
range as always-skip *if* the range would exclude the pinned version, and otherwise ignore the
`skip` entirely — no actual version negotiation needed since there's only one target version.

---

## 8. Vendoring the tests

```
tests/yaml/
├── INCLUDED.md
├── XFAIL.md
├── indices.create/
│   └── 10_basic.yml
├── indices.delete/
│   └── 10_basic.yml
├── index/
│   └── 10_with_id.yml
├── get/
│   └── 10_basic.yml
├── bulk/
│   └── 10_basic.yml
├── search/
│   └── 10_basic.yml
└── esql/
    └── 10_basic.yml
```

Files are copied **verbatim** from the pinned `rest-api-spec` ref (`COMPATIBILITY.md`) into this
tree, mirroring their upstream relative path (upstream groups by API name, e.g. everything under
`indices.create/`). `tests/yaml/INCLUDED.md` is the manifest:

```markdown
# Vendored YAML test manifest

Source: elastic/elasticsearch @ <pinned commit/tag from COMPATIBILITY.md>
Path: rest-api-spec/src/main/resources/rest-api-spec/test/

## Included

| File | Reason |
|---|---|
| indices.create/10_basic.yml | index management, Phase 1 Task 6 |
| indices.delete/10_basic.yml | index management, Phase 1 Task 6 |
| index/10_with_id.yml | document CRUD, Phase 1 Task 7 |
| get/10_basic.yml | document CRUD, Phase 1 Task 7 |
| bulk/10_basic.yml | bulk ingest, Phase 1 Task 8 |
| search/10_basic.yml | search, Phase 2 Task 10 |
| esql/10_basic.yml | hand-picked FROM/WHERE/LIMIT/KEEP/DROP/SORT subset, Phase 2 Task 11 |

## Excluded (whole file, not individual cases)

| File | Reason |
|---|---|
| indices.create/20_synonym_analyzer.yml | analyzer feature out of MVP scope |
| search/20_query_string.yml | query_string DSL not implemented |
| esql/*.yml (rest of upstream, all other files) | upstream ES|QL suite exercises EVAL/STATS/multi-source FROM and other grammar outside Tutorial 12's supported subset; only a hand-picked `esql/10_basic.yml` covering FROM/WHERE/LIMIT/KEEP/DROP/SORT is vendored, not copied verbatim from a single upstream file |
```

**Selection criteria** (how to decide which files to include, since upstream has hundreds): pick
files whose action names map onto implemented endpoints — `indices.*`, bare document actions
(`index`, `get`, `delete`, `update`), `bulk`, `search`, a hand-filtered `esql` subset — and, within
a chosen file, accept that *individual test cases* inside it may still not pass yet (that's what
XFAIL, §11, is for — file-level inclusion and per-case pass/fail are different decisions).
**Never edit a vendored file's contents** — a case that doesn't pass gets recorded in `XFAIL.md`,
not patched, deleted, or commented out. Excluding an entire file is allowed and is a normal,
recorded decision (`INCLUDED.md`'s exclusion table); silently trimming cases out of an included
file is not — that would make the manifest lie about what "included" means.

---

## 9. YAML parsing: `serde_yaml` is deprecated — use `serde_norway`

Verified on crates.io: `serde_yaml` is published as **`0.9.34+deprecated`** — the `+deprecated`
suffix is literally baked into the version string by its maintainer (dtolnay), and its own
`Cargo.toml`/readme point at community successors. `serde_norway` — a maintained fork with the
same API surface, actively versioned (current: **0.9.42**) — is the direct drop-in replacement
and is what's used here.

```toml
# Cargo.toml (root, [workspace.dependencies])
serde_norway = "0.9"
```

```toml
# xtask/Cargo.toml
serde_norway = { workspace = true }
```

API is unchanged from `serde_yaml` (rename the crate, nothing else): `serde_norway::from_str`,
`serde_norway::Value`, `serde_norway::from_slice`.

**Parse to `serde_norway::Value`, not a typed struct.** ES's YAML test format is dynamically
shaped in ways that resist a fixed `#[derive(Deserialize)]` struct — a `do:` block's inner
key *is* the action name (same "discriminant is a JSON/YAML key, not a tag value" shape Tutorial
09 hit for bulk actions and Tutorial 11 hit for the query DSL); assertion values (`match`,
`length`) can be strings, numbers, booleans, or nested maps depending on the field being checked;
`catch` is sometimes a bare string, sometimes a `/regex/`-delimited string — same field, two
shapes. A `Value`-first walk, keyed by whatever's actually present, is simpler and more honest
than fighting serde's tagged-enum machinery to force this into static types.

One YAML-1.1-vs-JSON wrinkle worth flagging explicitly, since it's a real historical gotcha with
this exact crate family: unquoted `yes`/`no`/`on`/`off` parse as YAML 1.1 booleans, not strings —
`serde_norway::Value` (like `serde_yaml` before it) follows this, so a step like
`match: { some_field: no }` becomes `Value::Bool(false)`, not `Value::String("no")`. The vendored
upstream test files are already written with this in mind (ES's own tooling uses the same
convention), so this doesn't need a workaround here — just don't be surprised by it when tracing
through a case that has a bare `yes`/`no` in it.

Converting a parsed `serde_norway::Value` to `serde_json::Value` for the rest of the runner
(everything downstream — path resolution, comparison against HTTP response bodies — is JSON, not
YAML) is a `serde_json::to_value` / round-trip through `serde_json::from_str(&serde_norway::
to_string(...))`, or, simplest, matching the two `Value` shapes structurally with a small
recursive `yaml_to_json(v: serde_norway::Value) -> serde_json::Value` converter (map/sequence/
scalar cases). Do this conversion once, right after parsing each YAML document — same "own at the
parse boundary" discipline Tutorial 12 established for ES|QL's borrowed-`&str` parser output.

---

## 10. Building the runner

### Data model

```rust
// xtask/src/yaml_runner/model.rs
use serde_json::Value;
use std::collections::HashMap;

pub struct TestFile {
    pub path: String,
    pub setup: Vec<Step>,
    pub teardown: Vec<Step>,
    pub cases: Vec<TestCase>,
}

pub struct TestCase {
    pub name: String,
    pub steps: Vec<Step>,
}

pub enum Step {
    Do { action: String, params: Value },
    Match { path: String, expected: Value },
    Length { path: String, expected: u64 },
    IsTrue(String),
    IsFalse(String),
    Set(HashMap<String, String>), // path -> stash key
    Skip { reason: String },
    Catch(String), // "missing" | "bad_request" | ... | "/regex/"
}
```

### JSON path resolution

Every `match`/`length`/`is_true`/`is_false`/`set` path is dot-notation over the *last `do:`'s
response JSON* — `hits.total.value`, `_source.title`, `_id`. One small helper, reused by all
four assertion kinds:

```rust
// xtask/src/yaml_runner/path.rs
use serde_json::Value;

pub fn resolve_path<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.').try_fold(value, |current, segment| match current {
        Value::Object(map) => map.get(segment),
        Value::Array(arr) => segment.parse::<usize>().ok().and_then(|i| arr.get(i)),
        _ => None,
    })
}
```

`.try_fold` here does the same job a hand-rolled loop with an early `return None` would, but as
one expression — each `.` segment either narrows `current` to the next nested value or the whole
fold short-circuits to `None` on the first missing/wrong-shaped segment. This is the same
"iterator adapter over manual loop" preference established since Tutorial 09/11, applied to tree
traversal instead of a flat sequence.

### The stash

```rust
// xtask/src/yaml_runner/stash.rs
use serde_json::Value;
use std::collections::HashMap;

#[derive(Default)]
pub struct Stash(HashMap<String, Value>);

impl Stash {
    pub fn set(&mut self, key: String, value: Value) {
        self.0.insert(key, value);
    }

    /// Recursively replace any string value of the exact form "$key" with the
    /// stashed value for `key`, throughout a `do:` block's parameter tree.
    pub fn substitute(&self, params: &Value) -> Value {
        match params {
            Value::String(s) => s
                .strip_prefix('$')
                .and_then(|key| self.0.get(key))
                .cloned()
                .unwrap_or_else(|| params.clone()),
            Value::Object(map) => Value::Object(
                map.iter().map(|(k, v)| (k.clone(), self.substitute(v))).collect(),
            ),
            Value::Array(arr) => Value::Array(arr.iter().map(|v| self.substitute(v)).collect()),
            other => other.clone(),
        }
    }
}
```

A `set: { _id: created_id }` step, after a `do:` whose response was `{"_id": "abc123", ...}`,
calls `stash.set("created_id".into(), json!("abc123"))`. A later `do:` block containing
`"id": "$created_id"` gets rewritten to `"id": "abc123"` by `substitute()` before the request is
sent — this rewrite happens once, right before dispatch (§10 "Executing a case"), never inside
the action-dispatch methods themselves.

### `ApiClient` — mapping YAML action names to HTTP calls

```rust
// xtask/src/yaml_runner/api_client.rs
use reqwest::Client;
use serde_json::Value;

pub struct ApiClient {
    client: Client,
    base_url: String,
}

impl ApiClient {
    pub async fn dispatch(&self, action: &str, params: &Value) -> reqwest::Result<(u16, Value)> {
        match action {
            "indices.create" => self.indices_create(params).await,
            "indices.delete" => self.indices_delete(params).await,
            "index" => self.index_doc(params).await,
            "get" => self.get_doc(params).await,
            "delete" => self.delete_doc(params).await,
            "bulk" => self.bulk(params).await,
            "search" => self.search(params).await,
            "esql.query" => self.esql_query(params).await,
            other => panic!("yaml runner: unsupported action {other} — add an ApiClient method or exclude the file"),
        }
    }

    async fn indices_create(&self, params: &Value) -> reqwest::Result<(u16, Value)> {
        let index = params["index"].as_str().unwrap();
        let body = params.get("body").cloned().unwrap_or_else(|| serde_json::json!({}));
        let resp = self.client.put(format!("{}/{index}", self.base_url)).json(&body).send().await?;
        Ok((resp.status().as_u16(), resp.json().await?))
    }

    async fn index_doc(&self, params: &Value) -> reqwest::Result<(u16, Value)> {
        let index = params["index"].as_str().unwrap();
        let body = &params["body"];
        let resp = match params.get("id").and_then(Value::as_str) {
            Some(id) => self.client.put(format!("{}/{index}/_doc/{id}", self.base_url)).json(body).send().await?,
            None => self.client.post(format!("{}/{index}/_doc", self.base_url)).json(body).send().await?,
        };
        Ok((resp.status().as_u16(), resp.json().await?))
    }

    async fn get_doc(&self, params: &Value) -> reqwest::Result<(u16, Value)> {
        let index = params["index"].as_str().unwrap();
        let id = params["id"].as_str().unwrap();
        let resp = self.client.get(format!("{}/{index}/_doc/{id}", self.base_url)).send().await?;
        Ok((resp.status().as_u16(), resp.json().await?))
    }

    /// `do: { esql.query: { body: { query: "..." } } }` — the one action whose params nest the
    /// real request body under a `body` key instead of spelling out `index`/`id`/etc. fields
    /// directly, since ES|QL's only request field is the query string itself (Tutorial 12 §16).
    async fn esql_query(&self, params: &Value) -> reqwest::Result<(u16, Value)> {
        let body = &params["body"];
        let resp = self.client.post(format!("{}/_query", self.base_url)).json(body).send().await?;
        Ok((resp.status().as_u16(), resp.json().await?))
    }

    // delete_doc, indices_delete, bulk, search — same shape: read fields out of
    // `params`, build the matching es-mini route, return (status, body).
    // elided for brevity — one method per action name, adding a new one is the
    // entire cost of supporting a newly-vendored file's action.
}
```

One method per action name is a closed, exhaustive-by-construction mapping — the same
philosophy behind Tutorial 11's `Query` enum (spec-fixed vocabulary, no need for a `dyn Trait`
registry): the set of ES REST actions this project supports is exactly the set of vendored files
this project chose to include (§8), so an unmatched action name means either a bug or a file that
should have been excluded — `panic!` on the `other` arm is deliberate, loud failure, not a silent
no-op.

### Executing a case

```rust
async fn run_case(client: &ApiClient, setup: &[Step], case: &TestCase) -> CaseResult {
    let mut stash = Stash::default();
    let mut last_response: Option<Value> = None;
    let mut expect_catch: Option<String> = None;

    for step in setup.iter().chain(case.steps.iter()) {
        match step {
            Step::Skip { reason } => return CaseResult::Skipped(reason.clone()),
            Step::Catch(spec) => expect_catch = Some(spec.clone()),
            Step::Do { action, params } => {
                let substituted = stash.substitute(params);
                let (status, body) = client.dispatch(action, &substituted).await.unwrap();
                if let Some(spec) = expect_catch.take() {
                    if !catch_matches(&spec, status, &body) {
                        return CaseResult::Fail(format!("expected catch {spec:?}, got {status} {body}"));
                    }
                } else if !(200..300).contains(&status) {
                    return CaseResult::Fail(format!("unexpected {status}: {body}"));
                }
                last_response = Some(body);
            }
            Step::Match { path, expected } => {
                let actual = resolve_path(last_response.as_ref().unwrap(), path);
                if actual != Some(expected) {
                    return CaseResult::Fail(format!("match {path}: expected {expected:?}, got {actual:?}"));
                }
            }
            Step::Length { path, expected } => { /* resolve_path then .as_array()/.as_object()/.as_str().len() */ }
            Step::IsTrue(path) => { /* resolve_path(...).map(is_truthy).unwrap_or(false) check */ }
            Step::IsFalse(path) => { /* inverse of IsTrue */ }
            Step::Set(mapping) => {
                for (path, key) in mapping {
                    let value = resolve_path(last_response.as_ref().unwrap(), path).unwrap().clone();
                    stash.set(key.clone(), value);
                }
            }
        }
    }
    CaseResult::Pass
}
```

Trace through the vendored `"test basic get"` case from §7 against this loop: step 1 (`do:
index`) sends `PUT /test/_doc/1`, gets back `{"_index":"test","_id":"1","result":"created",...}`,
stores it as `last_response`; step 2 (`match: {_id: "1"}`) calls `resolve_path(last_response,
"_id")`, gets `Some(Value::String("1"))`, compares equal to `expected = Value::String("1")` —
pass. The `"test missing"` case: `do: get` with `id: "missing"` (and `ignore: 404` in the
params, which `ApiClient` should pass straight through, unmodified — it's an ES *client-side*
directive telling the runner "a 404 here isn't an error," equivalent to this case having an
implicit `catch: missing` scoped only to that one `do:`); then `is_true: _source` checks that
even a 404 response still carries a `_source` field in es-mini's JSON body (matching real ES,
which returns the document's last-known `_source` on a "not found" response when `ignore: 404`
suppressed the error) — this is exactly the kind of case that's easy to get subtly wrong and
exactly why vendoring ES's own assertions instead of hand-writing new ones catches real bugs.

---

## 11. Xfail / allowlist

Not every vendored case will pass on day one — some exercise endpoints or fields not yet
implemented (Tutorial 12's own "Known scope gaps" DoD note flagged several ES|QL grammar gaps as
future XFAIL seeds). `tests/yaml/XFAIL.md` tracks these explicitly:

```markdown
# Expected-failure YAML test cases

Format: `<file path>::<case name>` — one per line, `#` comment allowed.

indices.create/20_synonym_analyzer.yml::"create index with synonym analyzer"  # analyzers TODO
search/10_basic.yml::"test sort by multiple fields with missing values"       # `missing` param on sort not implemented
esql/10_basic.yml::"test WHERE with boolean AND"                             # Tutorial 12 grammar doesn't support AND/OR yet
```

Runner behavior:

- A case listed in `XFAIL.md` that **fails** → reported as `XFAIL` (expected failure). Does not
  fail the CI job.
- A case listed in `XFAIL.md` that **passes** → reported as `UNEXPECTED PASS`, and this **does**
  fail the CI job. The fix is to remove that line from `XFAIL.md` in the same change that made it
  pass — an unexpected pass left un-recorded means the allowlist silently drifts out of sync with
  reality, exactly the kind of stale-allowlist rot that makes xfail lists untrustworthy over time.
- A case **not** listed in `XFAIL.md` that fails → plain `FAIL`, fails CI, same as any other test
  failure.

This gives `EXECUTION.md` Task 13's requirement directly: CI can run the *entire* vendored suite
today, with known gaps, without blocking on work not yet done, while still catching regressions
(anything that used to pass and doesn't anymore is a `FAIL`, not silently absorbed) and catching
drift (anything on the allowlist that's secretly fixed now has to come off the list).

---

## 12. Runner output

```
running tests/yaml/indices.create/10_basic.yml
  PASS   "create index with default settings"
  PASS   "create index with explicit settings"
running tests/yaml/index/10_with_id.yml
  PASS   "test basic get"
  PASS   "test missing"
running tests/yaml/search/10_basic.yml
  FAIL   "test sort by multiple fields with missing values"
         match hits.hits.0._source.tie_field: expected "a", got "b"
running tests/yaml/esql/10_basic.yml
  XFAIL  "test WHERE with boolean AND"  (nom-language grammar doesn't support AND/OR)

summary: 5 pass, 1 fail, 1 xfail, 0 unexpected pass (7 total)
```

```rust
struct Summary { pass: u32, fail: u32, xfail: u32, unexpected_pass: u32 }

fn exit_code(summary: &Summary) -> i32 {
    if summary.fail > 0 || summary.unexpected_pass > 0 { 1 } else { 0 }
}
```

CI wires `cargo xtask test-yaml`'s exit code straight into the job's pass/fail — the same
"generic CLI exit code, not a custom parser reading the summary text" convention every other
`cargo test`/`cargo clippy` step in `.github/workflows/ci.yml` already uses.

---

## 13. Definition of Done

1. `xtask` exists as a workspace member (`Cargo.toml`'s `[workspace] members` updated), builds
   with `cargo build -p xtask`, and `.cargo/config.toml`'s `[alias] xtask = "run --package xtask
   --"` makes `cargo xtask` resolve without the `--package`/`--` boilerplate spelled out.
2. `cargo xtask` with no task name prints a usage message and exits non-zero; `cargo xtask
   test-yaml` runs the YAML suite.
3. `reqwest = "0.13"` with the `json` feature is in `[workspace.dependencies]`; `xtask` depends
   on it directly (not as a dev-dependency — `xtask` *is* the test tool).
4. `serde_norway = "0.9"` is in `[workspace.dependencies]` (not `serde_yaml`, which is deprecated
   upstream) and is used to parse every vendored `.yml` file to `serde_norway::Value`, converted
   to `serde_json::Value` before any assertion logic runs.
5. `ServerGuard` (§5) implements `Drop`, is the only thing responsible for killing the spawned
   `es-mini` child process, and a deliberately-panicking test case still results in the child
   process being killed (verify: run the suite with one case forced to `panic!`, then check
   `ps`/`pgrep es-mini` shows no leftover process).
6. `tests/yaml/` contains the vendored files verbatim (byte-identical to the pinned
   `rest-api-spec` ref — verify with `diff` or a checksum against a fresh checkout of the pinned
   commit), with the single documented exception of `esql/10_basic.yml` (§8 — hand-assembled from
   a subset of upstream ES|QL cases rather than one verbatim upstream file, since no single
   upstream file matches Tutorial 12's grammar subset), plus `tests/yaml/INCLUDED.md` listing
   every included file's inclusion reason and every deliberately excluded file's exclusion
   reason.
7. The runner supports, at minimum: `setup`/`teardown`, `do`, `match`, `length`, `is_true`,
   `is_false`, `set`/`$stash` substitution, `skip`, and `catch` (both the short-name form and the
   `/regex/` form).
8. `tests/yaml/XFAIL.md` exists; a case listed there that fails is reported `XFAIL` and does not
   fail the job; a listed case that unexpectedly passes is reported `UNEXPECTED PASS` and does
   fail the job.
9. `cargo xtask test-yaml` is wired into `.github/workflows/ci.yml` as its own job/step, and its
   process exit code (not parsed console output) gates that job's pass/fail.
10. At least the Phase 1–2 endpoints (index management, document CRUD, bulk, search, ES|QL)
    have passing (non-XFAIL) YAML coverage — verify by running the suite and confirming the
    summary line's `pass` count is non-zero for files under each of those five action families.
11. No vendored YAML file's contents were edited to make a test pass — every fix that closes an
    XFAIL entry is a change to `es-mini`/`es-index`/`es-query` source, never a change to
    `tests/yaml/**/*.yml`.
