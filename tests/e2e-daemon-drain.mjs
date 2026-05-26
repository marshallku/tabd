/**
 * E2E test for in-flight drain + daemon.health + daemon control gates.
 *  - daemon.health reports inflight count + accepting flag
 *  - daemon.shutdown drains in-flight work before exiting
 *  - new non-control requests are rejected once drain begins
 *  - drain timeout forces context teardown (real cancel)
 *
 * Usage:
 *   node tests/e2e-daemon-drain.mjs
 */

import { spawnSync } from "node:child_process";
import { mkdtempSync, rmSync, existsSync, readFileSync } from "node:fs";
import { tmpdir, homedir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { connectClient } from "../dist/cli/daemonClient.js";
import { getDaemonPaths } from "../dist/shared/daemonPaths.js";

const here = dirname(fileURLToPath(import.meta.url));
const repo = dirname(here);
const bin = join(repo, "bin", "ai-browser.js");
const chromiumPath = `${homedir()}/.cache/ms-playwright/chromium-1208/chrome-linux64/chrome`;

const runtimeDir = mkdtempSync(join(tmpdir(), "ai-browser-drain-"));
process.env.XDG_RUNTIME_DIR = runtimeDir;
process.env.BROWSER_RUNTIME = "playwright";
process.env.BROWSER_HEADLESS = "1";
process.env.BROWSER_EXECUTABLE = chromiumPath;

let failed = false;
const check = (cond, label) => {
  if (cond) console.log(`[PASS] ${label}`);
  else { console.error(`[FAIL] ${label}`); failed = true; }
};

const cli = (...args) =>
  spawnSync(process.execPath, [bin, ...args], {
    encoding: "utf8",
    env: process.env,
    timeout: 60_000,
  });

const ensureClean = () => {
  cli("daemon", "stop");
  // Give the daemon a beat to fully exit on a clean stop
  spawnSync("sleep", ["0.3"]);
};

const slowPage = (id) =>
  "data:text/html," + encodeURIComponent(
    `<html><body><h1>page-${id}</h1>` +
    `<script>const t=performance.now();while(performance.now()-t<2200){}</script>` +
    `</body></html>`
  );

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

try {
  ensureClean();

  // 1. daemon.health is answered even before any browser work
  let r = cli("navigate", "about:blank", "--json");
  check(r.status === 0, "daemon spawned");
  r = cli("daemon", "health");
  check(r.status === 0, `daemon health (status=${r.status})`);
  let health;
  try { health = JSON.parse(r.stdout); } catch {}
  check(
    health && typeof health.uptimeMs === "number" && health.accepting === true,
    `health payload ok (accepting=${health?.accepting}, uptime=${health?.uptimeMs}ms)`
  );

  // 2. inflight counter increments while a slow action is running
  const { socketPath } = getDaemonPaths();
  const clientA = await connectClient(socketPath);
  const clientB = await connectClient(socketPath);

  const slow = clientA.send("tabs.navigate", {
    tabId: 1,
    url: slowPage("drain"),
  });
  // Wait briefly so the action lands in the queue
  await sleep(50);
  let h = await clientB.send("daemon.health", {});
  check(
    h.success && h.data?.inflight === 1,
    `inflight=1 during slow navigate (got ${h.data?.inflight}, success=${h.success})`
  );

  // Let the slow action finish naturally
  const slowRes = await slow;
  check(slowRes.success, `slow navigate eventually succeeded`);

  // inflight returns to 0
  h = await clientB.send("daemon.health", {});
  check(
    h.success && h.data?.inflight === 0,
    `inflight=0 after slow navigate (got ${h.data?.inflight})`
  );

  clientA.close();
  clientB.close();

  // 3. Graceful shutdown drains in-flight: send a slow navigate, then
  //    daemon.shutdown immediately. The slow action must complete.
  const clientC = await connectClient(socketPath);
  const clientD = await connectClient(socketPath);
  const dStart = Date.now();
  const slow2 = clientC
    .send("tabs.navigate", { tabId: 1, url: slowPage("drain2") })
    .then((res) => ({ res, at: Date.now() - dStart }));
  await sleep(50);
  // Issue shutdown via clientD (must respond immediately)
  const shutRes = await clientD.send("daemon.shutdown", {});
  check(
    shutRes.success && shutRes.data?.stopping === true,
    "daemon.shutdown returns stopping=true"
  );

  // 3-race. The accepting gate must close on the SAME message that returns
  //         daemon.shutdown — no grace window where new work slips in. Fire
  //         a non-control request immediately after shutdown response.
  const raceReject = await clientD.send("tabs.list", {});
  check(
    raceReject.success === false && /drain/i.test(raceReject.error ?? ""),
    `post-shutdown request rejected with drain message (success=${raceReject.success}, err="${raceReject.error}")`
  );

  // 3a. While draining, daemon.health must still answer (listener stays open
  //     until drain completes). A NEW connection should also succeed.
  await sleep(80);
  const newObserver = await connectClient(socketPath);
  const drainHealth = await newObserver.send("daemon.health", {});
  check(
    drainHealth.success && drainHealth.data?.accepting === false,
    `during drain: new observer sees accepting=false (got accepting=${drainHealth.data?.accepting})`
  );
  check(
    drainHealth.success && drainHealth.data?.inflight === 1,
    `during drain: inflight=1 visible to new observer (got ${drainHealth.data?.inflight})`
  );

  // 3b. Non-control request during drain must be rejected with the explicit
  //     drain message — not a connection error.
  const reject = await newObserver.send("tabs.list", {});
  check(
    reject.success === false && /drain/i.test(reject.error ?? ""),
    `non-control rejected during drain (success=${reject.success}, err="${reject.error}")`
  );
  newObserver.close();

  // Slow action should still finish before the daemon exits
  const slow2Done = await slow2;
  check(
    slow2Done.res.success === true,
    `in-flight action completed during drain (success=${slow2Done.res.success})`
  );
  check(
    slow2Done.at >= 1500 && slow2Done.at <= 5000,
    `drain wall-clock ~ slow action time (got ${slow2Done.at}ms)`
  );

  clientC.close();
  clientD.close();
  await sleep(500);

  // 4. Drain timeout forces real cancel — set DRAIN_TIMEOUT_MS short
  //    relative to action duration, expect the in-flight action to be
  //    rejected with a cancel-like error rather than completing.
  ensureClean();
  const drainTimeoutDaemon = spawnSync(process.execPath, [bin, "navigate", "about:blank"], {
    encoding: "utf8",
    env: { ...process.env, AI_BROWSER_DRAIN_TIMEOUT_MS: "500" },
    timeout: 60_000,
  });
  check(drainTimeoutDaemon.status === 0, "daemon with short drain timeout spawned");

  const clientE = await connectClient(socketPath);
  const clientF = await connectClient(socketPath);
  // Use a wait_for_url that will never resolve — pure timeout target
  const longWait = clientE.send("wait.url", {
    pattern: "https://never-resolves.invalid/*",
    patternType: "glob",
    timeout: 60_000, // user-specified large timeout
  });
  await sleep(80);
  // Shutdown — drain timeout (500ms) will elapse before wait_for_url returns,
  // forcing the context to close and the wait to reject.
  await clientF.send("daemon.shutdown", {});
  const waitRes = await longWait.catch((err) => ({ success: false, error: String(err) }));
  check(
    !waitRes.success,
    `forced-cancel wait_for_url rejected (success=${waitRes.success})`
  );
  // Error should be something specific to the context closing
  check(
    /closed|cancel|terminat|target|connection|destroyed/i.test(waitRes.error ?? ""),
    `cancel error mentions context close / target / connection (got "${waitRes.error}")`
  );

  clientE.close();
  clientF.close();

  console.log(`\n[test] ${failed ? "SOME TESTS FAILED" : "ALL TESTS PASSED"}`);
} catch (err) {
  console.error("[FATAL]", err);
  failed = true;
} finally {
  ensureClean();
  rmSync(runtimeDir, { recursive: true, force: true });
}

process.exit(failed ? 1 : 0);
