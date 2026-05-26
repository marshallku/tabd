/**
 * E2E test for CLI secret subcommands (in-memory store, the default).
 *  - secret-put --from-env: never accepts plaintext via argv
 *  - secret-list: ids+labels only, no plaintext
 *  - type-secret: fills a password input through a secret handle
 *  - secret-delete: removes the handle
 *
 * Usage:
 *   node tests/e2e-secrets-cli.mjs
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

const runtimeDir = mkdtempSync(join(tmpdir(), "ai-browser-secrets-cli-"));

const SECRET_VALUE = "hunter2-the-actual-password";

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
      // Defaults to memory store; the secret never leaves the daemon process.
      LOGIN_PW: SECRET_VALUE,
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

const loginPage =
  "data:text/html," +
  encodeURIComponent(
    `<!doctype html><html><body>
      <input id="pw" type="password" />
      <input id="email" type="email" />
      <button id="go">Login</button>
    </body></html>`
  );

try {
  ensureClean();

  // 1. Navigate to a page with a password input
  let r = cli("navigate", loginPage, "--json");
  check(r.status === 0, `navigate to login page (status=${r.status})`);

  // 2. Plaintext is not accepted as argv
  r = cli("secret-put", SECRET_VALUE);
  check(
    r.status !== 0,
    `secret-put rejects bare positional plaintext (status=${r.status})`
  );

  // 3. secret-put rejects empty source list
  r = cli("secret-put", "--label", "login");
  check(
    r.status !== 0,
    `secret-put rejects missing source (status=${r.status})`
  );

  // 4a. --stdin must coexist with --label without swallowing the following flag
  {
    const stdinRes = spawnSync(
      process.execPath,
      [bin, "secret-put", "--stdin", "--label", "via-stdin", "--json"],
      {
        encoding: "utf8",
        env: {
          ...process.env,
          XDG_RUNTIME_DIR: runtimeDir,
          BROWSER_RUNTIME: "playwright",
          BROWSER_HEADLESS: "1",
          BROWSER_EXECUTABLE: chromiumPath,
        },
        input: SECRET_VALUE + "\n",
        timeout: 60_000,
      }
    );
    check(stdinRes.status === 0, `secret-put --stdin --label parses correctly (status=${stdinRes.status})`);
    let stdinStored;
    try { stdinStored = JSON.parse(stdinRes.stdout); } catch {}
    check(stdinStored?.label === "via-stdin", `--stdin handler honored --label`);
    if (stdinStored?.id) {
      cli("secret-delete", stdinStored.id);
    }
  }

  // 4. Store via env, get back a secretId
  r = cli("secret-put", "--from-env", "LOGIN_PW", "--label", "login", "--json");
  check(r.status === 0, `secret-put --from-env succeeded (status=${r.status})`);
  let stored;
  try { stored = JSON.parse(r.stdout); } catch {}
  const secretId = stored?.id;
  check(typeof secretId === "string" && secretId.length > 0, `secret-put returned secretId`);

  // 5. The stdout/stderr must never contain the plaintext value
  const combined = r.stdout + r.stderr;
  check(
    !combined.includes(SECRET_VALUE),
    `secret-put output does not echo plaintext`
  );

  // 6. secret-list shows it with metadata
  r = cli("secret-list", "--json");
  let items;
  try { items = JSON.parse(r.stdout); } catch {}
  check(
    Array.isArray(items) && items.some((entry) => entry.id === secretId),
    `secret-list contains the new handle`
  );
  check(
    !r.stdout.includes(SECRET_VALUE),
    `secret-list does not return plaintext`
  );

  // 7. type-secret fills the password field via the handle
  r = cli("type-secret", "#pw", "--secret-id", secretId, "--json");
  check(r.status === 0, `type-secret completed (status=${r.status})`);

  // 8. The value is in the DOM (not the CLI output)
  r = cli("eval", "document.querySelector('#pw').value", "--json");
  const echoed = r.stdout.trim().replace(/^"|"$/g, "");
  check(echoed === SECRET_VALUE, `password input value matches stored secret`);

  // 9. secret-delete removes the handle
  r = cli("secret-delete", secretId, "--json");
  check(r.status === 0, `secret-delete succeeded (status=${r.status})`);
  r = cli("secret-list", "--json");
  try { items = JSON.parse(r.stdout); } catch { items = []; }
  check(
    !items.some((entry) => entry.id === secretId),
    `secret is gone after delete`
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
