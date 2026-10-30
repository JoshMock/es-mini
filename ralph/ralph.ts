// Ralph loop: each iteration gives one fresh pi agent session one target (a known-failing test
// or a goal), then verifies the result independently and rolls back anything that does not hold.
//
//   node ralph.ts [--suites yaml,parity,goals] [--max N] [--until '[DAY] HH:MM'] [--target ID]
//                 [--review-every N] [--timeout-min N] [--dry-run]
//
// Suites run in the given order; a suite ends when it runs out of targets or rejects 3
// iterations in a row. Without --max or --until, one iteration runs. --dry-run shows the next
// target and prompt without starting an agent. Unattended runs append to ralph/REPORT.md, read
// agent escalations from ralph/NEEDS_HUMAN.md (both git-ignored), and send a notification at the end.
import { execFileSync, spawnSync } from "node:child_process";
import { appendFileSync, existsSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";

interface Suite {
  /** Target ids, one per line; `#` lines are comments. */
  list: string;
  /** Paths this suite's agents must never modify. Every iteration honors every suite's list. */
  frozen: string[];
  /** How to work on one target, shown to the agent. */
  howTo: (id: string) => string;
  /**
   * Known-failure lists require the target's line to be deleted. Goals can take several
   * iterations: the agent deletes the line only when the goal's done-criteria hold.
   */
  kind: "xfail" | "goal";
}

const SUITES: Record<string, Suite> = {
  yaml: {
    kind: "xfail",
    list: "tests/yaml-xfail.txt",
    frozen: ["tests/yaml", "tests/yaml-out-of-scope.txt", "es-mini/tests/yaml.rs"],
    howTo: (id) => `Reproduce: \`YAML_FILTER='${id}' YAML_VERBOSE=1 cargo test -p es-mini --test yaml -- --nocapture\``,
  },
  parity: {
    kind: "xfail",
    list: "fixtures/parity-xfail.txt",
    frozen: ["fixtures/expected", "fixtures/es-9.5.5", "fixtures/generate.sh", "es-mini/tests/fixture_parity.rs"],
    howTo: () =>
      "Reproduce: `PARITY_VERBOSE=1 cargo test -p es-mini --test fixture_parity -- --nocapture`. " +
      "The recorded responses in fixtures/expected/ came from real Elasticsearch 9.5.5 and are ground truth. " +
      "The comment above the target in the list explains the known cause.",
  },
  goals: {
    kind: "goal",
    list: "ralph/goals.txt",
    frozen: ["ralph/ponytail-ledger.md", "ralph/ledger-check.sh", "bench", "ralph/esql-cold.sh"],
    howTo: () =>
      "If z_old/EXECUTION.md defines this goal, read it for the full definition of done; otherwise the goal " +
      "line is the full definition and overrides that plan's scope assumptions. Make one coherent, verified step " +
      "of progress per iteration (it is fine if the goal needs several iterations). Delete the goal's line " +
      "from ralph/goals.txt only when its done-criteria hold. The loop then runs the goal's Done-check " +
      "command itself and rejects the iteration if it fails, so run that command yourself first.",
  },
};
/** Planning docs: scope changes are reserved for a human (see AGENTS.md "Unattended runs"). */
const ALL_FROZEN = ["z_old", ...Object.values(SUITES).flatMap((s) => s.frozen)];
const LISTS = Object.values(SUITES).map((s) => s.list);
/** Rejections in a row (across targets) that end a suite: something systemic is wrong. */
const MAX_CONSECUTIVE_REJECTIONS = 6;
/** Rejected attempts after which a goal is set aside for the rest of the run. */
const MAX_GOAL_ATTEMPTS = 3;
/** Stop when the temp filesystem (build cache, test scratch) has less free space than this. */
const MIN_FREE_TMP_BYTES = 1.5e9;
/** Marks an iteration in progress, so a restart after a crash can roll it back. */
const IN_PROGRESS = "ralph/.iteration-start";
const SONNET = "anthropic/claude-sonnet-5-5", OPUS = "anthropic/claude-opus-5-5";
const REVIEW_MODEL = `${OPUS}:high`;
/**
 * Model and thinking level per goal, by how hard and how risky the work is. Opus high: Lucene and
 * Elasticsearch formats, durability, concurrency, and new engines (aggregations, ES|QL
 * expressions). Opus medium: new APIs and features with clear Elasticsearch behavior to match.
 * Sonnet: small, well-scoped changes. A goal missing here runs on Opus medium with a warning.
 */
const GOAL_MODELS: Record<string, string> = Object.fromEntries(
  Object.entries({
    [`${OPUS}:high`]: [
      "aggs-framework", "aggs-stats-cardinality", "translog-routing-and-batches",
      "cross-engine-crash-recovery", "flush-by-size-and-idle", "group-commit", "merge-policy",
      "superseded-per-segment", "count-from-postings", "score-from-postings", "points-tree",
      "terms-dict-floor-blocks", "postings-positions", "pfor-exceptions", "doc-values-encodings",
      "unsupported-codecs", "index-sorting", "date-semantics", "per-index-locks",
      "esql-parity-harness", "esql-eval-functions", "esql-stats",
      "disk-writer-completeness", "write-failure-handling", "memory-bounds", "date-formats",
      "esql-es-semantics", "regexp-automaton", "esql-perf-100k", "esql-cold-after-write", "search-memory-walk", "term-query-speed", "range-sort-speed",
    ],
    [`${OPUS}:medium`]: [
      "api-field-caps", "api-by-query", "api-scroll-pit", "api-search-template", "aggs-terms",
      "aggs-histograms", "aggs-ranges-filters", "aggs-top-hits-typed-keys", "aggs-parity",
      "stale-commit-files", "flush-on-shutdown", "range-from-doc-values-memory",
      "disk-float-ranges", "real-deflate", "unreadable-segments", "nested-dynamic-mapping",
      "missing-queries", "settings-validation", "refresh-interval", "multi-index-esql-types",
      "esql-errors-grammar", "esql-source-commands", "mapping-validation", "query-dsl-gaps",
      "ip-and-range-fields", "error-shapes", "esql-conversions", "aggs-gaps",
    ],
    [`${SONNET}:medium`]: [
      "api-node-stubs", "constant-score-boost", "auto-id-format", "alias-date-math",
      "tidy-review-refactors", "perf-small-ceilings", "settings-validation-2", "api-gaps",
      "bench-demo", "readme-current",
    ],
    [`${SONNET}:low`]: ["docs-translog-agents", "test-scratch-cleanup"],
  }).flatMap(([spec, ids]) => ids.map((id) => [id, spec])),
);

/** Model per target; --model overrides all. */
function modelFor(suite: string, id: string): string {
  if (flag("model")) return flag("model")!;
  switch (suite) {
    case "goals": {
      const spec = GOAL_MODELS[id.split(":")[0]];
      if (!spec) console.log(`ralph: no model listed for goal ${id.split(":")[0]}; using Opus medium`);
      return spec ?? `${OPUS}:medium`;
    }
    // Aggregation tests need engine work; the remaining YAML and parity targets are small fixes.
    case "yaml":
      return id.startsWith("aggregations/") ? `${SONNET}:high` : `${SONNET}:medium`;
    default:
      return `${SONNET}:medium`;
  }
}

/** `provider/model-id:thinking`, e.g. `anthropic/claude-opus-5-5:high`; see models.md. */
async function resolveModel(spec: string, runtime: any) {
  const [path, thinking = "low"] = spec.split(":");
  const [provider, ...rest] = path.split("/");
  const model = runtime.getModel(provider, rest.join("/"));
  if (!model) throw new Error(`model ${path} is not available; check ~/.pi/agent settings and credentials`);
  return { model, thinkingLevel: thinking };
}


const repo = resolve(import.meta.dirname, "..");
const args = process.argv.slice(2);
const flag = (name: string) => {
  const i = args.indexOf(`--${name}`);
  return i >= 0 ? args[i + 1] : undefined;
};
const suiteNames = (flag("suites") ?? flag("suite") ?? "yaml").split(",");
for (const s of suiteNames) if (!SUITES[s]) throw new Error(`unknown suite ${s}; choose from ${Object.keys(SUITES).join(", ")}`);
const until = flag("until") ? deadline(flag("until")!) : undefined;
const max = Number(flag("max") ?? (until ? Infinity : 1));
const reviewEvery = Number(flag("review-every") ?? 0);
const timeoutMs = Number(flag("timeout-min") ?? 90) * 60_000;
const dryRun = args.includes("--dry-run");
const unattended = until !== undefined || max > 1;

/** The next occurrence of `HH:MM` or `DAY HH:MM` local time, parsed by GNU date like overnight.sh. */
function deadline(when: string): Date {
  const d = new Date(Number(execFileSync("date", ["-d", when, "+%s"], { encoding: "utf8" })) * 1000);
  if (d <= new Date()) d.setDate(d.getDate() + (/^\d/.test(when) ? 1 : 7));
  return d;
}

const git = (...a: string[]) => execFileSync("git", a, { cwd: repo, encoding: "utf8" }).trim();
const lines = (path: string) =>
  readFileSync(join(repo, path), "utf8")
    .split("\n")
    .map((l) => l.trim())
    .filter((l) => l && !l.startsWith("#"));
const trailer = (model: string) => `Assisted-by: Pi Coding Agent (${model})`;

function report(line: string) {
  console.log(`ralph: ${line}`);
  if (unattended) appendFileSync(join(repo, "ralph/REPORT.md"), `- ${new Date().toISOString()} ${line}\n`);
}

function notify(title: string, body: string) {
  if (!unattended || dryRun) return;
  const script = 'curl -s -H "Authorization: Bearer $(pass agents/ntfy/pi)" -H "Title: $1" -d "$2" https://ntfy.sh/joshmock-pi-agent-events';
  spawnSync("sh", ["-c", script, "notify", title, body], { stdio: "ignore" });
}

const COMMON_RULES = (model: string) => `
- Follow AGENTS.md, including its "Unattended runs" standing orders.
- Never modify ${ALL_FROZEN.map((p) => `\`${p}\``).join(", ")}. Fix the implementation, not tests or runners. If one of those blocks you, stop and record why in ralph/NEEDS_HUMAN.md (git-ignored; append, never commit it).
- Never add lines to ${LISTS.map((p) => `\`${p}\``).join(", ")}.
- Before writing new parsing or resolution logic, search for an existing helper (e.g. es_index::aliases::resolve_read for index expressions) and extend it instead of duplicating it.
- Red-green TDD for behavior changes. Run \`scripts/check.sh\` until it passes. If the sandbox blocks ~/.cargo, prefix cargo commands with CARGO_HOME=/tmp/esm-cargo.
- Commit atomically: one commit per distinct behavior, each describing it and ending with the trailer \`${trailer(model)}\`.
- If the work needs a decision the standing orders reserve for a human, stop without committing and record it in ralph/NEEDS_HUMAN.md.`;

