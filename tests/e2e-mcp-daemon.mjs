/**
 * E2E MCP daemon-attach test — verifies the bridge in `client` role forwards
 * to a running daemon, so an "MCP-style" client and a CLI command see the
 * same browser state (shared Chromium between AI and human).
 *
 * Usage:
 *   node tests/e2e-mcp-daemon.mjs
 */

import { spawnSync } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir, homedir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { initBridge, send, shutdownBridge } from "../dist/server/bridge.js";

const here = dirname(fileURLToPath(import.meta.url));
const repo = dirname(here);
const bin = join(repo, "bin", "ai-browser.js");
const chromiumPath = `${homedir()}/.cache/ms-playwright/chromium-1208/chrome-linux64/chrome`;

const runtimeDir = mkdtempSync(join(tmpdir(), "ai-browser-mcp-daemon-"));
process.env.XDG_RUNTIME_DIR = runtimeDir;
process.env.BROWSER_RUNTIME = "playwright";
process.env.BROWSER_HEADLESS = "1";
process.env.BROWSER_EXECUTABLE = chromiumPath;

let failed = false;
const check = (cond, label) => {
  if (cond) console.log(`[PASS] ${label}`);
  else { console.error(`[FAIL] ${label}`); failed = true; }
};
const unwrap = (res, action) => {
  if (!res.success) throw new Error(`${action} failed: ${res.error}`);
  return res.data;
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

try {
  ensureClean();

  // 1. Initialize bridge as client — should auto-spawn the daemon and attach.
  await initBridge({ role: { kind: "client" } });
  check(true, "bridge.initBridge({client}) resolved (daemon attached)");

  // 2. send() now forwards to the daemon. Navigate to example.com.
  let res = await send("tabs.navigate", { url: "https://example.com" });
  check(res.success, `bridge.send navigate succeeded (success=${res.success})`);

  // 3. Cross-check via the CLI — same daemon, so same browser state.
  const txt = cli("get-text", "--selector", "h1");
  check(
    /Example Domain/i.test(txt.stdout),
    "CLI sees the page that the bridge navigated to (shared state)"
  );

  // 4. Eval through the bridge and verify the same value through the CLI.
  res = await send("execution.executeJs", { code: "document.title" });
  check(
    res.success && res.data === "Example Domain",
    `bridge eval returned title (${JSON.stringify(res.data)})`
  );

  // 4b. Secret store survives the MCP↔daemon split: put through the bridge,
  //     then type_secret over the bridge. If put landed in a separate process
  //     store than typeSecret reads, this would fail with "Secret not found".
  unwrap(
    await send("tabs.navigate", {
      url: "data:text/html;charset=utf-8," +
        encodeURIComponent("<input id=pw type=password>"),
    }),
    "navigate to password form"
  );
  const putRes = await send("secrets.put", { value: "hunter2", label: "test" });
  check(putRes.success, `secrets.put via bridge ok (success=${putRes.success})`);
  const secretId = putRes.data?.id;
  res = await send("interaction.typeSecret", {
    selector: "#pw",
    secretId,
  });
  check(
    res.success,
    `interaction.typeSecret resolves the bridge-stored secret (err=${res.error ?? "-"})`
  );
  res = await send("execution.executeJs", {
    code: "document.getElementById('pw').value",
  });
  check(
    res.success && res.data === "hunter2",
    `secret value reached the input field`
  );
  await send("secrets.delete", { id: secretId });

  // 5. Daemon restart while bridge is attached — bridge should auto-reconnect
  //    on the next send() rather than hang or error out.
  cli("daemon", "restart");
  // Note: restart auto-spawns a fresh daemon. The previous daemonClient inside
  //       the bridge is now talking to a closed socket; bridge.send must detect
  //       and reconnect transparently.
  res = await send("tabs.navigate", { url: "https://example.com" });
  check(
    res.success,
    `bridge auto-reconnected after daemon restart (success=${res.success}, err=${res.error ?? "-"})`
  );

  // 6. Detach + reattach as client: shutdown drops the daemon connection,
  //    re-init must reconnect (auto-spawning if needed) without errors.
  await shutdownBridge();
  await initBridge({ role: { kind: "client" } });
  res = await send("tabs.navigate", { url: "https://example.com" });
  check(res.success, "client role reconnects after shutdownBridge");
  await shutdownBridge();

  console.log(`\n[test] ${failed ? "SOME TESTS FAILED" : "ALL TESTS PASSED"}`);
} catch (err) {
  console.error("[FATAL]", err);
  failed = true;
} finally {
  try { await shutdownBridge(); } catch {}
  ensureClean();
  rmSync(runtimeDir, { recursive: true, force: true });
}

process.exit(failed ? 1 : 0);
