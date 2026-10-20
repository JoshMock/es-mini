// Ralph loop: each iteration gives one fresh pi agent session one target (a known-failing test
// or a goal), then verifies the result independently and rolls back anything that does not hold.
//
//   node ralph.ts [--suites yaml,parity,goals] [--max N] [--until HH:MM] [--target ID]
//                 [--review-every N] [--timeout-min N] [--dry-run]
//
// Suites run in the given order; a suite ends when it runs out of targets or rejects 3
// iterations in a row. Without --max or --until, one iteration runs. --dry-run shows the next
// target and prompt without starting an agent. Unattended runs append to ralph/REPORT.md, read
// agent escalations from ralph/NEEDS_HUMAN.md (both git-ignored), and send a notification at the end.
import { execFileSync, spawnSync } from "node:child_process";
import { appendFileSync, readFileSync } from "node:fs";
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
    frozen: [],
    howTo: () =>
      "Read z_old/EXECUTION.md for the task's full definition of done. Make one coherent, verified step " +
      "of progress per iteration (it is fine if the goal needs several iterations). Delete the goal's line " +
      "from ralph/goals.txt only when its done-criteria hold, verified by a test you ran.",
  },
};
const ALL_FROZEN = Object.values(SUITES).flatMap((s) => s.frozen);
const LISTS = Object.values(SUITES).map((s) => s.list);
const MAX_CONSECUTIVE_REJECTIONS = 3;

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

/** The next occurrence of HH:MM local time. */
function deadline(hhmm: string): Date {
  const [h, m] = hhmm.split(":").map(Number);
  const d = new Date();
  d.setHours(h, m, 0, 0);
  if (d <= new Date()) d.setDate(d.getDate() + 1);
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
      ? `Make one known-failing test pass: \`${id}\`. Then delete exactly its line from \`${suite.list}\` (and the lines of any other listed tests that now pass).`
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

/** Returns the reason an iteration is rejected, or undefined if it holds. */
function verify(start: string, model: string, opts: { requireCommit: boolean; mustRemove?: [string, string] }) {
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
  const check = spawnSync("scripts/check.sh", [], { cwd: repo, stdio: "inherit" });
  if (check.status !== 0) return "scripts/check.sh failed";
  return undefined;
}

/** Runs one agent session with a time limit; returns the model name it used. */
async function runAgent(makePrompt: (model: string) => string): Promise<string> {
  // Imported lazily so --dry-run works without installing the SDK.
  const { createAgentSession, SessionManager } = await import("@earendil-works/pi-coding-agent");
  const { session } = await createAgentSession({ cwd: repo, sessionManager: SessionManager.inMemory() });
  const model = session.model?.name ?? session.model?.id ?? "unknown model";
  session.subscribe((event) => {
    if (event.type === "message_update" && event.assistantMessageEvent.type === "text_delta") {
      process.stdout.write(event.assistantMessageEvent.delta);
    }
  });
  const timer = setTimeout(() => {
    console.log(`\nralph: iteration exceeded ${timeoutMs / 60_000} minutes; aborting`);
    void session.abort();
  }, timeoutMs);
  try {
    await session.prompt(makePrompt(model));
  } finally {
    clearTimeout(timer);
    session.dispose();
  }
  return model;
}

/** Runs one iteration and rolls it back if verification fails; returns whether it was accepted. */
async function iterate(label: string, makePrompt: (m: string) => string, opts: Parameters<typeof verify>[2]) {
  const start = git("rev-parse", "HEAD");
  if (dryRun) {
    console.log(makePrompt("<model from the session>"));
    return false;
  }
  const model = await runAgent(makePrompt);
  const failure = verify(start, model, opts);
  if (failure) {
    // Only discards this iteration's own commits and edits; the tree was clean at start.
    git("reset", "--hard", start);
    report(`rejected ${label}: ${failure}`);
    return false;
  }
  const made = git("log", "--oneline", `${start}..HEAD`);
  report(`accepted ${label}${made ? `: ${made.split("\n").join("; ")}` : " (no changes)"}`);
  return true;
}

if (git("status", "--porcelain")) throw new Error("working tree must be clean before starting");
if (!dryRun && unattended && spawnSync("scripts/check.sh", [], { cwd: repo, stdio: "inherit" }).status !== 0) {
  notify("ralph: not started", "scripts/check.sh fails on main; fix it first.");
  throw new Error("scripts/check.sh fails before the first iteration");
}
report(`started: suites ${suiteNames.join(",")}, max ${max}, until ${until?.toLocaleString() ?? "-"}`);

let done = 0;
let accepted = 0;
let lastReview = git("rev-parse", "HEAD");
let stopReason = "all suites finished";
outer: for (const name of suiteNames) {
  const suite = SUITES[name];
  const skipped = new Set<string>();
  let rejections = 0;
  while (true) {
    if (done >= max) { stopReason = `reached --max ${max}`; break outer; }
    if (until && new Date() >= until) { stopReason = "reached --until"; break outer; }
    const id = flag("target") ?? lines(suite.list).find((t) => !skipped.has(t));
    if (!id) { report(`${name}: no targets left`); break; }
    done++;
    const ok = await iterate(`${name}: ${id}`, (m) => prompt(suite, id, m), {
      requireCommit: true,
      mustRemove: suite.kind === "xfail" ? [suite.list, id] : undefined,
    });
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
    } // goals are ordered: a rejected goal is retried, never skipped
    if (reviewEvery && accepted > 0 && accepted % reviewEvery === 0 && git("rev-parse", "HEAD") !== lastReview) {
      const since = lastReview;
      await iterate(`review of ${since.slice(0, 8)}..HEAD`, (m) => reviewPrompt(since, m), { requireCommit: false });
      lastReview = git("rev-parse", "HEAD");
    }
  }
}
const counts = LISTS.map((l) => `${l}: ${lines(l).length}`).join(", ");
report(`finished (${stopReason}): ${accepted}/${done} iterations accepted; ${counts}`);
notify("ralph finished", `${stopReason}. ${accepted}/${done} accepted. ${counts}. See ralph/REPORT.md and ralph/NEEDS_HUMAN.md.`);