function prompt(suite: Suite, id: string, model: string): string {
  const goal =
    suite.kind === "xfail"
      ? `Make one known-failing test pass: \`${id}\`. Then delete exactly its line from \`${suite.list}\` (and the lines of any other listed tests that now pass). First check what the test needs against the out-of-scope list in z_old/PLAN.md: if it needs anything listed there, stop without committing and record the test and the feature in ralph/NEEDS_HUMAN.md.`
      : `Make progress on this goal from \`${suite.list}\`: ${id}`;
  return `You are one iteration of a Ralph loop in the es-mini repository (an Elasticsearch-compatible server in Rust).

Goal: ${goal}

${suite.howTo(id)}

Rules:${COMMON_RULES(model)}`;
}

function reviewPrompt(since: string, model: string): string {
  return `You are the review iteration of a Ralph loop in the es-mini repository.

Review the commits in \`git log ${since}..HEAD\`, which earlier iterations made to fix one target each. Look for:
1. Duplicated logic that should use or extend an existing helper.
2. Regressions in behavior that no test covers (e.g. a change to shared types breaking another endpoint).
3. Stale \`ponytail:\` comments or comments that no longer match the code.

Fix only real problems, with a failing test first for behavior fixes. Making no commit is a valid outcome.

Rules:${COMMON_RULES(model)}`;
}

