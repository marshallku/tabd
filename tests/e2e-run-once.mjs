/**
 * E2E test for `ai-browser run-once`.
 *  - run-once spawns an isolated daemon (does not collide with the main one)
 *  - the main daemon survives a run-once invocation
 *  - run-once refuses to wrap meta subcommands (daemon/run-once/mcp/repl/help)
 *  - the ephemeral daemon is torn down after the single action finishes
 *
 * Usage:
 *   node tests/e2e-run-once.mjs
 */

import { spawnSync } from "node:child_process";
import { mkdtempSync, rmSync, existsSync } from "node:fs";
import { tmpdir, homedir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const repo = dirname(here);
const bin = join(repo, "bin", "ai-browser.js");
const chromiumPath = `${homedir()}/.cache/ms-playwright/chromium-1208/chrome-linux64/chrome`;

const runtimeDir = mkdtempSync(join(tmpdir(), "ai-browser-runonce-"));

let failed = false;
const check = (cond, label) => {
  if (cond) console.log(`[PASS] ${label}`);
  else { console.error(`[FAIL] ${label}`); failed = true; }
};

const cli = (args, extraEnv = {}) =>
  spawnSync(process.execPath, [bin, ...args], {
    encoding: "utf8",
    env: {
      ...process.env,
      XDG_RUNTIME_DIR: runtimeDir,
      BROWSER_RUNTIME: "playwright",
      BROWSER_HEADLESS: "1",
      BROWSER_EXECUTABLE: chromiumPath,
      ...extraEnv,
    },
    timeout: 120_000,
  });

const ensureClean = () => {
  cli(["daemon", "stop"]);
  spawnSync("sleep", ["0.3"]);
};

try {
  ensureClean();

  // 1. Boot the MAIN daemon (default XDG_RUNTIME_DIR path)
  let r = cli(["navigate", "https://example.com"]);
  check(r.status === 0, "main daemon spawned + navigated");
  r = cli(["daemon", "status"]);
  check(/status : running/.test(r.stdout), "main daemon running");

  // 2. Recursion guard: refuse to wrap a meta subcommand
  r = cli(["run-once", "daemon", "status"]);
  check(
    r.status === 2 && /refusing to wrap/i.test(r.stderr),
    `run-once refuses 'daemon' (status=${r.status})`
  );
  r = cli(["run-once", "run-once", "navigate", "https://example.com"]);
  check(
    r.status === 2 && /refusing to wrap/i.test(r.stderr),
    `run-once refuses recursive 'run-once' (status=${r.status})`
  );
  r = cli(["run-once"]);
  check(
    r.status === 2 && /requires a subcommand/i.test(r.stderr),
    `run-once with no subcommand exits 2`
  );

  // 3. Successful run-once: navigate inside the ephemeral daemon
  r = cli(["run-once", "navigate", "https://example.com", "--json"]);
  check(r.status === 0, `run-once navigate (status=${r.status}, err=${r.stderr.slice(0,200)})`);

  // 4. MAIN daemon must still be alive AND its tab must NOT have been
  //    affected by the ephemeral daemon (state isolation).
  r = cli(["daemon", "status"]);
  check(/status : running/.test(r.stdout), "main daemon survived run-once");

  // The main daemon's tab is still on example.com. Use list-tabs to check
  // exactly one tab — the ephemeral daemon's tabs are gone with it.
  r = cli(["list-tabs", "--json"]);
  let tabs;
  try { tabs = JSON.parse(r.stdout); } catch {}
  check(
    Array.isArray(tabs) && tabs.length === 1 && /example\.com/.test(tabs[0]?.url ?? ""),
    `main daemon retains exactly one example.com tab (got ${JSON.stringify(tabs)})`
  );

  // 5. Repeat run-once a couple more times — none of them should affect
  //    the main daemon.
  for (let i = 0; i < 2; i++) {
    const ri = cli(["run-once", "eval", `1 + ${i}`, "--json"]);
    check(ri.status === 0, `run-once eval #${i} (status=${ri.status})`);
  }
  r = cli(["daemon", "status"]);
  check(
    /status : running/.test(r.stdout),
    "main daemon still alive after multiple run-once invocations"
  );

  console.log(`\n[test] ${failed ? "SOME TESTS FAILED" : "ALL TESTS PASSED"}`);
} catch (err) {
  console.error("[FATAL]", err);
  failed = true;
} finally {
  ensureClean();
  rmSync(runtimeDir, { recursive: true, force: true });
}

process.exit(failed ? 1 : 0);
