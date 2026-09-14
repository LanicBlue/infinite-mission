// Test-suite wrapper with containment. A wedged test run must never outlive
// this script: node --test's parent does not reliably tear its per-file
// children down on TERM (an orphaned group once spun four cores for hours),
// so the runner gets its own process group and three fences —
//   1. --test-timeout   a stuck individual test is failed after 60s;
//   2. --test-force-exit the runner exits even with pending event-loop work;
//   3. wall clock below  a wedged runner is TERM→wait→SIGKILLed as a group.
// Usage: node tests/run.mjs [extra node --test flags or file globs]
import { spawn } from "node:child_process";
import { readdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const args = process.argv.slice(2).filter((arg) => !arg.endsWith(".mjs"));
const files = process.argv.slice(2).filter((arg) => arg.endsWith(".mjs"));
const testFiles = (files.length ? files : readdirSync(here)
  .filter((name) => name.endsWith(".test.mjs"))
  .map((name) => join(here, name)));

const WALL_CLOCK_MS = 5 * 60_000;
const child = spawn(process.execPath, [
  "--test",
  "--test-timeout=60000",
  "--test-force-exit",
  ...args,
  ...testFiles,
], { stdio: "inherit", detached: true });

const alive = () => {
  if (child.pid === undefined) return false;
  try { process.kill(-child.pid, 0); return true; } catch { return false; }
};
const killGroup = (signal) => {
  if (child.pid !== undefined) { try { process.kill(-child.pid, signal); } catch { /* gone */ } }
};
// Signal handlers cannot await an escalation ladder; hard-kill immediately.
for (const signal of ["SIGINT", "SIGTERM"]) {
  process.on(signal, () => { killGroup("SIGKILL"); process.exit(signal === "SIGINT" ? 130 : 143); });
}

const wall = setTimeout(() => {
  console.error(`[tests/run] wall clock exceeded ${WALL_CLOCK_MS}ms — killing the test group`);
  killGroup("SIGTERM");
  setTimeout(() => { if (alive()) killGroup("SIGKILL"); }, 5_000).unref();
}, WALL_CLOCK_MS);
wall.unref();

const code = await new Promise((resolve) => {
  child.on("close", (exitCode, signal) => resolve(signal !== null ? 124 : (exitCode ?? 1)));
});
process.exit(code);