/** Runs a test command in its own memory-capped cgroup, so a runaway test is killed alone instead
 * of the OOM killer taking down the loop. Swap is capped too, or the test just swaps. */
const capped = (...cmd: string[]) =>
  spawnSync("systemd-run", ["--user", "--scope", "-q", "-p", "MemoryMax=10G", "-p", "MemorySwapMax=0", "--", ...cmd], { cwd: repo, stdio: "inherit" });

/** Returns the reason an iteration is rejected, or undefined if it holds. */
function verify(
  start: string,
  model: string,
  opts: { requireCommit: boolean; mustRemove?: [string, string]; goal?: [string, string] },
) {
  if (git("status", "--porcelain")) return "uncommitted changes left behind";
  const head = git("rev-parse", "HEAD");
  if (head === start) return opts.requireCommit ? "no commit made" : undefined;
  const messages = git("log", "--format=%B%x00", `${start}..HEAD`).split("\0").filter((m) => m.trim());
  if (messages.some((m) => !m.includes(trailer(model)))) return `commit lacks trailer "${trailer(model)}"`;
  const frozen = git("diff", "--name-only", start, "HEAD", "--", ...ALL_FROZEN);
  if (frozen) return `modified frozen paths: ${frozen}`;
  for (const list of LISTS) {
    const before = new Set(git("show", `${start}:${list}`).split("\n").map((l) => l.trim()));
    const added = lines(list).filter((l) => !before.has(l));
    if (added.length) return `added entries to ${list}: ${added.join(", ")}`;
  }
  if (opts.mustRemove && lines(opts.mustRemove[0]).includes(opts.mustRemove[1])) return "target still listed";
  // A goal whose line was deleted must pass its own done-check, run here rather than trusted.
  if (opts.goal && !lines(opts.goal[0]).includes(opts.goal[1])) {
    const cmd = opts.goal[1].match(/Done-check: `([^`]+)`/)?.[1];
    if (!cmd) return "goal removed, but it has no Done-check to verify it";
    if (capped("sh", "-c", cmd).status !== 0) return `goal removed, but its Done-check failed: ${cmd}`;
  }
  const check = capped("scripts/check.sh");
  if (check.status !== 0) return "scripts/check.sh failed";
  // A goal's Gate runs on every iteration (with START set to where it began), e.g. a benchmark
  // that keeps only changes that make es-mini faster.
  const gate = opts.goal?.[1].match(/Gate: `([^`]+)`/)?.[1];
  if (gate && capped("env", `START=${start}`, "sh", "-c", gate).status !== 0) return `Gate failed: ${gate}`;
  return undefined;
}

/** Runs one agent session with a time limit; returns the model name it used. */
async function runAgent(makePrompt: (model: string) => string, spec: string): Promise<string> {
  // Imported lazily so --dry-run works without installing the SDK.
  const { createAgentSession, SessionManager, ModelRuntime } = await import("@earendil-works/pi-coding-agent");
  const modelRuntime = await ModelRuntime.create();
  const { model: chosen, thinkingLevel } = await resolveModel(spec, modelRuntime);
  console.log(`ralph: model ${spec}`);
  const { session } = await createAgentSession({
    cwd: repo,
    sessionManager: SessionManager.inMemory(),
    modelRuntime,
    model: chosen,
    thinkingLevel: thinkingLevel as any,
  });
  const model = session.model?.name ?? session.model?.id ?? "unknown model";
  session.subscribe((event) => {
    if (event.type === "message_update" && event.assistantMessageEvent.type === "text_delta") {
      process.stdout.write(event.assistantMessageEvent.delta);
    }
  });
  let timer: ReturnType<typeof setTimeout> | undefined;
  let hard: ReturnType<typeof setTimeout> | undefined;
  // Abort at the limit; if the session does not wind down within 5 more minutes, give up on it.
  const deadline = new Promise<never>((_, reject) => {
    timer = setTimeout(() => {
      console.log(`\nralph: iteration exceeded ${timeoutMs / 60_000} minutes; aborting`);
      void session.abort();
      hard = setTimeout(() => reject(new Error("session did not stop after abort")), 5 * 60_000);
    }, timeoutMs);
  });
  try {
    await Promise.race([session.prompt(makePrompt(model)), deadline]);
  } finally {
    clearTimeout(timer);
    clearTimeout(hard);
    session.dispose();
  }
  return model;
}

/** Removes what iterations leave outside git: Elasticsearch containers and test scratch dirs. */
function cleanEnvironment() {
  spawnSync("sh", ["-c", "docker ps -aq --filter name=es-mini | xargs -r docker rm -f"], { stdio: "ignore" });
  spawnSync("sh", ["-c", "rm -rf /tmp/esm-save-* /tmp/esm-persist-* /tmp/esm-disk-meta-* /tmp/esm-incr-* /tmp/esm-tlog-* /tmp/esm-timing"], { stdio: "ignore" });
}

/** Discards everything since `start`: commits, edits, and untracked files (git-ignored ones stay). */
function rollBack(start: string) {
  git("reset", "--hard", start);
  git("clean", "-fdq");
  cleanEnvironment();
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

/** Bytes free on the filesystem holding /tmp. */
function freeTmpBytes(): number {
  const out = spawnSync("df", ["-B1", "--output=avail", "/tmp"], { encoding: "utf8" }).stdout ?? "";
  return Number(out.trim().split("\n").pop()) || Infinity;
}

/** Runs one iteration and rolls it back if verification fails or the agent errors; returns
 * whether it was accepted. */
async function iterate(label: string, makePrompt: (m: string) => string, opts: Parameters<typeof verify>[2], spec: string) {
  const start = git("rev-parse", "HEAD");
  if (dryRun) {
    console.log(makePrompt("<model from the session>"));
    return false;
  }
  writeFileSync(join(repo, IN_PROGRESS), start);
  let failure: string | undefined;
  let model = "";
  try {
    model = await runAgent(makePrompt, spec);
  } catch (e) {
    failure = `agent error: ${e instanceof Error ? e.message : String(e)}`;
  }
  failure ??= verify(start, model, opts);
  if (failure) {
    rollBack(start);
    rmSync(join(repo, IN_PROGRESS), { force: true });
    report(`rejected ${label}: ${failure}`);
    // Back off on agent errors (rate limits, outages) instead of burning through targets.
    if (failure.startsWith("agent error")) await sleep(2 * 60_000);
    return false;
  }
  rmSync(join(repo, IN_PROGRESS), { force: true });
  cleanEnvironment();
  const made = git("log", "--oneline", `${start}..HEAD`);
  report(`accepted ${label}${made ? `: ${made.split("\n").join("; ")}` : " (no changes)"}`);
  return true;
}

if (!dryRun && existsSync(join(repo, IN_PROGRESS))) {
  const start = readFileSync(join(repo, IN_PROGRESS), "utf8").trim();
  report(`recovering: rolling back an iteration interrupted by a crash to ${start.slice(0, 8)}`);
  rollBack(start);
  rmSync(join(repo, IN_PROGRESS), { force: true });
}
if (git("status", "--porcelain")) throw new Error("working tree must be clean before starting");
if (!dryRun && unattended && capped("scripts/check.sh").status !== 0) {
  notify("ralph: not started", "scripts/check.sh fails on main; fix it first.");
  console.error("scripts/check.sh fails before the first iteration");
  process.exit(3);
}
report(`started: suites ${suiteNames.join(",")}, max ${max}, until ${until?.toLocaleString() ?? "-"}`);

let done = 0;
let accepted = 0;
let lastReview = git("rev-parse", "HEAD");
let stopReason = "all suites finished";
outer: for (const name of suiteNames) {
  const suite = SUITES[name];
  const skipped = new Set<string>();
  const attempts = new Map<string, number>();
  let rejections = 0;
  while (true) {
    if (done >= max) { stopReason = `reached --max ${max}`; break outer; }
    if (until && new Date() >= until) { stopReason = "reached --until"; break outer; }
    if (!dryRun && freeTmpBytes() < MIN_FREE_TMP_BYTES) {
      stopReason = "less than 1.5 GB free in /tmp";
      break outer;
    }
    const id = flag("target") ?? lines(suite.list).find((t) => !skipped.has(t));
    if (!id) { report(`${name}: no targets left`); break; }
    done++;
    const ok = await iterate(`${name}: ${id}`, (m) => prompt(suite, id, m), {
      requireCommit: true,
      mustRemove: suite.kind === "xfail" ? [suite.list, id] : undefined,
      goal: suite.kind === "goal" ? [suite.list, id] : undefined,
    }, modelFor(name, id));
    if (dryRun) break outer;
    if (ok) {
      rejections = 0;
      accepted++;
      if (suite.kind === "xfail") skipped.add(id); // a still-listed id would be a verification failure
    } else if (++rejections >= MAX_CONSECUTIVE_REJECTIONS) {
      report(`${name}: stopping after ${rejections} rejections in a row`);
      break;
    } else if (suite.kind === "xfail") {
      skipped.add(id);
    } else {
      // Goals are retried a few times, then set aside so one hard goal cannot block the rest.
      const n = (attempts.get(id) ?? 0) + 1;
      attempts.set(id, n);
      if (n >= MAX_GOAL_ATTEMPTS) {
        report(`${name}: setting aside after ${n} rejected attempts: ${id.slice(0, 80)}`);
        skipped.add(id);
      }
    }
    if (reviewEvery && accepted > 0 && accepted % reviewEvery === 0 && git("rev-parse", "HEAD") !== lastReview) {
      const since = lastReview;
      await iterate(`review of ${since.slice(0, 8)}..HEAD`, (m) => reviewPrompt(since, m), { requireCommit: false }, flag("model") ?? REVIEW_MODEL);
      lastReview = git("rev-parse", "HEAD");
    }
  }
}
const counts = LISTS.map((l) => `${l}: ${lines(l).length}`).join(", ");
report(`finished (${stopReason}): ${accepted}/${done} iterations accepted; ${counts}`);
notify("ralph finished", `${stopReason}. ${accepted}/${done} accepted. ${counts}. See ralph/REPORT.md and ralph/NEEDS_HUMAN.md.`);
// Exit code 3 tells ralph/overnight.sh not to restart: the run ended on purpose.
process.exitCode = 3;
