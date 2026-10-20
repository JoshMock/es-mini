// Ralph loop: each iteration gives one fresh pi agent session exactly one known-failing test
// to fix, then verifies the result independently and rolls back anything that does not hold.
//
//   node ralph.ts [--suite yaml] [--max N] [--target ID] [--dry-run]
//
// --dry-run runs the selection and verification steps without starting an agent.
import { execFileSync, spawnSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { join, resolve } from "node:path";

interface Suite {
  /** Known-failing test ids, one per line; `#` lines are comments. */
  xfail: string;
  /** Paths the agent must never modify: test definitions, the runner, and the scope list. */
  frozen: string[];
  /** Command that runs the suite and fails on any unlisted failure or listed pass. */
  check: string[];
  /** How to run just one test, shown to the agent. */
  runOne: (id: string) => string;
}

const SUITES: Record<string, Suite> = {
  yaml: {
    xfail: "tests/yaml-xfail.txt",
    frozen: ["tests/yaml", "tests/yaml-out-of-scope.txt", "es-mini/tests/yaml.rs"],
    check: ["scripts/check.sh"],
    runOne: (id) => `YAML_FILTER='${id}' YAML_VERBOSE=1 cargo test -p es-mini --test yaml -- --nocapture`,
  },
};

const repo = resolve(import.meta.dirname, "..");
const args = process.argv.slice(2);
const flag = (name: string) => {
  const i = args.indexOf(`--${name}`);
  return i >= 0 ? args[i + 1] : undefined;
};
const suite = SUITES[flag("suite") ?? "yaml"];
if (!suite) throw new Error(`unknown suite; choose one of ${Object.keys(SUITES).join(", ")}`);
const max = Number(flag("max") ?? 1);
const dryRun = args.includes("--dry-run");

const git = (...a: string[]) => execFileSync("git", a, { cwd: repo, encoding: "utf8" }).trim();
const xfail = () =>
  readFileSync(join(repo, suite.xfail), "utf8")
    .split("\n")
    .filter((l) => l.trim() && !l.startsWith("#"));

function prompt(id: string, model: string): string {
  return `You are one iteration of a Ralph loop in the es-mini repository (an Elasticsearch-compatible server in Rust).

Goal: make exactly one known-failing test pass: \`${id}\`

Rules:
1. Do not modify anything under ${suite.frozen.map((p) => `\`${p}\``).join(", ")}. Fix the implementation, not the test or its runner. If a runner bug blocks the test, stop and explain it instead.
2. Fix only this test. Keep the change minimal and follow AGENTS.md.
3. Reproduce first: \`${suite.runOne(id)}\`
4. Delete exactly the line \`${id}\` from \`${suite.xfail}\`. If other listed tests now pass too, delete their lines as well; never add lines.
5. Run \`scripts/check.sh\` until it passes. If the sandbox blocks ~/.cargo, prefix commands with CARGO_HOME=/tmp/esm-cargo.
6. Make one git commit with a message describing the behavior fixed, ending with the trailer \`${trailer(model)}\`.
7. If the fix needs work far outside this test's scope, stop without committing and explain why.`;
}

const trailer = (model: string) => `Assisted-by: Pi Coding Agent (${model})`;

/** Returns the reason the iteration is rejected, or undefined if it holds. */
function verify(start: string, id: string, before: string[], model: string): string | undefined {
  if (git("status", "--porcelain")) return "uncommitted changes left behind";
  if (git("rev-parse", "HEAD") === start) return "no commit made";
  const messages = git("log", "--format=%B%x00", `${start}..HEAD`).split("\0").filter((m) => m.trim());
  if (messages.some((m) => !m.includes(trailer(model)))) return `commit lacks trailer "${trailer(model)}"`;
  const frozen = git("diff", "--name-only", start, "HEAD", "--", ...suite.frozen);
  if (frozen) return `modified frozen paths: ${frozen}`;
  const after = new Set(xfail());
  if (after.has(id)) return "target still listed in the xfail file";
  const added = [...after].filter((t) => !before.includes(t));
  if (added.length) return `added xfail entries: ${added.join(", ")}`;
  const check = spawnSync(suite.check[0], suite.check.slice(1), { cwd: repo, stdio: "inherit" });
  if (check.status !== 0) return `${suite.check.join(" ")} failed`;
  return undefined;
}

/** Runs one agent session; returns the model name it used. */
async function runAgent(id: string): Promise<string> {
  // Imported lazily so --dry-run works without installing the SDK.
  const { createAgentSession, SessionManager } = await import("@earendil-works/pi-coding-agent");
  const { session } = await createAgentSession({ cwd: repo, sessionManager: SessionManager.inMemory() });
  const model = session.model?.name ?? session.model?.id ?? "unknown model";
  session.subscribe((event) => {
    if (event.type === "message_update" && event.assistantMessageEvent.type === "text_delta") {
      process.stdout.write(event.assistantMessageEvent.delta);
    }
  });
  try {
    await session.prompt(prompt(id, model));
  } finally {
    session.dispose();
  }
  return model;
}

if (git("status", "--porcelain")) throw new Error("working tree must be clean before starting");
const skipped = new Set<string>();
for (let i = 1; i <= max; i++) {
  const before = xfail();
  const id = flag("target") ?? before.find((t) => !skipped.has(t));
  if (!id) {
    console.log("ralph: nothing left to fix");
    break;
  }
  const start = git("rev-parse", "HEAD");
  console.log(`\nralph: iteration ${i}/${max}: ${id}`);
  if (dryRun) {
    const model = "<model from the session>";
    console.log(prompt(id, model));
    console.log(`ralph: dry run; verification would report: ${verify(start, id, before, model) ?? "ok"}`);
    break;
  }
  const model = await runAgent(id);
  const failure = verify(start, id, before, model);
  if (failure) {
    console.log(`ralph: rejected (${failure}); resetting to ${start.slice(0, 8)}`);
    // Only discards this iteration's own commits and edits; the tree was clean at start.
    git("reset", "--hard", start);
    skipped.add(id);
  } else {
    console.log(`ralph: accepted ${git("log", "--oneline", "-1")}`);
  }
}
