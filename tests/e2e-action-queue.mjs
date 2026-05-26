/**
 * E2E test for the ActionQueue cross-client serialization.
 *  - Two clients connect to the same daemon concurrently.
 *  - Same-tab actions must serialize (B starts only after A returns).
 *  - Different-tab actions may overlap.
 *  - A global structural action (tabs.open) blocks both per-tab queues.
 *
 * Usage:
 *   node tests/e2e-action-queue.mjs
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

const runtimeDir = mkdtempSync(join(tmpdir(), "ai-browser-aq-"));
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
};

// Page that resolves the navigation after ~300ms — enough delay to make
// serialization observable in wall-clock measurement.
const slowPage = (id) =>
  "data:text/html," + encodeURIComponent(
    `<html><body><h1>page-${id}</h1>` +
    `<script>const t=performance.now();while(performance.now()-t<350){}</script>` +
    `</body></html>`
  );

const tt = () => process.hrtime.bigint();
const ms = (start, end) => Number((end - start) / 1_000_000n);

try {
  ensureClean();

  // Spawn daemon via a CLI noop
  let r = cli("navigate", "about:blank");
  check(r.status === 0, `daemon spawned`);

  // Open a second tab so we have two pages to drive concurrently
  r = cli("open-tab", "about:blank", "--json");
  check(r.status === 0, `second tab opened`);

  // Connect two independent daemon clients
  const { socketPath } = getDaemonPaths();
  const clientA = await connectClient(socketPath);
  const clientB = await connectClient(socketPath);
  check(true, "two daemon clients connected");

  // 1. Same-tab navigate must serialize: B must start after A's response
  {
    const start = tt();
    const aDone = clientA
      .send("tabs.navigate", { tabId: 1, url: slowPage("a1") })
      .then(() => ({ kind: "a", at: ms(start, tt()) }));
    // Small lag so A registers in the queue first
    await new Promise((r) => setTimeout(r, 5));
    const bDone = clientB
      .send("tabs.navigate", { tabId: 1, url: slowPage("a2") })
      .then(() => ({ kind: "b", at: ms(start, tt()) }));

    const [a, b] = await Promise.all([aDone, bDone]);
    check(
      b.at >= a.at,
      `same-tab serialization: A returned at ${a.at}ms, B at ${b.at}ms`
    );
    // The total wall-clock should be roughly 2x the per-action time,
    // not 1x. Two ~350ms actions → expect >500ms total.
    check(
      b.at >= 500,
      `cross-client back-to-back navigate >= 500ms (got ${b.at}ms)`
    );
  }

  // 2. Different-tab navigates may overlap: total time should be near 1x
  {
    const start = tt();
    const aDone = clientA
      .send("tabs.navigate", { tabId: 1, url: slowPage("b1") })
      .then(() => ms(start, tt()));
    const bDone = clientB
      .send("tabs.navigate", { tabId: 2, url: slowPage("b2") })
      .then(() => ms(start, tt()));
    const [aMs, bMs] = await Promise.all([aDone, bDone]);
    const slower = Math.max(aMs, bMs);
    check(
      slower < 900,
      `different-tab concurrency: slower of two completed in ${slower}ms (target < 900)`
    );
  }

  // 3. Global structural action gates per-tab work. Enqueue order is
  //    a → g → b with a 10ms gap between each socket write so the daemon
  //    processes them in that order (socket arrival on different clients
  //    is not otherwise ordered). Expected timeline:
  //      a: 0..360ms   (tab-1 navigate, slow)
  //      g: 360..361ms (waits on tab-1 chain, then runs essentially free)
  //      b: 361..720ms (waits on globalLock that g set)
  //    So b's wall-clock duration must be > a's + g's slack, i.e. ~700ms.
  {
    const start = tt();
    const aDone = clientA
      .send("tabs.navigate", { tabId: 1, url: slowPage("c1") })
      .then(() => ms(start, tt()));
    await new Promise((r) => setTimeout(r, 10));
    const gDone = clientB
      .send("tabs.list", {})
      .then(() => ms(start, tt()));
    await new Promise((r) => setTimeout(r, 10));
    const bDone = clientA
      .send("tabs.navigate", { tabId: 2, url: slowPage("c2") })
      .then(() => ms(start, tt()));
    const [a, g, b] = await Promise.all([aDone, gDone, bDone]);
    check(
      b >= 600,
      `global lock: tab-2 navigate took ${b}ms (expect >=600 because it waits on globalLock; a=${a} g=${g})`
    );
  }

  // 4. Stable page identity: queue a slow navigate against tab 2, then
  //    close tab 1 while it's still in flight, then send another action
  //    against "tab 2" — both must land on the same Page that was tab 2
  //    when the first action was enqueued, even though positional indices
  //    have shifted (tab 2 → tab 1 after the close).
  {
    // Reset: ensure 2 tabs again (close any extras)
    const listRes = await clientA.send("tabs.list", {});
    const tabs = listRes.data ?? [];
    for (const t of tabs.slice(2)) {
      await clientA.send("tabs.close", { tabId: t.id ?? tabs.indexOf(t) + 1 });
    }

    // Mark tab 2 with a known title so we can verify identity.
    await clientA.send("tabs.navigate", {
      tabId: 2,
      url: "data:text/html," + encodeURIComponent("<title>TAB-TWO</title><h1>two</h1>"),
    });

    // Now: enqueue a slow navigate on tab 2 (uses pageUuid pinning)
    const slow = clientA.send("tabs.navigate", { tabId: 2, url: slowPage("identity") });
    // Immediately close tab 1 — positional index 2 → 1 after this
    await clientB.send("tabs.close", { tabId: 1 });
    // Wait for the slow navigate to finish
    const slowRes = await slow;
    check(slowRes.success, `pinned tab navigate succeeded after positional shift`);

    // Verify exactly one tab remains AND it is the original tab-2 Page
    // (with the slow navigate applied). If pinning had failed, the slow
    // navigate would have either errored ("Tab not found") or landed on
    // the wrong Page (the one we tried to keep alive).
    const after = await clientA.send("tabs.list", {});
    check(
      after.data?.length === 1,
      `one tab remains (got ${after.data?.length})`
    );
    const text = await clientA.send("dom.getText", { tabId: 1, selector: "h1" });
    check(
      typeof text.data === "string" && /page-identity/.test(text.data),
      `slow navigate landed on the pinned (original tab-2) page (got "${text.data}")`
    );
  }

  clientA.close();
  clientB.close();
  console.log(`\n[test] ${failed ? "SOME TESTS FAILED" : "ALL TESTS PASSED"}`);
} catch (err) {
  console.error("[FATAL]", err);
  failed = true;
} finally {
  ensureClean();
  rmSync(runtimeDir, { recursive: true, force: true });
}

process.exit(failed ? 1 : 0);
