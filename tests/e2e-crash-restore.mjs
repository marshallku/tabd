/**
 * E2E test for crash restore.
 *  - Navigate to a real URL, set a cookie
 *  - Kill the Chromium process forcibly (SIGKILL)
 *  - Wait for the supervisor's restart-with-snapshot to complete
 *  - Verify URL list is restored and cookie is back (non-persistent path)
 *
 * Usage:
 *   node tests/e2e-crash-restore.mjs
 */

import { spawnSync, execSync } from "node:child_process";
import { mkdtempSync, rmSync, existsSync } from "node:fs";
import { tmpdir, homedir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { connectClient } from "../dist/cli/daemonClient.js";
import { getDaemonPaths } from "../dist/shared/daemonPaths.js";

const here = dirname(fileURLToPath(import.meta.url));
const repo = dirname(here);
const bin = join(repo, "bin", "ai-browser.js");
const chromiumPath = `${homedir()}/.cache/ms-playwright/chromium-1208/chrome-linux64/chrome`;

const runtimeDir = mkdtempSync(join(tmpdir(), "ai-browser-crash-"));
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
  spawnSync("sleep", ["0.4"]);
};

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function findChromiumPids(daemonPid) {
  try {
    // Get the immediate children of the daemon process — Chromium is among them.
    const out = execSync(`pgrep -P ${daemonPid}`, { encoding: "utf8" });
    return out.split("\n").map((s) => s.trim()).filter(Boolean).map(Number);
  } catch {
    return [];
  }
}

try {
  ensureClean();

  // Boot the daemon
  let r = cli("navigate", "about:blank", "--json");
  check(r.status === 0, "daemon spawned");

  const { socketPath } = getDaemonPaths();
  const client = await connectClient(socketPath);

  // 1. Set up state: navigate to a data URL with a known title + set a cookie
  let res = await client.send("tabs.navigate", {
    tabId: 1,
    url: "data:text/html;charset=utf-8," +
      encodeURIComponent("<!doctype html><title>RESTORED-PAGE</title><h1>survives</h1>"),
  });
  check(res.success, `navigate ok`);

  res = await client.send("cookies.set", {
    url: "https://example.com",
    name: "crash_test",
    value: "lives",
  });
  check(res.success, `cookies.set ok (err=${res.error ?? "-"})`);

  // Give the snapshot keeper a moment to capture storageState (5s timer
  // is too slow for this test — force a refresh by issuing an action,
  // then waiting longer than the refresh interval).
  await sleep(5500);

  // 2. Find Chromium PID via daemon.health pid, then pgrep children
  const health = await client.send("daemon.health", {});
  check(health.success, "health ok");
  const daemonPid = health.data?.pid;
  check(typeof daemonPid === "number", `daemon pid known: ${daemonPid}`);

  const chromiumPids = findChromiumPids(daemonPid);
  check(chromiumPids.length > 0, `found Chromium child pid(s): ${chromiumPids.join(", ")}`);

  // 3. Kill Chromium hard — SIGKILL bypasses any cleanup. Supervisor must
  //    detect the crash and trigger a restart.
  for (const pid of chromiumPids) {
    try { process.kill(pid, "SIGKILL"); } catch {}
  }
  console.log(`[info] killed Chromium PIDs, awaiting supervisor restart...`);

  // 4. Wait for restart (backoff is 1s on first attempt + spawn time).
  //    Poll daemon.health until the daemon is again accepting work AND a
  //    fresh navigate succeeds.
  const deadline = Date.now() + 30_000;
  let restored = false;
  let lastSeen = "";
  while (Date.now() < deadline) {
    await sleep(500);
    // Note: the existing client may have errored from the kill; reconnect.
    try {
      const probe = await connectClient(socketPath, 2_000);
      const tabsRes = await probe.send("tabs.list", {});
      const navRes = await probe.send("execution.executeJs", {
        code: "document.title",
      });
      probe.close();
      lastSeen = `tabs=${JSON.stringify(tabsRes.data)} title=${JSON.stringify(navRes.data)}`;
      if (navRes.success && /RESTORED-PAGE/i.test(String(navRes.data ?? ""))) {
        restored = true;
        break;
      }
    } catch (err) {
      lastSeen = `connect err: ${err?.message ?? err}`;
    }
  }
  check(restored, `URL restored after Chromium SIGKILL (last seen: ${lastSeen})`);

  // 5. Cookie should also be back (storageState replay)
  const probe = await connectClient(socketPath);
  const cookieRes = await probe.send("cookies.get", { url: "https://example.com" });
  const hasCookie =
    cookieRes.success &&
    Array.isArray(cookieRes.data) &&
    cookieRes.data.some(
      (c) => c.name === "crash_test" && c.value === "lives"
    );
  check(hasCookie, `cookie restored from snapshot (got ${JSON.stringify(cookieRes.data)})`);
  probe.close();

  try { client.close(); } catch {}

  console.log(`\n[test] ${failed ? "SOME TESTS FAILED" : "ALL TESTS PASSED"}`);
} catch (err) {
  console.error("[FATAL]", err);
  failed = true;
} finally {
  ensureClean();
  rmSync(runtimeDir, { recursive: true, force: true });
}

process.exit(failed ? 1 : 0);
