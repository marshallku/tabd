/**
 * E2E test for wait_for_url via the CLI.
 *   - Navigate to a data: page with a delayed redirect
 *   - wait-url with glob pattern resolves once the URL matches
 *
 * Usage:
 *   node tests/e2e-wait-url.mjs
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

const runtimeDir = mkdtempSync(join(tmpdir(), "ai-browser-wait-url-"));

let failed = false;
const check = (cond, label) => {
  if (cond) console.log(`[PASS] ${label}`);
  else { console.error(`[FAIL] ${label}`); failed = true; }
};

const cli = (...args) =>
  spawnSync(process.execPath, [bin, ...args], {
    encoding: "utf8",
    env: {
      ...process.env,
      XDG_RUNTIME_DIR: runtimeDir,
      BROWSER_RUNTIME: "playwright",
      BROWSER_HEADLESS: "1",
      BROWSER_EXECUTABLE: chromiumPath,
    },
    timeout: 60_000,
  });

const ensureClean = () => {
  cli("daemon", "stop");
  const sock = join(runtimeDir, "ai-browser", "daemon.sock");
  const pid = join(runtimeDir, "ai-browser", "daemon.pid");
  if (existsSync(sock)) rmSync(sock, { force: true });
  if (existsSync(pid)) rmSync(pid, { force: true });
};

// A self-redirecting page: schedules location.replace after 400ms.
const redirectingPage =
  "data:text/html," +
  encodeURIComponent(
    `<!doctype html><html><body>
      <h1>start</h1>
      <script>setTimeout(() => location.replace("https://example.com/"), 400);</script>
    </body></html>`
  );

try {
  ensureClean();

  let r = cli("navigate", redirectingPage, "--json");
  check(r.status === 0, `navigate to redirecting page (status=${r.status})`);

  // 1. glob match against example.com root
  r = cli("wait-url", "https://example.com/*", "--pattern-type", "glob", "--timeout", "8000", "--json");
  check(
    r.status === 0 && /example\.com/.test(r.stdout),
    `wait-url glob matched (status=${r.status}, out=${r.stdout.trim()})`
  );

  // 2. After matching the URL is already example.com — exact match against same URL
  r = cli("eval", "location.href", "--json");
  const finalUrl = r.stdout.trim().replace(/^"|"$/g, "");
  check(/example\.com/.test(finalUrl), `final URL is example.com (got ${finalUrl})`);

  r = cli("wait-url", finalUrl, "--pattern-type", "exact", "--timeout", "3000", "--json");
  check(r.status === 0, `wait-url exact matched current URL (status=${r.status})`);

  // 3. Regex match
  r = cli("wait-url", "example\\.com\\/?$", "--pattern-type", "regex", "--timeout", "3000", "--json");
  check(r.status === 0, `wait-url regex matched (status=${r.status})`);

  // 4. Times out cleanly for impossible pattern
  r = cli("wait-url", "https://this-host-never-loads.invalid/*", "--pattern-type", "glob", "--timeout", "1500");
  check(r.status !== 0, `wait-url times out for impossible pattern (status=${r.status})`);
  check(
    /Timed out|Timeout/i.test(r.stderr + r.stdout),
    `timeout produces a useful error message`
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
