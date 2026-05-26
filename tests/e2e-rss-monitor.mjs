/**
 * E2E test for the RSS monitor.
 *   1. daemon.health exposes chromiumPid + chromiumRssBytes after boot
 *   2. BROWSER_MAX_RSS_MB threshold triggers a graceful restart that
 *      preserves the URL via SnapshotKeeper
 *
 * Usage:
 *   node tests/e2e-rss-monitor.mjs
 */

import { spawnSync } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir, homedir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { connectClient } from "../dist/cli/daemonClient.js";
import { getDaemonPaths } from "../dist/shared/daemonPaths.js";

const here = dirname(fileURLToPath(import.meta.url));
const repo = dirname(here);
const bin = join(repo, "bin", "ai-browser.js");
const chromiumPath = `${homedir()}/.cache/ms-playwright/chromium-1208/chrome-linux64/chrome`;

const runtimeDir = mkdtempSync(join(tmpdir(), "ai-browser-rss-"));

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
    timeout: 60_000,
  });

const ensureClean = () => {
  cli(["daemon", "stop"]);
  spawnSync("sleep", ["0.4"]);
};

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

try {
  ensureClean();

  // ---- Phase 1: monitor reports plausible RSS without a threshold ----
  let r = cli(["navigate", "about:blank"], { BROWSER_RSS_POLL_MS: "500" });
  check(r.status === 0, "daemon spawned with short RSS poll");

  // Wait for at least one poll tick to land
  await sleep(800);
  process.env.XDG_RUNTIME_DIR = runtimeDir;
  const { socketPath } = getDaemonPaths();
  let observer = await connectClient(socketPath);
  let h = await observer.send("daemon.health", {});
  check(
    h.success && typeof h.data?.driver?.chromiumPid === "number",
    `health.driver.chromiumPid present (${h.data?.driver?.chromiumPid})`
  );
  const rss1 = h.data?.driver?.chromiumRssBytes;
  check(
    typeof rss1 === "number" && rss1 > 1_000_000,
    `chromiumRssBytes >1MB (got ${rss1})`
  );
  check(
    typeof h.data?.driver?.rssCheckedAt === "number" &&
      h.data?.driver?.rssCheckedAt > 0,
    `rssCheckedAt populated`
  );
  observer.close();
  ensureClean();

  // ---- Phase 2: low threshold triggers graceful restart with snapshot ----
  // Set BROWSER_MAX_RSS_MB very low so the first poll detects an overshoot.
  // Daemon starts → navigates → RSS poll → threshold exceeded → restart.
  // After restart, the URL list should still be example.com (snapshot replay).
  r = cli(["navigate", "https://example.com"], {
    BROWSER_RSS_POLL_MS: "500",
    BROWSER_MAX_RSS_MB: "10", // unrealistically low — first poll will exceed
  });
  check(r.status === 0, "daemon spawned with low RSS cap");

  // Give the monitor time to: poll → see overshoot → schedule restart →
  // backoff (1s) → relaunch → restore.
  await sleep(8_000);

  observer = await connectClient(socketPath);
  h = await observer.send("daemon.health", {});
  // restartAttempt should be > 0 OR restarting should be true; either way
  // the supervisor has been triggered.
  const restartSignaled =
    (h.data?.driver?.restartAttempt ?? 0) > 0 ||
    h.data?.driver?.restarting === true;
  check(
    restartSignaled,
    `RSS threshold triggered supervisor (restartAttempt=${h.data?.driver?.restartAttempt}, restarting=${h.data?.driver?.restarting})`
  );

  // URL should be preserved via SnapshotKeeper, despite the restart loop.
  // Wait a bit more if a restart is still in progress.
  const deadline = Date.now() + 15_000;
  let urlRestored = false;
  while (Date.now() < deadline) {
    await sleep(500);
    try {
      const probe = await connectClient(socketPath, 2_000);
      const tabsRes = await probe.send("tabs.list", {});
      probe.close();
      const tab = tabsRes.data?.[0];
      if (tab?.url && /example\.com/.test(tab.url)) {
        urlRestored = true;
        break;
      }
    } catch {
      // recovery in progress
    }
  }
  check(urlRestored, "URL preserved across RSS-triggered restart");

  observer.close();

  console.log(`\n[test] ${failed ? "SOME TESTS FAILED" : "ALL TESTS PASSED"}`);
} catch (err) {
  console.error("[FATAL]", err);
  failed = true;
} finally {
  ensureClean();
  rmSync(runtimeDir, { recursive: true, force: true });
}

process.exit(failed ? 1 : 0);
